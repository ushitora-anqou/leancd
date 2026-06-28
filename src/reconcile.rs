//! The reconciliation engine shared by `controller` (polling loop) and `sync`
//! (single pass). Fetches Git, parses manifests, applies via server-side apply,
//! prunes removed resources, detects drift, and persists state.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kube::client::Client;
use kube::core::DynamicObject;
use rand::RngExt;
use tokio::sync::Notify;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::git_sync;
use crate::hooks;
use crate::kube_util;
use crate::lock;
use crate::manifest::{self, RawManifest};
use crate::metrics::Metrics;
use crate::watch;
use crate::{
    drift,
    prune::{self, ResourceKey},
    resource_health, state,
};

/// Drives a single repository-sync target.
pub struct Reconciler {
    pub client: Client,
    pub cfg: Config,
    pub metrics: Arc<Metrics>,
    /// Cooperative shutdown flag: the loop checks it between passes (and
    /// short-circuits the inter-pass sleep) and exits cleanly when it is set.
    pub stop: Arc<AtomicBool>,
    /// Last parsed managed-GVK set, stashed by `reconcile_inner` for `run_loop`
    /// to rebuild the watch streams between passes. Held only briefly between
    /// passes (never across an `.await`), so the lock never contends with the
    /// watch drivers. `sync` (one-shot) never reads this.
    pub last_gvks: Arc<Mutex<HashSet<watch::GvkKey>>>,
    /// Cache-mode object stores, snapshotted from the `WatchSet` after each
    /// `rebuild` so `reconcile_inner` can drift-check from the cache instead of
    /// `List`ing. Empty (and unused) in `Trigger`/`Off` modes. Each
    /// `LightweightStore` is behind an `Arc<Mutex>` shared with the watch driver.
    pub cache_stores: Arc<Mutex<HashMap<watch::GvkKey, Arc<Mutex<watch::LightweightStore>>>>>,
    /// When set, force the git checkout to this target instead of the tracked
    /// branch HEAD. Used by `leancd rollback`; `None` (the controller and
    /// `sync`) tracks the branch as usual.
    pub force_target: Option<git_sync::CheckoutTarget>,
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

    /// Validate the desired state via a server-side dry-run apply (no mutation,
    /// no state, no metrics, no hooks/prune). Goes through `reconcile()` (so the
    /// reconcile Lease is acquired as in `run_once`, keeping the git checkout
    /// consistent), then short-circuits in `reconcile_inner` after the dry-run
    /// apply. Used by `sync --dry-run`.
    pub async fn dry_run(&self) -> Result<()> {
        self.reconcile().await
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

        let target = self
            .force_target
            .clone()
            .unwrap_or(git_sync::CheckoutTarget::Branch);
        // A forced target (rollback) always re-applies: ignore the persisted
        // last_sha so checkout reports a change and the pass is a full apply.
        let prev_sha_ref = if self.force_target.is_some() {
            None
        } else {
            prev_sha.as_deref()
        };
        let sync = git_sync::checkout(&self.cfg, prev_sha_ref, &target).await?;
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
        let manifests = manifest::parse_paths(&roots).await?;
        let classified = hooks::classify(manifests);
        // Only non-hook ("main") resources are tracked in the applied set; hooks
        // are managed by the hook engine and excluded from prune.
        let current_keys = prune::ResourceKey::keys_of(&classified.main);
        // Extract the distinct managed GVKs before `apply_all` consumes
        // `classified.main` (moved into apply), so `run_loop` can rebuild the
        // watch streams between passes.
        let managed_gvks: HashSet<watch::GvkKey> =
            classified.main.iter().map(|m| m.gvk()).collect();

        let do_full = should_full_apply(prev.is_some(), sync.changed);
        // A full teardown: every main resource has left Git while Lean CD still
        // has an applied set. pre-delete/post-delete hooks wrap the prune.
        let teardown = classified.main.is_empty() && !prev_applied.is_empty();

        if self.cfg.dry_run {
            // Validate the desired state via a server-side dry-run apply only —
            // no hooks, no prune, no state/metrics update. Early return so a
            // dry run never mutates the cluster or advances sync state.
            self.apply_all(&classified.main, true).await?;
            tracing::info!(
                dry_run = true,
                managed = current_keys.len(),
                "dry-run validation complete; no changes applied"
            );
            return Ok(());
        }

        let mut drifts: Vec<drift::DriftItem> = Vec::new();
        let mut apply_failures: Vec<ResourceKey> = Vec::new();
        let mut post_error: Option<String> = None;
        let mut pruned = 0usize;
        // Live objects for the health assessment; populated on every pass that
        // evaluates health (reused from the drift List, or a dedicated collect
        // on a full-apply pass).
        let mut health_live: Vec<DynamicObject> = Vec::new();

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
            let pruned_keys =
                prune::prune(&self.client, &prev_applied, &current_keys, &self.cfg).await?;
            pruned = pruned_keys.len();
            for key in &pruned_keys {
                tracing::info!(
                    target: "leancd.audit",
                    action = "prune",
                    key = ?key,
                    result = "deleted",
                );
            }
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
            apply_failures = self.apply_all(&classified.main, false).await?;
            if self.cfg.health_enabled {
                health_live = drift::collect_live(&self.client, &classified.main, &self.cfg)
                    .await
                    .unwrap_or_default();
            }
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
            // Cache mode drift-checks against the watch-backed `LightweightStore`
            // (SmallTier from the cache; LargeTier via a per-GVK `List` fallback);
            // Trigger/Off modes List live objects as before.
            let (d, l) = if self.cfg.watch_mode == watch::WatchMode::Cache {
                let stores = self
                    .cache_stores
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or_default();
                drift::detect_from_lw(&self.client, &classified.main, &stores, &self.cfg).await?
            } else {
                drift::detect(&self.client, &classified.main, &self.cfg).await?
            };
            drifts = d;
            health_live = l;
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
                apply_failures = self.apply_all(&classified.main, false).await?;
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
            let pruned_keys =
                prune::prune(&self.client, &prev_applied, &current_keys, &self.cfg).await?;
            pruned = pruned_keys.len();
            for key in &pruned_keys {
                tracing::info!(
                    target: "leancd.audit",
                    action = "prune",
                    key = ?key,
                    result = "deleted",
                );
            }
        }
        self.touch_lease(guard).await;

