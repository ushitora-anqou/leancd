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
//!  - `Cache`:   a size-bounded `LightweightStore` per managed GVK (the watcher
//!    stream consumed directly, no reflector). Objects up to
//!    `--cache-max-object-bytes` are cached in full (SmallTier) and drift-checked
//!    straight from the cache; larger ones are tracked by key only (LargeTier)
//!    and drift-checked via a per-GVK `List` fallback (`drift::detect_from_lw`).
//!    The cache's RSS therefore does not grow with per-object payload size.
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
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use kube::api::Api;
use kube::client::Client;
use kube::core::DynamicObject;
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

/// Which tier a key falls into in a [`LightweightStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Small object: body cached, drift-checked directly from the cache.
    Small,
    /// Large object: key only; drift-checked via a per-GVK `List` fallback.
    Large,
    /// No live object known for this key (the store is past `InitDone`) → drift.
    Absent,
}

/// Identity of one object within a single GVK's [`LightweightStore`]:
/// `(name, namespace)`. The GVK is the outer `HashMap` key, not repeated here.
pub type ObjKey = (String, Option<String>);

/// A size-bounded replacement for `reflector::Store` in `cache` watch mode.
///
/// Objects whose serialized size is `<= max_bytes` are kept in full (SmallTier)
/// so drift can be subset-checked straight from the cache. Larger objects are
/// tracked by key only (LargeTier): their existence is known (so a vanished
/// live object is still detected as drift), but their body is not held — a
/// LargeTier key's drift falls back to a per-GVK `List` (see `drift.rs`). This
/// keeps the cache's RSS from growing with per-object payload size, regardless
/// of which resource kind carries the large `data`/`spec`.
///
/// Event handling mirrors `reflector::store::Writer::apply_watcher_event`:
/// `Init` opens a buffer, `InitApply` fills it, `InitDone` atomically swaps it
/// in (so a relist drops stale objects and prevents phantom drift). Readers
/// (drift checks) only see the swapped-in `small`/`large`, never the buffer.
pub struct LightweightStore {
    /// SmallTier: full bodies (size <= max_bytes).
    small: HashMap<ObjKey, DynamicObject>,
    /// LargeTier: keys only (size > max_bytes).
    large: HashSet<ObjKey>,
    /// Init buffer for SmallTier (Some only between `Init` and `InitDone`).
    buf_small: Option<HashMap<ObjKey, DynamicObject>>,
    /// Init buffer for LargeTier (Some only between `Init` and `InitDone`).
    buf_large: Option<HashSet<ObjKey>>,
    max_bytes: usize,
}

impl LightweightStore {
    /// Empty store with the given per-object size threshold (bytes). `0` means
    /// every object is LargeTier (no bodies cached; drift is fully List-based).
    pub fn new(max_bytes: usize) -> Self {
        Self {
            small: HashMap::new(),
            large: HashSet::new(),
            buf_small: None,
            buf_large: None,
            max_bytes,
        }
    }

    /// The serialized size (bytes) of an object, used for tier routing. Falls
    /// back to 0 (SmallTier) if serialization fails — a malformed object is not
    /// worth a List round-trip.
    fn obj_size(obj: &DynamicObject) -> usize {
        serde_json::to_vec(obj).map(|v| v.len()).unwrap_or(0)
    }

    /// `(name, namespace)` for an object.
    fn key_of(obj: &DynamicObject) -> ObjKey {
        (
            obj.metadata.name.clone().unwrap_or_default(),
            obj.metadata.namespace.clone(),
        )
    }

