//! The reconciliation engine shared by `controller` (polling loop) and `sync`
//! (single pass). Fetches Git, parses manifests, applies via server-side apply,
//! prunes removed resources, detects drift, and persists state.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kube::client::Client;
use rand::Rng;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::git_sync;
use crate::hooks;
use crate::kube_util;
use crate::lock;
use crate::manifest::{self, RawManifest};
use crate::metrics::Metrics;
use crate::{drift, prune, state};

/// Drives a single repository-sync target.
pub struct Reconciler {
    pub client: Client,
    pub cfg: Config,
    pub metrics: Arc<Metrics>,
    /// Cooperative shutdown flag: the loop checks it between passes (and
    /// short-circuits the inter-pass sleep) and exits cleanly when it is set.
    pub stop: Arc<AtomicBool>,
}

impl Reconciler {
    /// One reconciliation pass.
    pub async fn run_once(&self) -> Result<()> {
        self.metrics.sync_total.add(1, &[]);
        match self.reconcile().await {
            Ok(()) => {
                self.metrics.set_last_success_epoch(now_epoch() as i64);
                Ok(())
            }
            Err(e) => {
                self.metrics.sync_errors.add(1, &[]);
                if let Err(state_err) = self.record_error(&e).await {
                    tracing::warn!(error = %state_err, "failed to record error in state");
                }
                Err(e)
            }
        }
    }

    async fn record_error(&self, e: &Error) -> Result<()> {
        let mut s = state::read(&self.client, &self.cfg)
            .await?
            .unwrap_or_default();
        s.last_error = Some(e.to_string());
        s.sync_count = s.sync_count.saturating_add(1);
        state::write(&self.client, &self.cfg, &s).await
    }

    /// Acquire the reconcile Lease, run one pass, then release the lease — even
    /// on error. A busy lease (another pass in flight) skips with an INFO log
    /// rather than erroring, so `sync_errors` is not incremented and the
    /// controller does not back off. Serializing passes this way is what makes
    /// the state ConfigMap safe without CAS (see `lock.rs`).
    async fn reconcile(&self) -> Result<()> {
        let holder = lock::holder_identity(&lock::holder_base(), std::process::id());
        let guard = match lock::acquire(&self.client, &self.cfg, &holder, &self.stop).await {
            Ok(g) => g,
            Err(lock::AcquireError::Busy) => {
                tracing::info!("reconcile lock busy; skipping this pass");
                return Ok(());
            }
            Err(lock::AcquireError::Kube(e)) => return Err(Error::Kube(e)),
        };
        let result = self.reconcile_inner(&guard).await;
        // Release on both success and failure so the next pass need not wait
        // for the stale-reclaim timeout. Best-effort: a failed release just
        // means the lease lingers until lock_lease_duration reclaims it.
        if let Err(e) = guard.release().await {
            tracing::warn!(error = %e, "failed to release reconcile lease");
        }
        result
    }

