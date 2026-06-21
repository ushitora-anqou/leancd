//! Reconcile-pass mutual exclusion via a Kubernetes Lease.
//!
//! `controller` (polling loop) and a concurrent `sync` (one pass, possibly via
//! `kubectl exec` in the same Pod or in a separate Pod) share one `Reconciler`.
//! To guarantee that a Git HEAD is applied atomically — never two passes at
//! once racing on the git checkout or clobbering the state ConfigMap — each
//! reconcile pass first acquires a `coordination.k8s.io/v1` Lease and holds it
//! for the duration of the pass (git fetch → apply → prune → state write).
//!
//! Acquisition uses Kubernetes optimistic concurrency: create the Lease (409 if
//! a peer raced ahead), or, for an existing Lease that has gone stale (its
//! `renewTime` is older than `leaseDurationSeconds`), `replace` it with the
//! prior `resourceVersion` as a precondition (409 on contention). A pass that
//! finds a fresh lease held by another process skips with an INFO log rather
//! than erroring, so `sync_errors` is not incremented and the controller does
//! not back off. A holder that crashes without releasing is reclaimed after
//! `lock_lease_duration` by the next passer.
//!
//! This serialization is what makes the state ConfigMap safe without CAS: with
//! passes serialized, the SSA `state::write` cannot lose updates.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use jiff::{Timestamp, Unit};
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::api::{Api, DeleteParams, ObjectMeta, PostParams};
use kube::client::Client;

use crate::config::Config;
use crate::error::{Error, Result};

/// Interval between acquire retries while waiting for a busy lease.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Why acquiring the reconcile Lease failed. `Busy` is not an error: another
/// pass is in flight and this one should skip with an INFO log (not counted as
/// a sync error). `Kube` is a real API failure that should propagate.
pub enum AcquireError {
    /// Another process holds a fresh (non-stale) lease; we gave up after the
    /// wait timeout (or shutdown was requested).
    Busy,
    /// The Lease API call itself failed (RBAC, apiserver unreachable, ...).
    Kube(kube::Error),
}

/// RAII handle to a held reconcile Lease.
///
/// Held for the duration of one reconcile pass. Dropping it without
/// [`Self::release`] only logs a warning — `async` cannot run in `Drop`, so
/// best-effort cleanup relies on the stale-reclaim timeout
/// (`lock_lease_duration`): if a holder dies without releasing, the next
/// passer forcibly acquires after that duration. Callers that can should
/// `await release()` explicitly so the next pass need not wait.
pub struct LeaseGuard {
    name: String,
    namespace: String,
    holder: String,
    client: Client,
    released: AtomicBool,
}

impl LeaseGuard {
    /// Explicitly release the lease by deleting it. Idempotent (404 ignored,
    /// double-release ignored). Prefer this over letting the guard drop.
    pub async fn release(self) -> Result<()> {
        if self.released.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        match api.delete(&self.name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(Error::Kube(e)),
        }
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if !self.released.load(Ordering::Acquire) {
            // Cannot run async deletion from Drop; rely on stale-reclaim.
            tracing::warn!(
                lease = %self.name,
                "lease guard dropped without explicit release; \
                 it will be reclaimed after lock_lease_duration"
            );
        }
    }
}

/// The reconcile-exclusion Lease name, derived from the state ConfigMap name so
/// each sync target gets its own lock within the namespace.
pub fn lease_name(state_configmap: &str) -> String {
    format!("{state_configmap}-reconcile-lock")
}

/// The holder-identity base: the Pod hostname (injected by Kubernetes as
/// `HOSTNAME`) if set, else the explicit `LEANCD_POD_NAME`, else `"leancd"`.
pub fn holder_base() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("LEANCD_POD_NAME")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "leancd".to_string())
}

/// Build a Kubernetes `holderIdentity` (`{base}:{pid}`), truncated to fit the
/// 63-byte limit on a UTF-8 char boundary. An empty `base` falls back to
/// `"leancd"` so the identity is never empty.
pub fn holder_identity(base: &str, pid: u32) -> String {
    let suffix = format!(":{pid}");
    let max_base = 63usize.saturating_sub(suffix.len());
    let chosen = if base.is_empty() {
        "leancd".to_string()
    } else {
        truncate_char_boundary(base, max_base)
    };
    format!("{chosen}{suffix}")
}

/// Truncate `s` to at most `max_bytes` on a UTF-8 char boundary.
fn truncate_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Whether a lease is stale (forcibly acquirable): true only when both the
/// renew time and the lease duration are present and `now - renew_time >=
/// duration`. Missing fields → `false` (safe side: never steal an unknown
/// lease). A renew time in the future (clock skew) → `false`.
pub fn is_stale(
    renew_time: Option<&MicroTime>,
    lease_duration_secs: Option<i32>,
    now: Timestamp,
) -> bool {
    let duration_secs = match lease_duration_secs {
        Some(s) if s > 0 => s,
        _ => return false,
    };
    let renew = match renew_time {
        Some(r) => r.0,
        None => return false,
    };
    let elapsed = match now.since(renew) {
        Ok(span) => span,
        Err(_) => return false,
    };
    match elapsed.total(Unit::Second) {
        Ok(secs) => secs >= duration_secs as f64,
        Err(_) => false,
    }
}