    /// Update the store from one `watcher::Event`, routing by serialized size.
    /// Mirrors `Writer::apply_watcher_event` with the added Small/Large split.
    pub fn apply_event(&mut self, event: &watcher::Event<DynamicObject>) {
        match event {
            watcher::Event::Apply(obj) => {
                let key = Self::key_of(obj);
                let large = Self::obj_size(obj) > self.max_bytes;
                if large {
                    self.small.remove(&key);
                    self.large.insert(key);
                } else {
                    self.large.remove(&key);
                    self.small.insert(key, obj.clone());
                }
            }
            watcher::Event::Delete(obj) => {
                let key = Self::key_of(obj);
                self.small.remove(&key);
                self.large.remove(&key);
            }
            watcher::Event::Init => {
                self.buf_small = Some(HashMap::new());
                self.buf_large = Some(HashSet::new());
            }
            watcher::Event::InitApply(obj) => {
                let key = Self::key_of(obj);
                let large = Self::obj_size(obj) > self.max_bytes;
                if let (Some(bs), Some(bl)) = (self.buf_small.as_mut(), self.buf_large.as_mut()) {
                    if large {
                        bs.remove(&key);
                        bl.insert(key);
                    } else {
                        bl.remove(&key);
                        bs.insert(key, obj.clone());
                    }
                }
            }
            watcher::Event::InitDone => {
                if let (Some(bs), Some(bl)) = (self.buf_small.take(), self.buf_large.take()) {
                    self.small = bs;
                    self.large = bl;
                }
            }
        }
    }

    /// Which tier `key` is in. `Absent` means no live object is known for the
    /// key (the store is past `InitDone`), so a manifest claiming it is drift.
    pub fn tier_of(&self, key: &ObjKey) -> Tier {
        if self.small.contains_key(key) {
            Tier::Small
        } else if self.large.contains(key) {
            Tier::Large
        } else {
            Tier::Absent
        }
    }

    /// Borrow the SmallTier body for `key` (for a direct subset drift check).
    pub fn small_get(&self, key: &ObjKey) -> Option<&DynamicObject> {
        self.small.get(key)
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
    stores: HashMap<GvkKey, Arc<Mutex<LightweightStore>>>,
    /// Per-object size threshold (bytes) for cache-mode `LightweightStore`s.
    max_bytes: usize,
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
        max_bytes: usize,
    ) -> Self {
        WatchSet {
            client,
            selector,
            mode,
            stop,
            notify,
            handles: HashMap::new(),
            stores: HashMap::new(),
            max_bytes,
        }
    }