    async fn reconcile_inner(&self, guard: &lock::LeaseGuard) -> Result<()> {
        let prev = state::read(&self.client, &self.cfg).await?;
        let prev_sha = prev
            .as_ref()
            .and_then(|s| s.last_sha.clone())
            .filter(|s| !s.is_empty());
        let prev_applied = prev.as_ref().map(|s| s.applied.clone()).unwrap_or_default();

        let sync = git_sync::sync(&self.cfg, prev_sha.as_deref()).await?;
        // The git fetch may have taken a while; refresh the lease so a long
        // pass is not reclaimed as stale mid-flight.
        self.touch_lease(guard).await;
        let work_dir = self.cfg.effective_work_dir();
        let work = Path::new(&work_dir);
        let roots = match manifest::expand_roots(work, &self.cfg.path) {
            Ok(r) => r,
            Err(Error::Config(_)) if !prev_applied.is_empty() => {
                // The sync path matches no directories, but Lean CD previously
                // managed resources: treat an emptied repo/path as a full
                // teardown rather than the usual "would prune everything"
                // fail-fast. pre-delete/post-delete hooks (if any remain in Git)
                // wrap the prune.
                tracing::info!("sync path matches no directories; treating as full teardown");
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        tracing::debug!(
            roots = roots.len(),
            patterns = ?self.cfg.path,
            "expanded sync path patterns into directories"
        );
        let mut manifests = manifest::parse_paths(&roots).await?;
        for m in &mut manifests {
            manifest::inject_managed_label(
                m,
                &self.cfg.managed_label_key,
                &self.cfg.managed_label_value,
            );
        }
        let classified = hooks::classify(manifests);
        // Only non-hook ("main") resources are tracked in the applied set; hooks
        // are managed by the hook engine and excluded from prune.
        let current_keys = prune::ResourceKey::keys_of(&classified.main);

        let do_full = should_full_apply(prev.is_some(), sync.changed);
        // A full teardown: every main resource has left Git while Lean CD still
        // has an applied set. pre-delete/post-delete hooks wrap the prune.
        let teardown = classified.main.is_empty() && !prev_applied.is_empty();

        let mut drifts: Vec<drift::DriftItem> = Vec::new();
        let mut post_error: Option<String> = None;
        let mut pruned = 0usize;

        if teardown {
            // pre-delete hooks; abort (skip the prune) on failure.
            let pre = self
                .hook_phase(
                    guard,
                    &classified.pre_delete,
                    hooks::HookPhase::PreDelete,
                    "predelete",
                )
                .await;
            if let Some((_, reason)) = pre.failures.first() {
                return Err(Error::Hook(format!("pre-delete hook failed: {reason}")));
            }
            pruned = prune::prune(&self.client, &prev_applied, &current_keys, &self.cfg)
                .await?
                .len();
            let post = self
                .hook_phase(
                    guard,
                    &classified.post_delete,
                    hooks::HookPhase::PostDelete,
                    "postdelete",
                )
                .await;
            if let Some((_, reason)) = post.failures.first() {
                post_error = Some(format!("post-delete hook failed: {reason}"));
            }
        } else if do_full {
            // PreSync -> apply main -> PostSync.
            let pre = self
                .hook_phase(guard, &classified.pre, hooks::HookPhase::PreSync, "presync")
                .await;
            if let Some((_, reason)) = pre.failures.first() {
                return Err(Error::Hook(format!("pre-sync hook failed: {reason}")));
            }
            self.apply_all(&classified.main).await?;
            let post = self
                .hook_phase(
                    guard,
                    &classified.post,
                    hooks::HookPhase::PostSync,
                    "postsync",
                )
                .await;
            if let Some((_, reason)) = post.failures.first() {
                post_error = Some(format!("post-sync hook failed: {reason}"));
            }
        } else {
            drifts = drift::detect(&self.client, &classified.main, &self.cfg).await?;
            for d in &drifts {
                tracing::warn!(key = ?d.key, reason = %d.reason, "drift detected");
            }
            if !drifts.is_empty() {
                tracing::info!(
                    drift = drifts.len(),
                    "drift detected; re-applying managed resources"
                );
                // Re-apply on drift so a field claimed by another SSA field
                // manager is reclaimed back to Git (all applies force-conflict SSA).
                let pre = self
                    .hook_phase(guard, &classified.pre, hooks::HookPhase::PreSync, "presync")
                    .await;
                if let Some((_, reason)) = pre.failures.first() {
                    return Err(Error::Hook(format!("pre-sync hook failed: {reason}")));
                }
                self.apply_all(&classified.main).await?;
                let post = self
                    .hook_phase(
                        guard,
                        &classified.post,
                        hooks::HookPhase::PostSync,
                        "postsync",
                    )
                    .await;
                if let Some((_, reason)) = post.failures.first() {
                    post_error = Some(format!("post-sync hook failed: {reason}"));
                }
            }
        }
        let drift_count = drifts.len();

        if !teardown {
            pruned = prune::prune(&self.client, &prev_applied, &current_keys, &self.cfg)
                .await?
                .len();
        }
        self.touch_lease(guard).await;

        let mut new_state = prev.clone().unwrap_or_default();
        new_state.last_sha = Some(sync.sha.clone());
        new_state.last_sync_epoch = Some(now_epoch());
        new_state.sync_count = new_state.sync_count.saturating_add(1);
        new_state.last_error = post_error;
        new_state.drift_count = drift_count;
        new_state.managed_count = current_keys.len();
        new_state.applied = current_keys.clone();
        self.touch_lease(guard).await;
        state::write(&self.client, &self.cfg, &new_state).await?;

        self.metrics
            .set_managed_resources(current_keys.len() as i64);
        // Per-GVK drift counts; reset first so resolved drifts clear next pass.
        self.metrics.reset_drift();
        let mut drift_by_gvk: HashMap<(String, String, String), i64> = HashMap::new();
        for d in &drifts {
            *drift_by_gvk
                .entry((
                    d.key.group.clone(),
                    d.key.version.clone(),
                    d.key.kind.clone(),
                ))
                .or_default() += 1;
        }
        for ((group, version, kind), n) in drift_by_gvk {
            self.metrics.set_drift(&group, &version, &kind, n);
        }

        tracing::info!(
            sha = %sync.sha, full = do_full, teardown,
            managed = current_keys.len(), pruned, drift = drift_count,
            "reconciliation complete"
        );
        Ok(())
    }

    /// Best-effort lease renew: refresh `renewTime` so a long pass is not
    /// reclaimed as stale. Never aborts the pass on failure or lease loss — the
    /// PID-scoped `work_dir` still prevents git corruption, and the next pass
    /// re-converges to Git HEAD.
    async fn touch_lease(&self, guard: &lock::LeaseGuard) {
        match lock::renew(&self.client, &self.cfg, guard).await {
            Ok(true) => {}
            Ok(false) => tracing::warn!("reconcile lease lost mid-pass; continuing best-effort"),
            Err(e) => tracing::warn!(error = %e, "lease renew failed; continuing best-effort"),
        }
    }

    /// Apply every manifest, sharing one API-discovery lookup per resource kind.
    /// Individual apply failures are logged but do not abort the pass.
    async fn apply_all(&self, manifests: &[RawManifest]) -> Result<()> {
        let mut cache = kube_util::DiscoveryCache::new();
        let mut applied = 0usize;
        let mut failed = 0usize;
        for m in manifests {
            let gk = m.gvk();
            let (ar, caps) = match cache
                .get_or_resolve(&self.client, &m.group, &m.version, &m.kind)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, ?gk, "discovery failed; skipping resource");
                    failed += 1;
                    continue;
                }
            };
            match kube_util::apply(
                &self.client,
                &ar,
                &caps.scope,
                &self.cfg.namespace,
                &m.data,
                &self.cfg.field_manager,
            )
            .await
            {
                Ok(_) => applied += 1,
                Err(e) => {
                    tracing::warn!(error = %e, name = %m.name, kind = %m.kind, "apply failed");
                    failed += 1;
                }
            }
        }
        if failed > 0 {
            tracing::warn!(applied, failed, "apply pass completed with failures");
        } else {
            tracing::debug!(applied, "apply pass complete");
        }
        Ok(())
    }

    /// Run one hook phase and record its outcome metrics, returning the outcome.
    async fn hook_phase(
        &self,
        guard: &lock::LeaseGuard,
        hooks_list: &[hooks::HookInfo],
        phase: hooks::HookPhase,
        label: &'static str,
    ) -> hooks::PhaseOutcome {
        let outcome =
            hooks::run_phase(&self.client, &self.cfg, hooks_list, phase, Some(guard)).await;
        let failed = outcome.failures.len() as u64;
        let succeeded = outcome.attempted.saturating_sub(outcome.failures.len()) as u64;
        self.metrics.record_hooks(label, succeeded, failed);
        outcome
    }

    /// Run reconciliation forever on the configured poll interval, backing off
    /// on consecutive failures and stopping cooperatively when [`Self::stop`]
    /// is set. An in-flight pass always finishes before the loop re-checks the
    /// flag, so callers can `await` the handle for a graceful shutdown and fall
    /// back to `abort()` after a timeout.
    pub async fn run_loop(&self) -> Result<()> {
        let mut consecutive_failures: u32 = 0;
        loop {
            if self.stop.load(Ordering::Acquire) {
                tracing::info!("shutdown requested; exiting reconciliation loop");
                break;
            }

            let result = self.run_once().await;
            match result {
                Ok(()) => consecutive_failures = 0,
                Err(e) => {
                    tracing::error!(error = %e, "reconciliation failed");
                    consecutive_failures = consecutive_failures.saturating_add(1);
                }
            }

            let delay = next_delay(
                consecutive_failures,
                self.cfg.backoff_base,
                self.cfg.backoff_max,
                self.cfg.poll_interval,
            );
            // Jitter the backoff path so repeated failures across instances do
            // not synchronize; the poll interval after a success is left exact.
            let delay = if consecutive_failures > 0 {
                jittered(delay)
            } else {
                delay
            };
            if consecutive_failures > 0 {
                tracing::warn!(
                    consecutive_failures,
                    backoff_secs = delay.as_secs(),
                    "backing off before next reconciliation"
                );
            }

            // Sleep, but wake immediately if shutdown is requested so a signal
            // during the idle interval does not wait for the full delay.
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = self.watch_stop() => {
                    tracing::info!("shutdown requested during sleep; exiting");
                    break;
                }
            }
        }
        Ok(())
    }

    /// Resolve once [`Self::stop`] is set. Used to short-circuit the inter-pass
    /// sleep on shutdown; polled on a short interval because the flag is set at
    /// most once (at termination), so no notification primitive is needed.
    async fn watch_stop(&self) {
        while !self.stop.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Whether a reconciliation should fully re-apply every manifest (rather than
/// only drift-check). Full apply runs on the first run (no prior state) or when
/// the Git HEAD moved. Pure: no API calls.
fn should_full_apply(has_prev: bool, changed: bool) -> bool {
    !has_prev || changed
}

/// Delay before the next reconciliation attempt after `consecutive_failures`
/// failures, using exponential backoff with a cap: failures=1 -> `base`, =2 ->
/// `2*base`, =3 -> `4*base`, ... capped at `cap`. `consecutive_failures == 0`
/// yields a zero delay (a success resets the backoff). Pure: no I/O.
fn backoff_delay(base: Duration, cap: Duration, consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    // failures=1 -> base, =2 -> 2*base, =3 -> 4*base, ..., capped at `cap`.
    // The exponent is clamped to avoid shift overflow; `checked_mul` falls
    // back to the cap on overflow.
    let exp = consecutive_failures.saturating_sub(1).min(31);
    let factor: u32 = 1u32 << exp;
    base.checked_mul(factor).unwrap_or(cap).min(cap)
}

/// Delay before the next reconciliation pass: `poll_interval` after a success
/// (failures == 0), otherwise the exponential [`backoff_delay`].
fn next_delay(
    consecutive_failures: u32,
    base: Duration,
    cap: Duration,
    poll: Duration,
) -> Duration {
    if consecutive_failures == 0 {
        poll
    } else {
        backoff_delay(base, cap, consecutive_failures)
    }
}

/// A pseudo-random factor in `[0.75, 1.0)` used to jitter the backoff delay so
/// repeated failures across instances do not synchronize. Sampled with `rand`
/// (not deterministic in a seed); see the test `jitter_factor_in_range`.
fn jitter_factor() -> f64 {
    rand::thread_rng().gen_range(0.75..1.0)
}

/// Scale `delay` by `jitter_factor()`, i.e. into `[0.75, 1.0)` of it.
fn jittered(delay: Duration) -> Duration {
    delay.mul_f64(jitter_factor())
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `should_full_apply` is true when there is no prior state (first run) or
    /// the Git HEAD moved. Otherwise (steady state) we drift-check instead of
    /// re-applying. Verified exhaustively.
    #[test]
    fn should_full_apply_truth_table() {
        for &(has_prev, changed) in &[(false, false), (false, true), (true, false), (true, true)] {
            assert_eq!(
                should_full_apply(has_prev, changed),
                !has_prev || changed,
                "has_prev={has_prev} changed={changed}"
            );
        }
    }

    #[test]
    fn steady_state_does_not_full_apply() {
        // The only combination that skips full apply: has prior state, no Git
        // change -> the drift-check path.
        assert!(!should_full_apply(true, false));
    }

    #[test]
    fn backoff_delay_zero_after_success() {
        assert_eq!(
            backoff_delay(Duration::from_secs(5), Duration::from_secs(600), 0),
            Duration::ZERO
        );
    }

    #[test]
    fn backoff_delay_exponential() {
        let base = Duration::from_secs(5);
        let cap = Duration::from_secs(600);
        assert_eq!(backoff_delay(base, cap, 1), Duration::from_secs(5));
        assert_eq!(backoff_delay(base, cap, 2), Duration::from_secs(10));
        assert_eq!(backoff_delay(base, cap, 3), Duration::from_secs(20));
        assert_eq!(backoff_delay(base, cap, 4), Duration::from_secs(40));
    }

    #[test]
    fn backoff_delay_capped() {
        let base = Duration::from_secs(5);
        let cap = Duration::from_secs(600);
        // 2^7 * 5s = 640s > 600s cap.
        assert_eq!(backoff_delay(base, cap, 8), cap);
        // A large failure count stays pinned at the cap.
        assert_eq!(backoff_delay(base, cap, 100), cap);
    }

    #[test]
    fn next_delay_uses_poll_after_success() {
        let poll = Duration::from_secs(60);
        assert_eq!(
            next_delay(0, Duration::from_secs(5), Duration::from_secs(600), poll),
            poll
        );
    }

    #[test]
    fn next_delay_uses_backoff_after_failure() {
        let poll = Duration::from_secs(60);
        // 1 failure -> backoff base (5s), not poll (60s).
        assert_eq!(
            next_delay(1, Duration::from_secs(5), Duration::from_secs(600), poll),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn jitter_factor_in_range() {
        for _ in 0..1024 {
            let f = jitter_factor();
            assert!((0.75..1.0).contains(&f), "{f} out of [0.75, 1.0)");
        }
    }

    #[test]
    fn jittered_scales_into_range() {
        let base = Duration::from_secs(100);
        for _ in 0..1024 {
            let d = jittered(base);
            // [75s, 100s): strictly below base (always jittered), at least 0.75x.
            assert!(d >= Duration::from_secs(75), "{d:?} < 75s");
            assert!(d < base, "{d:?} >= base (no jitter)");
        }
    }

    #[test]
    fn jittered_zero_is_zero() {
        assert_eq!(jittered(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn backoff_delay_unchanged_no_jitter() {
        // Regression guard: backoff_delay itself stays deterministic (jitter is
        // applied separately in run_loop), so the curve guarantees still hold.
        let base = Duration::from_secs(5);
        let cap = Duration::from_secs(600);
        assert_eq!(backoff_delay(base, cap, 1), Duration::from_secs(5));
        assert_eq!(backoff_delay(base, cap, 2), Duration::from_secs(10));
        assert_eq!(backoff_delay(base, cap, 3), Duration::from_secs(20));
    }
}
