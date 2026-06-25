//! Drift detection: compare the live cluster state of Git-managed resources
//! against the desired manifests and report differences.
//!
//! Per the memory strategy this is done with periodic `List` calls (one per
//! resource kind), not `Watch`. Comparison is a subset check: the Git manifest
//! is considered drifted when any of its declared fields diverge in the live
//! object (server-injected defaults are tolerated).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use kube::core::DynamicObject;
use serde_json::Value;

use crate::config::Config;
use crate::error::Result;
use crate::kube_util;
use crate::manifest::RawManifest;
use crate::prune::ResourceKey;
use crate::watch::{LightweightStore, ObjKey, Tier};

/// A single detected drift.
#[derive(Debug, Clone)]
pub struct DriftItem {
    pub key: ResourceKey,
    pub reason: String,
}

/// Detect drift across all manifest kinds.
pub async fn detect(
    client: &kube::client::Client,
    manifests: &[RawManifest],
    cfg: &Config,
) -> Result<Vec<DriftItem>> {
    // Collect the distinct GVKs to list (insertion order does not matter here).
    let mut gvks: HashMap<(String, String, String), ()> = HashMap::new();
    for m in manifests {
        gvks.entry(m.gvk()).or_insert(());
    }

    let label_sel = format!("{}={}", cfg.managed_label_key, cfg.managed_label_value);
    // Owned list results, kept alive so the reference map below can borrow them
    // (compute_drifts works on `&DynamicObject`, so the lists are not cloned).
    let mut lists: HashMap<(String, String, String), Vec<DynamicObject>> = HashMap::new();

    for (group, version, kind) in gvks.into_keys() {
        let (ar, _caps) = match kube_util::resolve(client, &group, &version, &kind).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, gvk = ?(&group, &version, &kind), "drift: discovery failed");
                continue;
            }
        };
        // List across ALL namespaces (BUG 5): a resource Lean CD applied in a
        // namespace other than cfg.namespace must still be drift-checked.
        let live = match kube_util::list_all(client, &ar, Some(&label_sel)).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, gvk = ?(&group, &version, &kind), "drift: list failed");
                continue;
            }
        };
        lists.insert((group, version, kind), live);
    }

    let mut live_by_gvk: HashMap<(String, String, String), Vec<&DynamicObject>> = HashMap::new();
    for (gvk, objs) in &lists {
        live_by_gvk.insert(gvk.clone(), objs.iter().collect());
    }

    Ok(compute_drifts(manifests, &live_by_gvk))
}

/// Compare desired manifests against live objects (grouped by GVK) and return
/// the drifts. Pure: no API calls. `live_by_gvk` is keyed by the
/// `(group, version, kind)` triple from [`RawManifest::gvk`]; a missing key is
/// treated as "nothing live for this kind", so every manifest of that kind is
/// reported as missing.
pub fn compute_drifts(
    manifests: &[RawManifest],
    live_by_gvk: &HashMap<(String, String, String), Vec<&DynamicObject>>,
) -> Vec<DriftItem> {
    let empty: Vec<&DynamicObject> = Vec::new();
    let mut drifts = Vec::new();
    for m in manifests {
        let live = live_by_gvk.get(&m.gvk()).unwrap_or(&empty);
        match find_live_match(m, live) {
            None => drifts.push(DriftItem {
                key: ResourceKey::from_manifest(m),
                reason: "missing in cluster".to_string(),
            }),
            Some(lo) => {
                // Compare the Git manifest against the *whole* live object
                // (re-serialized, so apiVersion/kind from `types` and metadata
                // are present). `DynamicObject.data` is `#[serde(flatten)]` and
                // holds only the fields left after apiVersion/kind/metadata are
                // peeled off into `types`/`metadata`, so comparing `m.data`
                // (which carries apiVersion/kind) against `lo.data` alone makes
                // every spec-less resource (ConfigMap, Namespace, …) report
                // drift forever.
                let live_value = serde_json::to_value(lo).unwrap_or(serde_json::Value::Null);
                if specs_differ(&manifest_value(m), &live_value) {
                    drifts.push(DriftItem {
                        key: ResourceKey::from_manifest(m),
                        reason: "spec differs from desired state".to_string(),
                    });
                }
            }
        }
    }
    drifts
}

/// GVK → manifests whose live object is LargeTier (deferred to a List fallback).
type LargeByGvk<'a> = HashMap<(String, String, String), Vec<&'a RawManifest>>;

