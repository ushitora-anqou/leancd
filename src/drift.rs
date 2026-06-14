//! Drift detection: compare the live cluster state of Git-managed resources
//! against the desired manifests and report differences.
//!
//! Per the memory strategy this is done with periodic `List` calls (one per
//! resource kind), not `Watch`. Comparison is a subset check: the Git manifest
//! is considered drifted when any of its declared fields diverge in the live
//! object (server-injected defaults are tolerated).

use std::collections::HashMap;

use kube::core::DynamicObject;
use serde_json::Value;

use crate::config::Config;
use crate::error::Result;
use crate::kube_util;
use crate::manifest::RawManifest;
use crate::prune::ResourceKey;

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
    let mut live_by_gvk: HashMap<(String, String, String), Vec<DynamicObject>> = HashMap::new();

    for (group, version, kind) in gvks.into_keys() {
        let (ar, caps) = match kube_util::resolve(client, &group, &version, &kind).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, gvk = ?(&group, &version, &kind), "drift: discovery failed");
                continue;
            }
        };
        let live = match kube_util::list(
            client,
            &ar,
            &caps.scope,
            None,
            &cfg.namespace,
            Some(&label_sel),
        )
        .await
        {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, gvk = ?(&group, &version, &kind), "drift: list failed");
                continue;
            }
        };
        live_by_gvk.insert((group, version, kind), live);
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
    live_by_gvk: &HashMap<(String, String, String), Vec<DynamicObject>>,
) -> Vec<DriftItem> {
    let empty: Vec<DynamicObject> = Vec::new();
    let mut drifts = Vec::new();
    for m in manifests {
        let live = live_by_gvk.get(&m.gvk()).unwrap_or(&empty);
        match find_live_match(m, live) {
            None => drifts.push(DriftItem {
                key: ResourceKey::from_manifest(m),
                reason: "missing in cluster".to_string(),
            }),
            Some(lo) => {
                if specs_differ(&m.data, &lo.data) {
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

/// Find the live object that matches a manifest by name and namespace.
pub fn find_live_match<'a>(
    m: &RawManifest,
    live: &'a [DynamicObject],
) -> Option<&'a DynamicObject> {
    live.iter().find(|o| {
        o.metadata.name.as_deref() == Some(m.name.as_str())
            && o.metadata.namespace.as_deref() == m.namespace.as_deref()
    })
}

/// True when the live object does not satisfy the Git-declared fields.
pub fn specs_differ(git: &Value, live: &Value) -> bool {
    let git_spec = git.get("spec");
    let live_spec = live.get("spec");
    match (git_spec, live_spec) {
        (Some(g), Some(l)) => !spec_subset(g, l),
        (Some(_), None) => true,
        // No spec to compare on the Git side: fall back to whole-object subset
        // so label/annotation drift on spec-less resources is still caught.
        (None, _) => !spec_subset(git, live),
    }
}

/// Recursive subset check: `git` is satisfied by `live` when every key in `git`
/// is present and recursively satisfied in `live`. Extra keys in `live` (server
/// defaults) are tolerated.
fn spec_subset(git: &Value, live: &Value) -> bool {
    match (git, live) {
        (Value::Object(g), Value::Object(l)) => g
            .iter()
            .all(|(k, v)| l.get(k).is_some_and(|lv| spec_subset(v, lv))),
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
        let mut data = json!({
            "kind": kind,
            "metadata": { "name": name },
            "spec": spec,
        });
        if let Some(ns) = namespace {
            data["metadata"]["namespace"] = json!(ns);
        }
        RawManifest {
            group: group.to_string(),
            version: "v1".to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            namespace: namespace.map(String::from),
            data,
        }
    }

    fn dyn_obj(name: &str, namespace: Option<&str>, spec: Value) -> DynamicObject {
        let mut v = json!({
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
        let live = vec![
            dyn_obj("other", Some("ns"), json!({})),
            dyn_obj("d", Some("ns"), json!({})),
        ];
        let found = find_live_match(&m, &live).expect("should match");
        assert_eq!(found.metadata.name.as_deref(), Some("d"));
    }

    #[test]
    fn find_live_match_returns_none_when_absent() {
        let m = test_manifest("apps", "Deployment", "d", Some("ns"), json!({}));
        let live = vec![dyn_obj("x", Some("ns"), json!({}))];
        assert!(find_live_match(&m, &live).is_none());
    }

    #[test]
    fn find_live_match_respects_namespace() {
        let m = test_manifest("", "ConfigMap", "c", Some("a"), json!({}));
        let live = vec![dyn_obj("c", Some("b"), json!({}))];
        assert!(find_live_match(&m, &live).is_none());
    }

    #[test]
    fn find_live_match_handles_cluster_scoped() {
        let m = test_manifest("", "Namespace", "n", None, json!({}));
        let live = vec![dyn_obj("n", None, json!({}))];
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
        let live: HashMap<(String, String, String), Vec<DynamicObject>> = HashMap::new();
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
        let mut live: HashMap<(String, String, String), Vec<DynamicObject>> = HashMap::new();
        live.insert(
            ("apps".into(), "v1".into(), "Deployment".into()),
            vec![live_obj],
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
        let mut live: HashMap<(String, String, String), Vec<DynamicObject>> = HashMap::new();
        live.insert(
            ("apps".into(), "v1".into(), "Deployment".into()),
            vec![live_obj],
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
        let mut live: HashMap<(String, String, String), Vec<DynamicObject>> = HashMap::new();
        live.insert(
            ("".into(), "v1".into(), "ConfigMap".into()),
            vec![dyn_obj("c", Some("ns"), json!({}))],
        );
        // Deployment has no live entry -> missing
        let drifts = compute_drifts(&[m1, m2], &live);
        assert_eq!(drifts.len(), 1);
        assert_eq!(drifts[0].key.kind, "Deployment");
    }
}