        let health_summary = if self.cfg.health_enabled {
            // cache watch-mode with no driver running (e.g. `leancd sync`'s
            // one-shot pass) yields an empty store snapshot, so health_live is
            // empty and every resource would read as Missing. Fall back to a
            // List so health is still assessed. On the controller path the drift
            // List / store already filled health_live, so this is a no-op there.
            let live = if health_live.is_empty() {
                drift::collect_live(&self.client, &classified.main, &self.cfg)
                    .await
                    .unwrap_or_default()
            } else {
                health_live
            };
            resource_health::assess(&classified.main, &live)
        } else {
            state::HealthSummary::default()
        };
        let mut new_state = prev.clone().unwrap_or_default();
        new_state.last_sha = Some(sync.sha.clone());
        new_state.last_sync_epoch = Some(now_epoch());
        new_state.sync_count = new_state.sync_count.saturating_add(1);
        new_state.last_error = post_error;
        new_state.drift_count = drift_count;
        new_state.managed_count = current_keys.len();
        new_state.applied = current_keys.clone();
        new_state.apply_failures = apply_failures.clone();
        new_state.health = health_summary.clone();
        self.touch_lease(guard).await;
        state::write(&self.client, &self.cfg, &new_state).await?;

        self.metrics
            .set_managed_resources(current_keys.len() as i64);
        if !apply_failures.is_empty() {
            self.metrics
                .apply_failures
                .add(apply_failures.len() as u64, &[]);
        }
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

        // Resource-health counts (reset first so a cleared status disappears).
        self.metrics.reset_health();
        self.metrics
            .set_health("Healthy", health_summary.healthy as i64);
        self.metrics
            .set_health("Progressing", health_summary.progressing as i64);
        self.metrics
            .set_health("Degraded", health_summary.degraded as i64);
        self.metrics
            .set_health("Suspended", health_summary.suspended as i64);
        self.metrics
            .set_health("Missing", health_summary.missing as i64);
        self.metrics
            .set_health("Unknown", health_summary.unknown as i64);