/// Acquire the reconcile Lease, waiting up to `cfg.lock_wait_timeout` for a
/// busy lease to free up or go stale. Returns `Err(Busy)` on timeout or
/// shutdown, `Err(Kube(..))` on an API failure.
pub async fn acquire(
    client: &Client,
    cfg: &Config,
    holder: &str,
    stop: &AtomicBool,
) -> std::result::Result<LeaseGuard, AcquireError> {
    let api: Api<Lease> = Api::namespaced(client.clone(), &cfg.namespace);
    let name = lease_name(&cfg.state_configmap);
    let deadline = tokio::time::Instant::now() + cfg.lock_wait_timeout;

    loop {
        if stop.load(Ordering::Acquire) {
            return Err(AcquireError::Busy);
        }
        match try_acquire(client, &api, &name, cfg, holder).await {
            AcquireOutcome::Acquired(g) => return Ok(g),
            AcquireOutcome::Busy => {}
            AcquireOutcome::Err(e) => return Err(e),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(AcquireError::Busy);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let sleep_dur = remaining.min(POLL_INTERVAL);
        tokio::select! {
            _ = tokio::time::sleep(sleep_dur) => {}
            _ = wait_stop(stop) => return Err(AcquireError::Busy),
        }
    }
}

enum AcquireOutcome {
    Acquired(LeaseGuard),
    Busy,
    Err(AcquireError),
}

/// One acquire attempt: create a missing lease, forcibly take a stale one, or
/// report busy. Never blocks.
async fn try_acquire(
    client: &Client,
    api: &Api<Lease>,
    name: &str,
    cfg: &Config,
    holder: &str,
) -> AcquireOutcome {
    let secs = cfg.lock_lease_duration.as_secs() as i32;
    let now = Timestamp::now();

    match api.get(name).await {
        Ok(existing) => {
            let stale = is_stale(
                existing.spec.as_ref().and_then(|s| s.renew_time.as_ref()),
                existing
                    .spec
                    .as_ref()
                    .and_then(|s| s.lease_duration_seconds),
                now,
            );
            if !stale {
                return AcquireOutcome::Busy;
            }
            // Forcibly take the stale lease, using its resourceVersion as the
            // optimistic-concurrency precondition (409 if someone updated it).
            let mut lease = build_lease(name, &cfg.namespace, holder, secs, now);
            lease.metadata.resource_version = existing.metadata.resource_version.clone();
            match api.replace(name, &post_params(cfg), &lease).await {
                Ok(_) => AcquireOutcome::Acquired(guard(client, cfg, name, holder)),
                Err(kube::Error::Api(e)) if e.code == 409 => AcquireOutcome::Busy,
                Err(e) => AcquireOutcome::Err(AcquireError::Kube(e)),
            }
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            // No lease yet: create one (409 if a peer raced ahead).
            let lease = build_lease(name, &cfg.namespace, holder, secs, now);
            match api.create(&post_params(cfg), &lease).await {
                Ok(_) => AcquireOutcome::Acquired(guard(client, cfg, name, holder)),
                Err(kube::Error::Api(e)) if e.code == 409 => AcquireOutcome::Busy,
                Err(e) => AcquireOutcome::Err(AcquireError::Kube(e)),
            }
        }
        Err(e) => AcquireOutcome::Err(AcquireError::Kube(e)),
    }
}

/// Build a fresh Lease body owned by `holder`. `resource_version` is left unset;
/// callers that `replace` an existing lease set it afterwards as a precondition.
fn build_lease(
    name: &str,
    namespace: &str,
    holder: &str,
    lease_duration_secs: i32,
    now: Timestamp,
) -> Lease {
    Lease {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(holder.to_string()),
            lease_duration_seconds: Some(lease_duration_secs),
            acquire_time: Some(MicroTime(now)),
            renew_time: Some(MicroTime(now)),
            lease_transitions: Some(0),
            ..Default::default()
        }),
    }
}

fn post_params(cfg: &Config) -> PostParams {
    PostParams {
        field_manager: Some(cfg.field_manager.clone()),
        ..Default::default()
    }
}

fn guard(client: &Client, cfg: &Config, name: &str, holder: &str) -> LeaseGuard {
    LeaseGuard {
        name: name.to_string(),
        namespace: cfg.namespace.clone(),
        holder: holder.to_string(),
        client: client.clone(),
        released: AtomicBool::new(false),
    }
}