/// First pass of cache-mode drift detection: subset-check SmallTier entries
/// straight from the cache (no clone, no API), report Absent keys as missing,
/// and collect LargeTier entries (body not cached) for a per-GVK `List`
/// fallback. Pure w.r.t. the API. Returns `(drifts, large_by_gvk)`.
fn drift_from_cache<'a>(
    manifests: &'a [RawManifest],
    stores: &HashMap<(String, String, String), Arc<Mutex<LightweightStore>>>,
) -> (Vec<DriftItem>, LargeByGvk<'a>) {
    let mut by_gvk: HashMap<(String, String, String), Vec<&RawManifest>> = HashMap::new();
    for m in manifests {
        by_gvk.entry(m.gvk()).or_default().push(m);
    }

    let mut drifts: Vec<DriftItem> = Vec::new();
    let mut large_by_gvk: LargeByGvk<'_> = HashMap::new();

    for (gvk, ms) in &by_gvk {
        let store = match stores.get(gvk) {
            Some(s) => s,
            None => {
                // No cache for this GVK yet (watch not built): report every
                // manifest of this kind missing, so the pass re-applies and the
                // store populates on the next watch event. Same absent-GVK
                // behavior as the former cache-mode path.
                for m in ms {
                    drifts.push(DriftItem {
                        key: ResourceKey::from_manifest(m),
                        reason: "missing in cluster".to_string(),
                    });
                }
                continue;
            }
        };
        let guard = match store.lock() {
            Ok(g) => g,
            Err(_) => {
                tracing::warn!(gvk = ?gvk, "drift: cache store poisoned; treating as missing");
                for m in ms {
                    drifts.push(DriftItem {
                        key: ResourceKey::from_manifest(m),
                        reason: "cache unavailable".to_string(),
                    });
                }
                continue;
            }
        };
        for m in ms {
            let key: ObjKey = (m.name.clone(), m.namespace.clone());
            match guard.tier_of(&key) {
                Tier::Small => match guard.small_get(&key) {
                    Some(lo) => {
                        let live_value = serde_json::to_value(lo).unwrap_or(Value::Null);
                        if specs_differ(&manifest_value(m), &live_value) {
                            drifts.push(DriftItem {
                                key: ResourceKey::from_manifest(m),
                                reason: "spec differs from desired state".to_string(),
                            });
                        }
                    }
                    None => drifts.push(DriftItem {
                        key: ResourceKey::from_manifest(m),
                        reason: "missing in cluster".to_string(),
                    }),
                },
                Tier::Large => {
                    large_by_gvk.entry(gvk.clone()).or_default().push(*m);
                }
                Tier::Absent => drifts.push(DriftItem {
                    key: ResourceKey::from_manifest(m),
                    reason: "missing in cluster".to_string(),
                }),
            }
        }
    }

    (drifts, large_by_gvk)
}

/// Cache-mode drift detection against `LightweightStore`s. SmallTier entries
/// are subset-checked straight from the cache (no clone, no per-pass `List`);
/// Absent keys are drift; LargeTier keys (body not cached) fall back to a
/// per-GVK `List`. This is the cache-mode counterpart of [`detect`]; both
/// reuse [`specs_differ`] and the internal `spec_subset` helper.
pub async fn detect_from_lw(
    client: &kube::client::Client,
    manifests: &[RawManifest],
    stores: &HashMap<(String, String, String), Arc<Mutex<LightweightStore>>>,
    cfg: &Config,
) -> Result<Vec<DriftItem>> {
    let (mut drifts, large_by_gvk) = drift_from_cache(manifests, stores);
    if large_by_gvk.is_empty() {
        return Ok(drifts);
    }

    let label_sel = format!("{}={}", cfg.managed_label_key, cfg.managed_label_value);
    let mut discovery = kube_util::DiscoveryCache::new();

    // LargeTier fallback: one List per GVK that had a LargeTier entry, then
    // subset-check only those manifests against the fresh live set.
    for (gvk, ms) in &large_by_gvk {
        let (group, version, kind) = gvk;
        let (ar, _caps) = match discovery.get_or_resolve(client, group, version, kind).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, gvk = ?gvk, "drift: large-tier discovery failed");
                continue;
            }
        };
        let live = match kube_util::list_all(client, &ar, Some(&label_sel)).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, gvk = ?gvk, "drift: large-tier List fallback failed");
                continue;
            }
        };
        let live_refs: Vec<&DynamicObject> = live.iter().collect();
        for m in ms {
            match find_live_match(m, &live_refs) {
                None => drifts.push(DriftItem {
                    key: ResourceKey::from_manifest(m),
                    reason: "missing in cluster".to_string(),
                }),
                Some(lo) => {
                    let live_value = serde_json::to_value(lo).unwrap_or(Value::Null);
                    if specs_differ(&manifest_value(m), &live_value) {
                        drifts.push(DriftItem {
                            key: ResourceKey::from_manifest(m),
                            reason: "spec differs from desired state".to_string(),
                        });
                    }
                }
            }
        }
    }

    Ok(drifts)
}

