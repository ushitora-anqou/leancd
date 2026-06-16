//! The reconciliation engine shared by `controller` (polling loop) and `sync`
//! (single pass). Fetches Git, parses manifests, applies via server-side apply,
//! prunes removed resources, detects drift, and persists state.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use kube::client::Client;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::git_sync;
use crate::kube_util;
use crate::manifest::{self, RawManifest};
use crate::metrics::Metrics;
use crate::{drift, prune, state};

/// Drives a single repository-sync target.
pub struct Reconciler {
    pub client: Client,
    pub cfg: Config,
    pub metrics: Arc<Metrics>,
}

impl Reconciler {
    /// One reconciliation pass. `force` enables force-conflict server-side apply.
    pub async fn run_once(&self, force: bool) -> Result<()> {
        self.metrics.sync_total.inc();
        match self.reconcile(force).await {
            Ok(()) => {
                self.metrics.last_success_epoch.set(now_epoch() as i64);
                Ok(())
            }
            Err(e) => {
                self.metrics.sync_errors.inc();
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

    async fn reconcile(&self, force: bool) -> Result<()> {
        let prev = state::read(&self.client, &self.cfg).await?;
        let prev_sha = prev
            .as_ref()
            .and_then(|s| s.last_sha.clone())
            .filter(|s| !s.is_empty());
        let prev_applied = prev.as_ref().map(|s| s.applied.clone()).unwrap_or_default();

        let sync = git_sync::sync(&self.cfg, prev_sha.as_deref()).await?;
        let work = Path::new(&self.cfg.work_dir);
        let roots = manifest::expand_roots(work, &self.cfg.path)?;
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
        let current_keys = prune::ResourceKey::keys_of(&manifests);

        let do_full = should_full_apply(force, prev.is_some(), sync.changed);
        let mut drifts: Vec<drift::DriftItem> = Vec::new();
        if do_full {
            self.apply_all(&manifests, force).await?;
        } else {
            drifts = drift::detect(&self.client, &manifests, &self.cfg).await?;
            for d in &drifts {
                tracing::warn!(key = ?d.key, reason = %d.reason, "drift detected");
            }
            if !drifts.is_empty() {
                tracing::info!(
                    drift = drifts.len(),
                    "drift detected; re-applying managed resources"
                );
                // BUG 4: force-apply on drift-triggered self-heal so a field
                // claimed by another SSA field manager (e.g. `kubectl
                // edit/patch`) is reclaimed back to Git. The controller's
                // top-level `force` (false) still governs the initial full apply.
                self.apply_all(&manifests, true).await?;
            }
        }
        let drift_count = drifts.len();

        let deleted = prune::prune(&self.client, &prev_applied, &current_keys, &self.cfg).await?;

        let mut new_state = prev.clone().unwrap_or_default();
        new_state.last_sha = Some(sync.sha.clone());
        new_state.last_sync_epoch = Some(now_epoch());
        new_state.sync_count = new_state.sync_count.saturating_add(1);
        new_state.last_error = None;
        new_state.drift_count = drift_count;
        new_state.managed_count = current_keys.len();
        new_state.applied = current_keys.clone();
        state::write(&self.client, &self.cfg, &new_state).await?;

        self.metrics
            .managed_resources
            .set(current_keys.len() as i64);
        // Per-GVK drift counts; reset first so resolved drifts clear next pass.
        self.metrics.drift_detected.reset();
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
            self.metrics
                .drift_detected
                .with_label_values(&[&group, &version, &kind])
                .set(n);
        }

        tracing::info!(
            sha = %sync.sha, force, full = do_full,
            managed = current_keys.len(), pruned = deleted.len(), drift = drift_count,
            "reconciliation complete"
        );
        Ok(())
    }

    /// Apply every manifest, sharing one API-discovery lookup per resource kind.
    /// Individual apply failures are logged but do not abort the pass.
    async fn apply_all(&self, manifests: &[RawManifest], force: bool) -> Result<()> {
        let mut cache: HashMap<
            (String, String, String),
            (kube::core::ApiResource, kube::discovery::ApiCapabilities),
        > = HashMap::new();
        let mut applied = 0usize;
        let mut failed = 0usize;
        for m in manifests {
            let gk = m.gvk();
            let (ar, caps) = match cache.get(&gk) {
                Some(c) => c.clone(),
                None => {
                    match kube_util::resolve(&self.client, &m.group, &m.version, &m.kind).await {
                        Ok(c) => {
                            cache.insert(gk, c.clone());
                            c
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, ?gk, "discovery failed; skipping resource");
                            failed += 1;
                            continue;
                        }
                    }
                }
            };
            match kube_util::apply(
                &self.client,
                &ar,
                &caps.scope,
                &self.cfg.namespace,
                &m.data,
                &self.cfg.field_manager,
                force,
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

    /// Run reconciliation forever on the configured poll interval. Designed to
    /// be spawned and aborted for shutdown.
    pub async fn run_loop(&self) -> Result<()> {
        loop {
            if let Err(e) = self.run_once(false).await {
                tracing::error!(error = %e, "reconciliation failed");
            }
            tokio::time::sleep(self.cfg.poll_interval).await;
        }
    }
}

/// Whether a reconciliation should fully re-apply every manifest (rather than
/// only drift-check). Full apply runs on `--force`, first run (no prior state),
/// or when the Git HEAD moved. Pure: no API calls.
fn should_full_apply(force: bool, has_prev: bool, changed: bool) -> bool {
    force || !has_prev || changed
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

    /// `should_full_apply` is true when force is set, there is no prior state
    /// (first run), or the Git HEAD moved. Otherwise (steady state) we
    /// drift-check instead of re-applying. Verified exhaustively.
    #[test]
    fn should_full_apply_truth_table() {
        for &(force, has_prev, changed) in &[
            (false, false, false),
            (false, false, true),
            (false, true, false),
            (false, true, true),
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (true, true, true),
        ] {
            assert_eq!(
                should_full_apply(force, has_prev, changed),
                force || !has_prev || changed,
                "force={force} has_prev={has_prev} changed={changed}"
            );
        }
    }

    #[test]
    fn steady_state_does_not_full_apply() {
        // The only combination that skips full apply: no force, has prior
        // state, no Git change -> the drift-check path.
        assert!(!should_full_apply(false, true, false));
    }
}
