//! Watch-based drift trigger: subscribe to live changes of the `managed-by`
//! resources Lean CD applies, so a cluster-side edit (`kubectl`, another
//! controller) wakes the reconcile loop immediately instead of waiting up to
//! `poll_interval` for the next periodic pass.
//!
//! Two modes (selected at runtime via `--watch-mode`):
//!  - `Trigger`: a `watcher` stream per managed GVK; each `touched` event (an
//!    apply OR a delete — a deleted managed object is drift too) pokes the
//!    shared `Notify`. Drift detection itself stays the existing List-based
//!    `drift::detect`; the watch only collapses detection latency. Minimal RSS.
//!  - `Cache`:   a `reflector` + `Store` per managed GVK; drift detection reads
//!    from the stores (`drift::detect_from_stores`). Holds a cache of all
//!    managed objects (larger RSS — scales with object count); measured and
//!    kept only if it wins on RSS.
//!
//! The watched GVK set is rebuilt (diffed against the running set) after every
//! successful pass from the manifests just parsed, so streams for kinds that
//! leave Git are dropped and kinds new to Git get streams — without churning
//! stable streams on steady-state passes.
//!
//! Correctness: a watch-triggered reconcile goes through the identical
//! `run_once -> reconcile -> lock::acquire` path, so the Lease serialization
//! (one pass at a time, cluster-wide) is preserved. The watch driver only pokes
//! a `Notify`; it never calls `run_once` itself (no re-entrancy). `sync`
//! (one-shot) never constructs a `WatchSet` — only `controller`'s `run_loop`
//! does.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::StreamExt;
use kube::api::Api;
use kube::client::Client;
use kube::core::DynamicObject;
use kube::runtime::reflector::{self, store::Writer, Store};
use kube::runtime::watcher::{self, Config as WatcherConfig};
use kube::runtime::WatchStreamExt;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::kube_util;

/// Identity of a watched kind: `(group, version, kind)`, matching
/// [`crate::manifest::RawManifest::gvk`].
pub type GvkKey = (String, String, String);

/// How (or whether) cluster-side changes wake the reconcile loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WatchMode {
    /// No watch; drift is found only by the periodic poll loop (today's behavior).
    #[default]
    Off,
    /// Watch managed-by resources and poke the reconcile loop on any change.
    /// Drift detection stays List-based. Minimal RSS.
    Trigger,
    /// Watch + cache managed-by resources in a `Store`; drift detection reads
    /// from the cache. Larger RSS (scales with object count).
    Cache,
}

impl WatchMode {
    /// Parse `off`/`trigger`/`cache` (case-insensitive, surrounding whitespace
    /// tolerated). Anything else is a config error.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(WatchMode::Off),
            "trigger" => Ok(WatchMode::Trigger),
            "cache" => Ok(WatchMode::Cache),
            other => Err(Error::Config(format!(
                "invalid watch-mode '{other}': expected off|trigger|cache"
            ))),
        }
    }
}

/// Build the kube-rs watcher config with the managed-by label selector.
/// Bookmarks stay on (default) so long-lived watches resume efficiently after a
/// quiet period without a full relist.
fn watcher_config(selector: &str) -> WatcherConfig {
    WatcherConfig::default().labels(selector)
}

/// A set of watch-driver tasks, one per managed GVK, all sharing one `Notify`.
///
/// Rebuilding is a diff against the previously-watched set, so steady-state
/// passes (unchanged GVK set) are a no-op and do not churn streams. Discovery
/// or stream failures are logged and skipped, never fatal — the poll loop still
/// converges as the last-resort fallback.
pub struct WatchSet {
    client: Client,
    selector: String,
    mode: WatchMode,
    stop: Arc<AtomicBool>,
    notify: Arc<Notify>,
    handles: HashMap<GvkKey, JoinHandle<()>>,
    /// Cache-mode stores, populated as drivers are spawned. `Trigger`/`Off`
    /// leave this empty.
    stores: HashMap<GvkKey, Store<DynamicObject>>,
}