    /// `true` only in `Cache` mode, where the reconciler should run
    /// `drift::detect_from_lw` against [`Self::stores`] instead of the
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
                    // A size-bounded store: objects <= max_bytes are cached in
                    // full (SmallTier), larger ones by key only (LargeTier, drift
                    // falls back to a per-GVK List). The watcher stream is
                    // consumed directly (no reflector) so each event can be
                    // routed by serialized size.
                    let store = Arc::new(Mutex::new(LightweightStore::new(self.max_bytes)));
                    let stop = self.stop.clone();
                    let sel = self.selector.clone();
                    let store_drv = store.clone();
                    let handle = tokio::spawn(async move {
                        let stream = watcher::watcher(api, watcher_config(&sel)).default_backoff();
                        futures::pin_mut!(stream);
                        while let Some(ev) = stream.next().await {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            match ev {
                                Ok(event) => {
                                    if let Ok(mut g) = store_drv.lock() {
                                        g.apply_event(&event);
                                    }
                                }
                                Err(e) => tracing::debug!(
                                    error = %e,
                                    "watch: stream error (will backoff/reconnect)"
                                ),
                            }
                        }
                    });
                    self.handles.insert(key.clone(), handle);
                    self.stores.insert(key, store);
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
    /// `drift::detect_from_lw`.
    pub fn stores(&self) -> &HashMap<GvkKey, Arc<Mutex<LightweightStore>>> {
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

    // --- LightweightStore: size-bounded cache tier routing ---

    /// A ConfigMap-like DynamicObject with `payload_bytes` of `data`.
    fn lw_obj(name: &str, namespace: Option<&str>, payload_bytes: usize) -> DynamicObject {
        let payload = "x".repeat(payload_bytes);
        let mut v = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": { "name": name },
            "data": { "payload": payload },
        });
        if let Some(ns) = namespace {
            v["metadata"]["namespace"] = serde_json::json!(ns);
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn lightweight_store_small_object_cached_in_full() {
        let mut s = LightweightStore::new(1024);
        s.apply_event(&watcher::Event::Apply(lw_obj("a", Some("ns"), 10)));
        assert_eq!(s.tier_of(&("a".into(), Some("ns".into()))), Tier::Small);
        assert!(s.small_get(&("a".into(), Some("ns".into()))).is_some());
    }

    #[test]
    fn lightweight_store_large_object_key_only() {
        let mut s = LightweightStore::new(100);
        s.apply_event(&watcher::Event::Apply(lw_obj("big", None, 10_000)));
        assert_eq!(s.tier_of(&("big".into(), None)), Tier::Large);
        assert!(s.small_get(&("big".into(), None)).is_none());
    }

    #[test]
    fn lightweight_store_delete_removes_from_both_tiers() {
        let mut s = LightweightStore::new(100);
        s.apply_event(&watcher::Event::Apply(lw_obj("small", None, 10)));
        s.apply_event(&watcher::Event::Apply(lw_obj("big", None, 10_000)));
        s.apply_event(&watcher::Event::Delete(lw_obj("small", None, 10)));
        s.apply_event(&watcher::Event::Delete(lw_obj("big", None, 10_000)));
        assert_eq!(s.tier_of(&("small".into(), None)), Tier::Absent);
        assert_eq!(s.tier_of(&("big".into(), None)), Tier::Absent);
    }

    #[test]
    fn lightweight_store_apply_promotes_small_to_large() {
        // Same key grows past the threshold → moves Small → Large, body dropped.
        let mut s = LightweightStore::new(100);
        s.apply_event(&watcher::Event::Apply(lw_obj("a", None, 10)));
        assert_eq!(s.tier_of(&("a".into(), None)), Tier::Small);
        s.apply_event(&watcher::Event::Apply(lw_obj("a", None, 10_000)));
        assert_eq!(s.tier_of(&("a".into(), None)), Tier::Large);
        assert!(s.small_get(&("a".into(), None)).is_none());
    }

    #[test]
    fn lightweight_store_threshold_boundary() {
        // size == max_bytes → Small; size == max_bytes + 1 → Large.
        let obj = lw_obj("a", None, 10);
        let size = LightweightStore::obj_size(&obj);
        let mut small = LightweightStore::new(size);
        small.apply_event(&watcher::Event::Apply(obj.clone()));
        assert_eq!(small.tier_of(&("a".into(), None)), Tier::Small);
        let mut large = LightweightStore::new(size - 1);
        large.apply_event(&watcher::Event::Apply(obj));
        assert_eq!(large.tier_of(&("a".into(), None)), Tier::Large);
    }

    #[test]
    fn lightweight_store_zero_threshold_all_large() {
        // max_bytes = 0 → every non-empty object is LargeTier.
        let mut s = LightweightStore::new(0);
        s.apply_event(&watcher::Event::Apply(lw_obj("a", None, 10)));
        assert_eq!(s.tier_of(&("a".into(), None)), Tier::Large);
    }

    #[test]
    fn lightweight_store_init_cycle_swaps_atomically() {
        // Seed SmallTier with a stale object, then relist: Init → InitApply(new
        // set) → InitDone must replace the store, dropping the stale object.
        let mut s = LightweightStore::new(1024);
        s.apply_event(&watcher::Event::Apply(lw_obj("stale", None, 10)));
        assert_eq!(s.tier_of(&("stale".into(), None)), Tier::Small);

        s.apply_event(&watcher::Event::Init);
        s.apply_event(&watcher::Event::InitApply(lw_obj("fresh", None, 10)));
        // During the Init cycle the old store is still visible (readers see the
        // swapped-in store, not the buffer): stale remains until InitDone.
        assert_eq!(s.tier_of(&("stale".into(), None)), Tier::Small);
        assert_eq!(s.tier_of(&("fresh".into(), None)), Tier::Absent);

        s.apply_event(&watcher::Event::InitDone);
        // After InitDone the buffer is swapped in: stale is gone, fresh is live.
        assert_eq!(s.tier_of(&("stale".into(), None)), Tier::Absent);
        assert_eq!(s.tier_of(&("fresh".into(), None)), Tier::Small);
    }

    #[test]
    fn lightweight_store_init_cycle_routes_large_in_buffer() {
        // A large object applied via InitApply lands in LargeTier after InitDone.
        let mut s = LightweightStore::new(100);
        s.apply_event(&watcher::Event::Init);
        s.apply_event(&watcher::Event::InitApply(lw_obj("big", None, 10_000)));
        s.apply_event(&watcher::Event::InitDone);
        assert_eq!(s.tier_of(&("big".into(), None)), Tier::Large);
    }
}