/// Refresh the lease's `renewTime` so it is not reclaimed as stale during a
/// long pass. Call this at the major await points of a reconcile pass (current
/// runtime is single-threaded, so a background renew task would not run while
/// the pass body is blocked).
///
/// Returns `Ok(true)` on success, `Ok(false)` if the lease was lost (held by
/// another process, or deleted/contended). A `false` means the current pass is
/// no longer the sole holder; the caller logs and continues best-effort — the
/// PID-scoped `work_dir` still prevents git corruption, and the next pass
/// re-converges to Git HEAD.
pub async fn renew(client: &Client, cfg: &Config, guard: &LeaseGuard) -> Result<bool> {
    let api: Api<Lease> = Api::namespaced(client.clone(), &guard.namespace);
    let existing = match api.get(&guard.name).await {
        Ok(o) => o,
        Err(kube::Error::Api(e)) if e.code == 404 => return Ok(false),
        Err(e) => return Err(Error::Kube(e)),
    };
    let still_mine = existing
        .spec
        .as_ref()
        .and_then(|s| s.holder_identity.as_deref())
        == Some(guard.holder.as_str());
    if !still_mine {
        tracing::warn!(
            lease = %guard.name,
            "renew: lease now held by another process; lost the lock"
        );
        return Ok(false);
    }
    let secs = cfg.lock_lease_duration.as_secs() as i32;
    let mut lease = build_lease(
        &guard.name,
        &guard.namespace,
        &guard.holder,
        secs,
        Timestamp::now(),
    );
    lease.metadata.resource_version = existing.metadata.resource_version.clone();
    match api.replace(&guard.name, &post_params(cfg), &lease).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => Ok(false),
        Err(e) => Err(Error::Kube(e)),
    }
}

/// Resolve once `stop` is set (polled on a short interval). Used to short
/// -circuit the inter-retry sleep during shutdown, mirroring `reconcile::watch_stop`.
async fn wait_stop(stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::{Timestamp, ToSpan};

    #[test]
    fn lease_name_uses_state_configmap_prefix() {
        assert_eq!(lease_name("leancd-state"), "leancd-state-reconcile-lock");
        assert_eq!(
            lease_name("leancd-state-e2e-concurrent"),
            "leancd-state-e2e-concurrent-reconcile-lock"
        );
    }

    #[test]
    fn holder_identity_combines_base_and_pid() {
        assert_eq!(holder_identity("leancd-abc", 42), "leancd-abc:42");
    }

    #[test]
    fn holder_identity_falls_back_when_empty() {
        assert_eq!(holder_identity("", 1), "leancd:1");
    }

    #[test]
    fn holder_identity_truncates_long_base_to_fit_63_bytes() {
        let long = "a".repeat(100);
        let id = holder_identity(&long, 99);
        assert!(id.len() <= 63, "holderIdentity must be <= 63 bytes: {id}");
        assert!(id.ends_with(":99"));
    }

    #[test]
    fn holder_identity_respects_utf8_char_boundary() {
        // Each 'あ' is 3 bytes; truncation must not split a code point.
        let base = "あ".repeat(30); // 90 bytes
        let id = holder_identity(&base, 7);
        assert!(id.len() <= 63, "{id} > 63 bytes");
        assert!(id.ends_with(":7"));
        // The base portion is valid UTF-8 (no panic, decodes cleanly).
        let base_part = &id[..id.len() - ":7".len()];
        assert!(std::str::from_utf8(base_part.as_bytes()).is_ok());
    }

    #[test]
    fn is_stale_true_when_renew_older_than_duration() {
        let now = Timestamp::now();
        let renew = now.checked_sub(120.seconds()).unwrap();
        assert!(is_stale(Some(&MicroTime(renew)), Some(60), now));
    }

    #[test]
    fn is_stale_false_when_within_duration() {
        let now = Timestamp::now();
        let renew = now.checked_sub(10.seconds()).unwrap();
        assert!(!is_stale(Some(&MicroTime(renew)), Some(60), now));
    }

    #[test]
    fn is_stale_false_when_fields_missing() {
        let now = Timestamp::now();
        // No renew time.
        assert!(!is_stale(None, Some(60), now));
        // No duration.
        assert!(!is_stale(Some(&MicroTime(now)), None, now));
        // Non-positive duration.
        assert!(!is_stale(Some(&MicroTime(now)), Some(0), now));
        assert!(!is_stale(Some(&MicroTime(now)), Some(-5), now));
    }

    #[test]
    fn is_stale_false_when_renew_in_future() {
        // Clock skew: renew time ahead of now. Must not be treated as stale.
        let now = Timestamp::now();
        let renew = now.checked_add(30.seconds()).unwrap();
        assert!(!is_stale(Some(&MicroTime(renew)), Some(60), now));
    }

    #[test]
    fn truncate_keeps_short_string_intact() {
        assert_eq!(truncate_char_boundary("abc", 10), "abc");
        assert_eq!(truncate_char_boundary("abc", 3), "abc");
    }

    #[test]
    fn truncate_cuts_on_byte_boundary() {
        assert_eq!(truncate_char_boundary("abcdef", 3), "abc");
    }
}