        // Stash the distinct managed GVKs (extracted before `apply_all` moved
        // `classified.main`) so `run_loop` can rebuild watch streams.
        if let Ok(mut g) = self.last_gvks.lock() {
            *g = managed_gvks;
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
    /// Individual apply failures are logged but do not abort the pass; the keys
    /// that failed (discovery or apply error) are returned so the caller can
    /// surface them in state/metrics — the next pass's drift check reports them
    /// missing and re-applies them. When `dry_run` is true each patch is a
    /// server-side dry run (no mutation) — used by `sync --dry-run`.
    async fn apply_all(
        &self,
        manifests: &[RawManifest],
        dry_run: bool,
    ) -> Result<Vec<ResourceKey>> {
        let mut cache = kube_util::DiscoveryCache::new();
        let mut applied = 0usize;
        let mut failed_keys: Vec<ResourceKey> = Vec::new();
        for m in manifests {
            let gk = m.gvk();
            let (ar, caps) = match cache
                .get_or_resolve(&self.client, &m.group, &m.version, &m.kind)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, ?gk, "discovery failed; skipping resource");
                    failed_keys.push(ResourceKey::from_manifest(m));
                    continue;
                }
            };
            let mut value: serde_json::Value = manifest::from_yaml_slice(&m.data)
                .map_err(|e| Error::Manifest(format!("failed to parse manifest: {e}")))?;
            manifest::inject_managed_label_value(
                &mut value,
                &self.cfg.managed_label_key,
                &self.cfg.managed_label_value,
            );
            match kube_util::apply(
                &self.client,
                &ar,
                &caps.scope,
                &self.cfg.namespace,
                value,
                &self.cfg.field_manager,
                dry_run,
            )
            .await
            {
                Ok(_) => {
                    applied += 1;
                    tracing::info!(
                        target: "leancd.audit",
                        action = "apply",
                        kind = %m.kind,
                        name = %m.name,
                        namespace = ?m.namespace,
                        dry_run,
                        result = "applied",
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, name = %m.name, kind = %m.kind, "apply failed");
                    tracing::info!(
                        target: "leancd.audit",
                        action = "apply",
                        kind = %m.kind,
                        name = %m.name,
                        namespace = ?m.namespace,
                        dry_run,
                        result = "failed",
                    );
                    failed_keys.push(ResourceKey::from_manifest(m));
                }
            }
        }
        if !failed_keys.is_empty() {
            tracing::warn!(
                applied,
                failed = failed_keys.len(),
                "apply pass completed with failures"
            );
        } else {
            tracing::debug!(applied, "apply pass complete");
        }
        Ok(failed_keys)
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
        // Audit: one summary line plus one line per failed hook.
        tracing::info!(
            target: "leancd.audit",
            action = "hook",
            phase = label,
            attempted = outcome.attempted,
            succeeded,
            failed = outcome.failures.len(),
        );
        for (key, reason) in &outcome.failures {
            tracing::info!(
                target: "leancd.audit",
                action = "hook",
                phase = label,
                key = ?key,
                result = "failed",
                reason = %reason,
            );
        }
        outcome
    }

    /// Run reconciliation forever on the configured poll interval, backing off
    /// on consecutive failures and stopping cooperatively when [`Self::stop`]
    /// is set. An in-flight pass always finishes before the loop re-checks the
    /// flag, so callers can `await` the handle for a graceful shutdown and fall
    /// back to `abort()` after a timeout.
    pub async fn run_loop(&self) -> Result<()> {
        // Watch trigger (off by default). A watch driver per managed GVK pokes
        // `notify` on any cluster-side change; the select below wakes the loop
        // early instead of waiting for `delay`. The watch only changes WHEN a
        // pass runs — the triggered pass still goes through `run_once` →
        // `reconcile` → Lease, so serialization is unchanged (no re-entrancy:
        // there is exactly one `run_once` call site, in this loop).
        let notify = Arc::new(Notify::new());
        let mut watch_set = if matches!(self.cfg.watch_mode, watch::WatchMode::Off) {
            None
        } else {
            let sel = format!(
                "{}={}",
                self.cfg.managed_label_key, self.cfg.managed_label_value
            );
            tracing::info!(mode = ?self.cfg.watch_mode, "watch mode enabled");
            Some(watch::WatchSet::new(
                self.client.clone(),
                sel,
                self.cfg.watch_mode,
                self.stop.clone(),
                notify.clone(),
                self.cfg.cache_max_object_bytes,
            ))
        };

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

            // Rebuild the watched-GVK set from the manifests just parsed (a diff
            // against the running set, so steady-state passes are a no-op and do
            // not churn streams). In cache mode, also snapshot the stores so the
            // next pass's drift-check reads fresh cached state.
            if let Some(w) = watch_set.as_mut() {
                let gvks = self.last_gvks.lock().map(|g| g.clone()).unwrap_or_default();
                w.rebuild(gvks).await;
                if w.uses_cache() {
                    if let Ok(mut cs) = self.cache_stores.lock() {
                        *cs = w.stores().clone();
                    }
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

            // Sleep, but wake immediately on a watch poke or a shutdown request.
            // On a watch poke, hold a debounce window so a burst of events (a
            // reconnect's InitApply burst, or a rapid edit storm) collapses into
            // a single pass rather than N. `Notify::notified` is cancel-safe.
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = notify.notified() => {
                    if !self.stop.load(Ordering::Acquire) {
                        tokio::time::sleep(self.cfg.watch_debounce).await;
                    }
                }
                _ = self.watch_stop() => {
                    tracing::info!("shutdown requested during sleep; exiting");
                    break;
                }
            }
        }
        if let Some(mut w) = watch_set {
            w.shutdown();
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
    rand::rng().random_range(0.75..1.0)
}

/// Scale `delay` by `jitter_factor()`, i.e. into `[0.75, 1.0)` of it.
fn jittered(delay: Duration) -> Duration {
    delay.mul_f64(jitter_factor())
}

/// Compute the drift between the desired manifests (at the current Git HEAD)
/// and the live cluster, and print a human-readable report. Read-only: no
/// apply, no prune, no state change, no Lease (the git checkout is PID-scoped
/// and the only API calls are `List`s). Used by `leancd diff`.
pub async fn diff(client: &kube::client::Client, cfg: &Config) -> Result<()> {
    // Always fetch the latest HEAD (prev_sha = None forces a fresh fetch).
    let sync = git_sync::sync(cfg, None).await?;
    let work_dir = cfg.effective_work_dir();
    let work = Path::new(&work_dir);
    let roots = manifest::expand_roots(work, &cfg.path)?;
    let manifests = manifest::parse_paths(&roots).await?;
    let classified = hooks::classify(manifests);
    let (drifts, _live) = drift::detect(client, &classified.main, cfg).await?;
    print!("{}", render_diff(&sync.sha, &drifts));
    Ok(())
}

/// Render a human-readable drift report. Pure: no I/O, unit-testable.
fn render_diff(sha: &str, drifts: &[drift::DriftItem]) -> String {
    if drifts.is_empty() {
        return format!("leancd diff (sha {sha}): in sync — no drift detected\n");
    }
    let mut out = format!("leancd diff (sha {sha}): {} drift(s)\n", drifts.len());
    for d in drifts {
        out.push_str(&format!(
            "  {} — {}\n",
            format_resource_key(&d.key),
            d.reason
        ));
    }
    out
}

/// Format a [`prune::ResourceKey`] as `[gvk namespace/name]` (or `[gvk name]`
/// for cluster-scoped resources) for human-readable output. Pure.
fn format_resource_key(key: &prune::ResourceKey) -> String {
    let gvk = if key.group.is_empty() {
        format!("{}/{}", key.version, key.kind)
    } else {
        format!("{}/{}/{}", key.group, key.version, key.kind)
    };
    match &key.namespace {
        Some(ns) => format!("[{gvk} {ns}/{}]", key.name),
        None => format!("[{gvk} {}]", key.name),
    }
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

    fn drift_key(group: &str, kind: &str, name: &str, ns: Option<&str>) -> drift::DriftItem {
        drift::DriftItem {
            key: prune::ResourceKey {
                group: group.to_string(),
                version: "v1".to_string(),
                kind: kind.to_string(),
                namespace: ns.map(String::from),
                name: name.to_string(),
            },
            reason: "spec differs from desired state".to_string(),
        }
    }

    #[test]
    fn render_diff_no_drift_reports_in_sync() {
        let s = render_diff("abc123", &[]);
        assert!(s.contains("in sync"), "{s}");
        assert!(s.contains("abc123"), "{s}");
    }

    #[test]
    fn render_diff_lists_each_drift_with_gvk_and_reason() {
        let drifts = vec![drift_key("apps", "Deployment", "web", Some("ns")), {
            let mut d = drift_key("", "ConfigMap", "global", None);
            d.reason = "missing in cluster".into();
            d
        }];
        let s = render_diff("deadbeef", &drifts);
        assert!(s.contains("2 drift"), "{s}");
        assert!(s.contains("apps/v1/Deployment"), "{s}");
        assert!(s.contains("ns/web"), "{s}");
        assert!(s.contains("v1/ConfigMap"), "{s}");
        assert!(s.contains("global"), "{s}");
        assert!(s.contains("spec differs"), "{s}");
        assert!(s.contains("missing"), "{s}");
        assert!(s.contains("deadbeef"), "{s}");
    }
}