impl WatchSet {
    /// Construct an empty set. `selector` is the `key=value` managed-by label
    /// selector applied to every watch (so only Lean CD-managed objects wake it).
    pub fn new(
        client: Client,
        selector: String,
        mode: WatchMode,
        stop: Arc<AtomicBool>,
        notify: Arc<Notify>,
    ) -> Self {
        WatchSet {
            client,
            selector,
            mode,
            stop,
            notify,
            handles: HashMap::new(),
            stores: HashMap::new(),
        }
    }

    /// `true` only in `Cache` mode, where the reconciler should run
    /// `drift::detect_from_stores` against [`Self::stores`] instead of the
    /// List-based `drift::detect`.
    pub fn uses_cache(&self) -> bool {
        matches!(self.mode, WatchMode::Cache)
    }

    /// Reconcile the watched-GVK set with `desired`: spawn drivers for new
    /// GVKs, abort drivers for removed GVKs. Kinds present in both are left
    /// untouched (no stream churn). A no-op when `desired` equals the running
    /// set. In `Off` mode this is always a no-op (no drivers).
    pub async fn rebuild(&mut self, desired: HashSet<GvkKey>) {
        if matches!(self.mode, WatchMode::Off) {
            return;
        }
        let current: HashSet<GvkKey> = self.handles.keys().cloned().collect();
        let (to_add, to_drop) = diff_watched_set(current, desired);

        for key in to_drop {
            if let Some(handle) = self.handles.remove(&key) {
                handle.abort();
            }
            self.stores.remove(&key);
        }

        for key in to_add {
            if self.stop.load(Ordering::Acquire) {
                break;
            }
            let (group, version, kind) = &key;
            let (ar, _caps) = match kube_util::resolve(&self.client, group, version, kind).await {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(error = %e, ?key, "watch: discovery failed; skipping kind");
                    continue;
                }
            };
            let api = Api::all_with(self.client.clone(), &ar);
            match self.mode {
                WatchMode::Off => {}
                WatchMode::Trigger => {
                    let notify = self.notify.clone();
                    let stop = self.stop.clone();
                    let sel = self.selector.clone();
                    let handle = tokio::spawn(async move {
                        let stream = watcher::watcher(api, watcher_config(&sel))
                            .default_backoff()
                            .touched_objects();
                        futures::pin_mut!(stream);
                        while let Some(ev) = stream.next().await {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            match ev {
                                Ok(_) => notify.notify_one(),
                                // default_backoff handles relist/reconnect; a
                                // transient stream error is expected and retried.
                                Err(e) => tracing::debug!(
                                    error = %e,
                                    "watch: stream error (will backoff/reconnect)"
                                ),
                            }
                        }
                    });
                    self.handles.insert(key, handle);
                }
                WatchMode::Cache => {
                    // DynamicObject's DynamicType is ApiResource, which does NOT
                    // implement Default, so we cannot use `reflector::store()`;
                    // construct the Writer with the resolved ApiResource and
                    // derive the read handle from it.
                    let writer = Writer::new(ar.clone());
                    let reader = writer.as_reader();
                    let stop = self.stop.clone();
                    let sel = self.selector.clone();
                    let stream = reflector::reflector(
                        writer,
                        watcher::watcher(api, watcher_config(&sel)).default_backoff(),
                    );
                    let handle = tokio::spawn(async move {
                        futures::pin_mut!(stream);
                        while let Some(ev) = stream.next().await {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            if let Err(e) = ev {
                                tracing::debug!(
                                    error = %e,
                                    "watch: reflector error (will backoff/reconnect)"
                                );
                            }
                        }
                    });
                    self.handles.insert(key.clone(), handle);
                    self.stores.insert(key, reader);
                }
            }
        }
    }

    /// Abort every driver and drop the stores. Also achieved by `stop`
    /// propagation; this is the deterministic shutdown path.
    pub fn shutdown(&mut self) {
        for (_, handle) in self.handles.drain() {
            handle.abort();
        }
        self.stores.clear();
    }

    /// Snapshot of the cache-mode stores (empty in `Trigger`/`Off`). Used by
    /// `drift::detect_from_stores`.
    pub fn stores(&self) -> &HashMap<GvkKey, Store<DynamicObject>> {
        &self.stores
    }
}