/// Find the live object that matches a manifest by name and namespace.
pub fn find_live_match<'a>(
    m: &RawManifest,
    live: &'a [&DynamicObject],
) -> Option<&'a DynamicObject> {
    live.iter().copied().find(|o| {
        o.metadata.name.as_deref() == Some(m.name.as_str())
            && o.metadata.namespace.as_deref() == m.namespace.as_deref()
    })
}

/// Deserialize a manifest's stored YAML bytes into a `Value` for comparison.
fn manifest_value(m: &RawManifest) -> Value {
    serde_yaml::from_slice(&m.data).unwrap_or(Value::Null)
}

/// True when the live object does not satisfy the Git-declared fields.
///
/// `apiVersion`/`kind` are stripped from both sides before comparing: the Git
/// manifest carries them, but the live `DynamicObject`'s `types` (where kube-rs
/// parks them) is often `None` on `List`, so they are an unreliable comparison
/// key and would make every spec-less resource drift forever.
pub fn specs_differ(git: &Value, live: &Value) -> bool {
    // Strip `stringData` before comparing: k8s converts Secret `stringData`
    // → base64 `data` on apply, so Git carries stringData while live carries
    // data, and a raw comparison would drift forever. Only `data` (and the
    // rest) is compared, tolerating either side holding what the other stores
    // under the opposite key. (BUG 9.) Only Secrets use `stringData`, so this
    // is a no-op for other kinds.
    let git = remove_top_level_keys(git, &["apiVersion", "kind", "stringData"]);
    let live = remove_top_level_keys(live, &["apiVersion", "kind", "stringData"]);
    let git_spec = git.get("spec");
    let live_spec = live.get("spec");
    match (git_spec, live_spec) {
        (Some(g), Some(l)) => !spec_subset(g, l),
        (Some(_), None) => true,
        // No spec to compare on the Git side: fall back to whole-object subset
        // so label/annotation drift on spec-less resources is still caught.
        (None, _) => !spec_subset(&git, &live),
    }
}

/// Return a copy of `v` with the given top-level keys removed. Non-objects are
/// returned unchanged (cloned). Used to strip type fields (`apiVersion`/`kind`)
/// and Secret `stringData` before drift comparison.
fn remove_top_level_keys(v: &Value, keys: &[&str]) -> Value {
    let mut obj = match v.as_object() {
        Some(o) => o.clone(),
        None => return v.clone(),
    };
    for k in keys {
        obj.remove(*k);
    }
    Value::Object(obj)
}

/// True for k8s field values the API server elides (omits from responses)
/// because they are the type's zero value: boolean `false`, `null`, empty
/// slices `[]`, and the number `0` (int32/float fields with omitempty, e.g.
/// `livenessProbe.initialDelaySeconds: 0`). An explicit zero value in Git
/// therefore compares equal to the field being absent in live. Other zeroes —
/// `""`, `{}` — are NOT elided by k8s and are not treated as zero here. Note
/// `replicas: 0` is unaffected: when live carries the key the scalar compare
/// still runs (`0 != 1` is drift). (BUG 9.)
fn is_k8s_zero_value(v: &Value) -> bool {
    matches!(v, Value::Bool(false) | Value::Null)
        || v.as_array().is_some_and(|a| a.is_empty())
        || v.as_f64().is_some_and(|n| n == 0.0)
}

/// Recursive subset check: `git` is satisfied by `live` when every key in `git`
/// is present and recursively satisfied in `live`. Extra keys in `live` (server
/// defaults) are tolerated.
fn spec_subset(git: &Value, live: &Value) -> bool {
    match (git, live) {
        (Value::Object(g), Value::Object(l)) => g.iter().all(|(k, v)| {
            // k8s omits zero-value fields — booleans (`hostNetwork`, `hostPID`,
            // `privileged`, …) and empty slices (`httpGet.httpHeaders: []`,
            // `env: []`, …). An explicit zero value in Git is equivalent to
            // the field being absent in live, so don't flag drift over a
            // server-elided zero value. Only `false`/`[]` — non-zero values
            // are always meaningful and must be present in live. (BUG 9.)
            if is_k8s_zero_value(v) && !l.contains_key(k) {
                return true;
            }
            l.get(k).is_some_and(|lv| spec_subset(v, lv))
        }),
        // Arrays: index-aligned recursive subset. Lean CD applies the array
        // itself via SSA, so live preserves Git's element order; extra trailing
        // live elements (server-injected defaults) are tolerated, and each Git
        // element must be a subset of the live element at the same index.
        (Value::Array(g), Value::Array(l)) => {
            g.len() <= l.len() && g.iter().enumerate().all(|(i, gv)| spec_subset(gv, &l[i]))
        }
        _ => git == live,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn subset_allows_server_defaults() {
        let git = json!({"replicas": 1, "image": "nginx"});
        let live = json!({"replicas": 1, "image": "nginx", "injected": true});
        assert!(spec_subset(&git, &live));
    }

    #[test]
    fn subset_detects_value_change() {
        let git = json!({"replicas": 1});
        let live = json!({"replicas": 2});
        assert!(!spec_subset(&git, &live));
    }

    #[test]
    fn subset_detects_missing_key() {
        let git = json!({"replicas": 1, "selector": {}});
        let live = json!({"replicas": 1});
        assert!(!spec_subset(&git, &live));
    }

    #[test]
    fn subset_tolerates_git_false_vs_live_omitted() {
        // k8s omits zero-value bool fields (hostNetwork, hostPID, privileged,
        // …). An explicit `false` in Git is equivalent to the field being
        // absent in live, so it must not read as drift. (BUG 9: the KSM
        // Deployment carries `hostNetwork: false`; the server omits it, and
        // every drift pass re-applied the Deployment forever.)
        let git = json!({"hostNetwork": false, "containers": [{"name": "c", "image": "i"}]});
        let live = json!({"containers": [{"name": "c", "image": "i"}]});
        assert!(spec_subset(&git, &live));
    }

    #[test]
    fn subset_tolerates_git_empty_array_vs_live_omitted() {
        // k8s omits empty slices (httpGet.httpHeaders: [], env: [], …). An
        // explicit `[]` in Git is equivalent to the field being absent in live.
        // (BUG 9: KSM/node-exporter livenessProbe.httpGet.httpHeaders: [].)
        let git = json!({"httpHeaders": [], "port": "http"});
        let live = json!({"port": "http"});
        assert!(spec_subset(&git, &live));
    }

    #[test]
    fn subset_tolerates_git_null_vs_live_omitted() {
        // k8s omits null fields (httpGet.httpHeaders: null, …). An explicit
        // null in Git is equivalent to the field being absent in live.
        // (BUG 9: KSM/node-exporter livenessProbe.httpGet.httpHeaders: null.)
        let git = json!({"httpHeaders": null, "port": "http"});
        let live = json!({"port": "http"});
        assert!(spec_subset(&git, &live));
    }

    #[test]
    fn subset_tolerates_git_zero_number_vs_live_omitted() {
        // k8s omits zero-value integer fields (livenessProbe.initialDelaySeconds,
        // periodSeconds, timeoutSeconds, … — int32 with omitempty). An explicit
        // `0` in Git is equivalent to the field being absent in live.
        // (BUG 9: node-exporter livenessProbe.initialDelaySeconds: 0.)
        let git = json!({"initialDelaySeconds": 0, "port": "http"});
        let live = json!({"port": "http"});
        assert!(spec_subset(&git, &live));
    }

    #[test]
    fn subset_still_detects_nonzero_number_mismatch() {
        // Guard: a non-zero number in Git must still match live (0 vs 1 is drift
        // when live actually carries the key).
        let git = json!({"replicas": 0});
        let live = json!({"replicas": 1});
        assert!(!spec_subset(&git, &live));
    }

    #[test]
    fn specs_differ_secret_stringdata_vs_live_data_no_drift() {
        // k8s converts Secret `stringData` → base64 `data` on apply. Git
        // carries stringData, live carries data; comparing them raw would
        // drift forever. (BUG 9: vmalertmanager-vmks Secret.)
        let git = json!({
            "kind": "Secret",
            "metadata": {"name": "s"},
            "stringData": {"alertmanager.yaml": "receivers: []\n"}
        });
        let live = json!({
            "kind": "Secret",
            "metadata": {"name": "s"},
            "data": {"alertmanager.yaml": "cmVjZWl2ZXJzOiBbXQo="}
        });
        assert!(!specs_differ(&git, &live));
    }

    // --- arrays: a Git element that is a subset of the live element (server
    //     injects resources/imagePullPolicy/ports[].protocol/…) must NOT drift. ---

    #[test]
    fn subset_allows_server_defaults_in_array_elements() {
        let git = json!({"containers": [{"name": "a", "image": "x"}]});
        let live = json!({"containers": [{"name": "a", "image": "x", "resources": {}, "imagePullPolicy": "IfNotPresent"}]});
        assert!(spec_subset(&git, &live));
    }

    #[test]
    fn subset_detects_missing_array_element() {
        // Fewer live elements than Git declares → drift.
        let git = json!({"containers": [{"name": "a"}, {"name": "b"}]});
        let live = json!({"containers": [{"name": "a"}]});
        assert!(!spec_subset(&git, &live));
    }

    #[test]
    fn subset_detects_value_change_in_array_element() {
        let git = json!({"containers": [{"name": "a", "image": "x"}]});
        let live = json!({"containers": [{"name": "a", "image": "y"}]});
        assert!(!spec_subset(&git, &live));
    }

    #[test]
    fn subset_allows_extra_trailing_live_array_elements() {
        // Extra trailing live elements are tolerated (server may append).
        let git = json!({"init": [{"name": "a"}]});
        let live = json!({"init": [{"name": "a"}, {"name": "b"}]});
        assert!(spec_subset(&git, &live));
    }

    #[test]
    fn specs_differ_when_live_lacks_spec() {
        let git = json!({"spec": {"port": 80}});
        let live = json!({});
        assert!(specs_differ(&git, &live));
    }

    #[test]
    fn specs_match_when_subset() {
        let git = json!({"spec": {"port": 80}});
        let live = json!({"spec": {"port": 80, "targetPort": 8080}});
        assert!(!specs_differ(&git, &live));
    }

    // --- find_live_match / compute_drifts: pure logic, no API needed ---

    fn test_manifest(
        group: &str,
        kind: &str,
        name: &str,
        namespace: Option<&str>,
        spec: Value,
    ) -> RawManifest {
        let api_version = if group.is_empty() {
            "v1".to_string()
        } else {
            format!("{group}/v1")
        };
        let mut value = json!({
            "apiVersion": api_version,
            "kind": kind,
            "metadata": { "name": name },
            "spec": spec,
        });
        if let Some(ns) = namespace {
            value["metadata"]["namespace"] = json!(ns);
        }
        let data = serde_yaml::to_string(&value).unwrap().into_bytes();
        RawManifest {
            group: group.to_string(),
            version: "v1".to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            namespace: namespace.map(String::from),
            data,
            annotations: std::collections::BTreeMap::new(),
        }
    }

    fn dyn_obj(name: &str, namespace: Option<&str>, spec: Value) -> DynamicObject {
        let mut v = json!({
            "apiVersion": "v1",
            "kind": "TestKind",
            "metadata": { "name": name },
            "spec": spec,
        });
        if let Some(ns) = namespace {
            v["metadata"]["namespace"] = json!(ns);
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn find_live_match_by_name_and_namespace() {
        let m = test_manifest(
            "apps",
            "Deployment",
            "d",
            Some("ns"),
            json!({"replicas": 1}),
        );
        let o1 = dyn_obj("other", Some("ns"), json!({}));
        let o2 = dyn_obj("d", Some("ns"), json!({}));
        let live = vec![&o1, &o2];
        let found = find_live_match(&m, &live).expect("should match");
        assert_eq!(found.metadata.name.as_deref(), Some("d"));
    }

    #[test]
    fn find_live_match_returns_none_when_absent() {
        let m = test_manifest("apps", "Deployment", "d", Some("ns"), json!({}));
        let o = dyn_obj("x", Some("ns"), json!({}));
        let live = vec![&o];
        assert!(find_live_match(&m, &live).is_none());
    }

    #[test]
    fn find_live_match_respects_namespace() {
        let m = test_manifest("", "ConfigMap", "c", Some("a"), json!({}));
        let o = dyn_obj("c", Some("b"), json!({}));
        let live = vec![&o];
        assert!(find_live_match(&m, &live).is_none());
    }

    #[test]
    fn find_live_match_handles_cluster_scoped() {
        let m = test_manifest("", "Namespace", "n", None, json!({}));
        let o = dyn_obj("n", None, json!({}));
        let live = vec![&o];
        assert!(find_live_match(&m, &live).is_some());
    }

    #[test]
    fn compute_drifts_flags_missing_resource() {
        let m = test_manifest(
            "apps",
            "Deployment",
            "d",
            Some("ns"),
            json!({"replicas": 1}),
        );
        let live: HashMap<(String, String, String), Vec<&DynamicObject>> = HashMap::new();
        let drifts = compute_drifts(&[m], &live);
        assert_eq!(drifts.len(), 1);
        assert!(drifts[0].reason.contains("missing"));
        assert_eq!(drifts[0].key.kind, "Deployment");
    }

    #[test]
    fn compute_drifts_flags_spec_difference() {
        let m = test_manifest(
            "apps",
            "Deployment",
            "d",
            Some("ns"),
            json!({"replicas": 1}),
        );
        let live_obj = dyn_obj("d", Some("ns"), json!({"replicas": 2}));
        let mut live: HashMap<(String, String, String), Vec<&DynamicObject>> = HashMap::new();
        live.insert(
            ("apps".into(), "v1".into(), "Deployment".into()),
            vec![&live_obj],
        );
        let drifts = compute_drifts(&[m], &live);
        assert_eq!(drifts.len(), 1);
        assert!(drifts[0].reason.contains("differ"));
    }

    #[test]
    fn compute_drifts_empty_when_live_is_superset() {
        let m = test_manifest(
            "apps",
            "Deployment",
            "d",
            Some("ns"),
            json!({"replicas": 1}),
        );
        let live_obj = dyn_obj("d", Some("ns"), json!({"replicas": 1, "extra": true}));
        let mut live: HashMap<(String, String, String), Vec<&DynamicObject>> = HashMap::new();
        live.insert(
            ("apps".into(), "v1".into(), "Deployment".into()),
            vec![&live_obj],
        );
        let drifts = compute_drifts(&[m], &live);
        assert!(
            drifts.is_empty(),
            "no drift when live is a superset of desired"
        );
    }

    #[test]
    fn compute_drifts_across_multiple_gvks() {
        let m1 = test_manifest("", "ConfigMap", "c", Some("ns"), json!({}));
        let m2 = test_manifest(
            "apps",
            "Deployment",
            "d",
            Some("ns"),
            json!({"replicas": 1}),
        );
        let cm = dyn_obj("c", Some("ns"), json!({}));
        let mut live: HashMap<(String, String, String), Vec<&DynamicObject>> = HashMap::new();
        live.insert(("".into(), "v1".into(), "ConfigMap".into()), vec![&cm]);
        // Deployment has no live entry -> missing
        let drifts = compute_drifts(&[m1, m2], &live);
        assert_eq!(drifts.len(), 1);
        assert_eq!(drifts[0].key.kind, "Deployment");
    }

    #[test]
    fn compute_drifts_distinguishes_namespaces() {
        // Same kind+name in two namespaces; live must be matched per-namespace
        // so only the drifted namespace is reported. This guards BUG 5: detect()
        // must list across all namespaces so both objects reach compute_drifts.
        let m_a = test_manifest("", "ConfigMap", "c", Some("ns1"), json!({"v": 1}));
        let m_b = test_manifest("", "ConfigMap", "c", Some("ns2"), json!({"v": 1}));
        let c1 = dyn_obj("c", Some("ns1"), json!({"v": 1}));
        let c2 = dyn_obj("c", Some("ns2"), json!({"v": 2}));
        let mut live: HashMap<(String, String, String), Vec<&DynamicObject>> = HashMap::new();
        live.insert(("".into(), "v1".into(), "ConfigMap".into()), vec![&c1, &c2]);
        let drifts = compute_drifts(&[m_a, m_b], &live);
        assert_eq!(drifts.len(), 1, "only ns2 should differ");
        assert_eq!(drifts[0].key.namespace.as_deref(), Some("ns2"));
    }

    // --- remove_top_level_keys: the shared strip helper ---

    #[test]
    fn remove_top_level_keys_removes_named_keys() {
        let v = json!({"a": 1, "b": 2, "c": 3});
        assert_eq!(remove_top_level_keys(&v, &["a", "c"]), json!({"b": 2}));
    }

    #[test]
    fn remove_top_level_keys_no_op_on_non_object() {
        assert_eq!(remove_top_level_keys(&json!(42), &["a"]), json!(42));
        assert_eq!(remove_top_level_keys(&json!("x"), &["a"]), json!("x"));
        assert_eq!(remove_top_level_keys(&json!([1, 2]), &["a"]), json!([1, 2]));
        assert_eq!(remove_top_level_keys(&Value::Null, &["a"]), Value::Null);
    }

    #[test]
    fn remove_top_level_keys_missing_key_is_noop() {
        let v = json!({"a": 1, "b": 2});
        assert_eq!(remove_top_level_keys(&v, &["zzz"]), json!({"a": 1, "b": 2}));
    }

    #[test]
    fn remove_top_level_keys_empty_slice_is_clone() {
        let v = json!({"a": 1, "b": 2});
        assert_eq!(remove_top_level_keys(&v, &[]), json!({"a": 1, "b": 2}));
    }

    // --- drift_from_cache: LightweightStore Small/Large/Absent routing ---

    /// A `LightweightStore` seeded with `objs` (applied), wrapped for sharing.
    fn lw_store(max_bytes: usize, objs: Vec<DynamicObject>) -> Arc<Mutex<LightweightStore>> {
        use kube::runtime::watcher;
        let mut s = LightweightStore::new(max_bytes);
        for o in objs {
            s.apply_event(&watcher::Event::Apply(o));
        }
        Arc::new(Mutex::new(s))
    }

    fn lw_stores_with(
        kind: &str,
        store: Arc<Mutex<LightweightStore>>,
    ) -> HashMap<(String, String, String), Arc<Mutex<LightweightStore>>> {
        let mut stores = HashMap::new();
        stores.insert(("".to_string(), "v1".to_string(), kind.to_string()), store);
        stores
    }

    #[test]
    fn drift_from_cache_small_match_no_drift() {
        let stores = lw_stores_with(
            "ConfigMap",
            lw_store(
                1024,
                vec![dyn_obj("c", Some("ns"), json!({"data": {"k": "1"}}))],
            ),
        );
        let m = test_manifest(
            "",
            "ConfigMap",
            "c",
            Some("ns"),
            json!({"data": {"k": "1"}}),
        );
        let manifests = [m];
        let (drifts, large) = drift_from_cache(&manifests, &stores);
        assert!(drifts.is_empty(), "matching SmallTier → no drift");
        assert!(large.is_empty(), "no LargeTier entries");
    }

    #[test]
    fn drift_from_cache_small_diff_is_drift() {
        let stores = lw_stores_with(
            "ConfigMap",
            lw_store(
                1024,
                vec![dyn_obj("c", Some("ns"), json!({"data": {"k": "2"}}))],
            ),
        );
        let m = test_manifest(
            "",
            "ConfigMap",
            "c",
            Some("ns"),
            json!({"data": {"k": "1"}}),
        );
        let manifests = [m];
        let (drifts, large) = drift_from_cache(&manifests, &stores);
        assert_eq!(drifts.len(), 1);
        assert!(drifts[0].reason.contains("differ"));
        assert!(large.is_empty());
    }

    #[test]
    fn drift_from_cache_absent_is_missing() {
        let stores = lw_stores_with("ConfigMap", lw_store(1024, vec![]));
        let m = test_manifest(
            "",
            "ConfigMap",
            "c",
            Some("ns"),
            json!({"data": {"k": "1"}}),
        );
        let manifests = [m];
        let (drifts, large) = drift_from_cache(&manifests, &stores);
        assert_eq!(drifts.len(), 1);
        assert!(drifts[0].reason.contains("missing"));
        assert!(large.is_empty());
    }

    #[test]
    fn drift_from_cache_large_deferred_to_list() {
        // A large object is tracked by key only → deferred to List fallback,
        // not drift-checked in this pass.
        let stores = lw_stores_with(
            "ConfigMap",
            lw_store(
                100,
                vec![dyn_obj(
                    "c",
                    Some("ns"),
                    json!({"payload": "x".repeat(1000)}),
                )],
            ),
        );
        let m = test_manifest(
            "",
            "ConfigMap",
            "c",
            Some("ns"),
            json!({"data": {"payload": "y"}}),
        );
        let manifests = [m];
        let (drifts, large) = drift_from_cache(&manifests, &stores);
        assert!(drifts.is_empty(), "LargeTier not checked in this pass");
        assert_eq!(large.len(), 1, "LargeTier entry deferred for List fallback");
    }

    #[test]
    fn drift_from_cache_no_store_for_gvk_is_missing() {
        // No cache entry for the manifest's GVK → all manifests missing.
        let stores: HashMap<(String, String, String), Arc<Mutex<LightweightStore>>> =
            HashMap::new();
        let m = test_manifest(
            "",
            "ConfigMap",
            "c",
            Some("ns"),
            json!({"data": {"k": "1"}}),
        );
        let manifests = [m];
        let (drifts, _large) = drift_from_cache(&manifests, &stores);
        assert_eq!(drifts.len(), 1);
        assert!(drifts[0].reason.contains("missing"));
    }

    // --- integration: parse → classify → LightweightStore → drift_from_cache ---
    // These exercise the cache-mode drift path's module seams end-to-end. The
    // unit tests above build `RawManifest`s directly; these run the real parse
    // + hook-split + tier-routing pipeline. No API server, no `kind` cluster.
    use crate::hooks::classify;
    use crate::manifest::parse_str;

    /// A live ConfigMap-style `DynamicObject` used to seed a cache store as the
    /// "live" state the watch driver would apply.
    fn live_cm(name: &str, ns: &str, data: &str) -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": { "name": name, "namespace": ns },
            "data": { "payload": data },
        }))
        .expect("live ConfigMap JSON parses into a DynamicObject")
    }

    #[test]
    fn pipeline_small_match_reports_no_drift() {
        let yaml = "\
apiVersion: v1
kind: ConfigMap
metadata:
  name: small
  namespace: default
data:
  payload: hi
";
        let main = classify(parse_str(yaml).unwrap()).main;
        assert_eq!(main.len(), 1, "the ConfigMap is the only main resource");
        let stores = lw_stores_with(
            "ConfigMap",
            lw_store(4096, vec![live_cm("small", "default", "hi")]),
        );
        let (drifts, large) = drift_from_cache(&main, &stores);
        assert!(drifts.is_empty(), "matching SmallTier cache → no drift");
        assert!(large.is_empty());
    }

    #[test]
    fn pipeline_small_diff_is_drift() {
        let yaml = "\
apiVersion: v1
kind: ConfigMap
metadata:
  name: small
  namespace: default
data:
  payload: hi
";
        let main = classify(parse_str(yaml).unwrap()).main;
        let stores = lw_stores_with(
            "ConfigMap",
            lw_store(4096, vec![live_cm("small", "default", "bye")]),
        );
        let (drifts, large) = drift_from_cache(&main, &stores);
        assert_eq!(drifts.len(), 1);
        assert!(drifts[0].reason.contains("differ"));
        assert!(large.is_empty());
    }

    #[test]
    fn pipeline_large_live_object_deferred_to_list() {
        // A large *live* object exceeds the threshold → LargeTier, deferred to
        // the per-GVK List fallback rather than drift-checked in this pass.
        let yaml = "\
apiVersion: v1
kind: ConfigMap
metadata:
  name: big
  namespace: default
data:
  payload: git
";
        let main = classify(parse_str(yaml).unwrap()).main;
        let stores = lw_stores_with(
            "ConfigMap",
            lw_store(1024, vec![live_cm("big", "default", &"x".repeat(100_000))]),
        );
        let (drifts, large) = drift_from_cache(&main, &stores);
        assert!(
            drifts.is_empty(),
            "LargeTier is not drift-checked in this pass"
        );
        assert_eq!(large.len(), 1, "LargeTier key deferred for List fallback");
    }

    #[test]
    fn pipeline_hook_is_classified_out_of_main() {
        // A pre-install hook is split into the `pre` phase, so only the
        // non-hook ConfigMap reaches drift_from_cache.
        let yaml = "\
apiVersion: v1
kind: ConfigMap
metadata:
  name: real
  namespace: default
data:
  payload: real
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: hook-cm
  namespace: default
  annotations:
    helm.sh/hook: pre-install
data:
  payload: hook
";
        let classified = classify(parse_str(yaml).unwrap());
        assert!(
            !classified.pre.is_empty(),
            "the pre-install hook lands in `pre`"
        );
        assert_eq!(classified.main.len(), 1);
        assert_eq!(classified.main[0].name, "real");
        let stores = lw_stores_with(
            "ConfigMap",
            lw_store(4096, vec![live_cm("real", "default", "real")]),
        );
        let (drifts, large) = drift_from_cache(&classified.main, &stores);
        assert!(drifts.is_empty());
        assert!(large.is_empty());
    }
}