impl Drop for WatchSet {
    fn drop(&mut self) {
        // Best-effort: abort drivers so a dropped WatchSet does not leak tasks
        // waiting on a `stop` that may never be set.
        for (_, handle) in self.handles.drain() {
            handle.abort();
        }
    }
}

/// Pure diff of the watched-GVK set: returns `(to_add, to_drop)` — kinds in
/// `desired` not currently watched, and kinds currently watched but no longer
/// desired. Kinds in both are left alone. Order within each vec is unspecified.
pub fn diff_watched_set(
    current: HashSet<GvkKey>,
    desired: HashSet<GvkKey>,
) -> (Vec<GvkKey>, Vec<GvkKey>) {
    let to_add = desired.difference(&current).cloned().collect();
    let to_drop = current.difference(&desired).cloned().collect();
    (to_add, to_drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gvk(g: &str, v: &str, k: &str) -> GvkKey {
        (g.to_string(), v.to_string(), k.to_string())
    }

    #[test]
    fn diff_no_change() {
        let cur: HashSet<_> = [gvk("apps", "v1", "Deployment")].into_iter().collect();
        let des: HashSet<_> = [gvk("apps", "v1", "Deployment")].into_iter().collect();
        let (add, drop_) = diff_watched_set(cur, des);
        assert!(add.is_empty());
        assert!(drop_.is_empty());
    }

    #[test]
    fn diff_adds_new_kind() {
        let cur: HashSet<_> = HashSet::new();
        let des: HashSet<_> = [gvk("", "v1", "ConfigMap")].into_iter().collect();
        let (add, drop_) = diff_watched_set(cur, des);
        assert_eq!(add, vec![gvk("", "v1", "ConfigMap")]);
        assert!(drop_.is_empty());
    }

    #[test]
    fn diff_drops_removed_kind() {
        let cur: HashSet<_> = [gvk("", "v1", "ConfigMap")].into_iter().collect();
        let des: HashSet<_> = HashSet::new();
        let (add, drop_) = diff_watched_set(cur, des);
        assert!(add.is_empty());
        assert_eq!(drop_, vec![gvk("", "v1", "ConfigMap")]);
    }

    #[test]
    fn diff_partial_change() {
        let cur: HashSet<_> = [gvk("", "v1", "ConfigMap"), gvk("apps", "v1", "Deployment")]
            .into_iter()
            .collect();
        let des: HashSet<_> = [gvk("apps", "v1", "Deployment"), gvk("", "v1", "Service")]
            .into_iter()
            .collect();
        let (mut add, mut drop_) = diff_watched_set(cur, des);
        add.sort();
        drop_.sort();
        assert_eq!(add, vec![gvk("", "v1", "Service")]);
        assert_eq!(drop_, vec![gvk("", "v1", "ConfigMap")]);
    }

    #[test]
    fn watch_mode_parse_variants() {
        assert_eq!(WatchMode::parse("off").unwrap(), WatchMode::Off);
        assert_eq!(WatchMode::parse("TRIGGER").unwrap(), WatchMode::Trigger);
        assert_eq!(WatchMode::parse(" Cache ").unwrap(), WatchMode::Cache);
        assert_eq!(WatchMode::default(), WatchMode::Off);
    }

    #[test]
    fn watch_mode_parse_invalid() {
        assert!(WatchMode::parse("yes").is_err());
        assert!(WatchMode::parse("").is_err());
    }
}
