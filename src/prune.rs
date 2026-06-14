//! Pruning: delete resources that leancd previously applied but that no
//! longer exist in Git.
//!
//! To stay state-light we compare the previously applied set (persisted in the
//! state ConfigMap) against the current Git set, and delete the difference.
//! All applies also inject a managed-by label, so a deployed resource that was
//! taken over by another manager is identifiable.

use std::collections::{HashMap, HashSet};

use kube::core::DynamicObject;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::Result;
use crate::kube_util;
use crate::manifest::RawManifest;

/// Stable identity of a managed resource, used for set operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ResourceKey {
    pub group: String,
    pub version: String,
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
}

impl ResourceKey {
    pub fn from_manifest(m: &RawManifest) -> Self {
        Self {
            group: m.group.clone(),
            version: m.version.clone(),
            kind: m.kind.clone(),
            namespace: m.namespace.clone(),
            name: m.name.clone(),
        }
    }

    pub fn keys_of(manifests: &[RawManifest]) -> Vec<ResourceKey> {
        manifests.iter().map(ResourceKey::from_manifest).collect()
    }

    /// Build a key from a live dynamic object, taking the GVK from the
    /// discovery result that produced it. The object's embedded `apiVersion`/
    /// `kind` are not reliably populated by the API server, so the caller
    /// supplies the resolved `(group, version, kind)`.
    pub fn from_dynamic(obj: &DynamicObject, group: &str, version: &str, kind: &str) -> Self {
        Self {
            group: group.to_string(),
            version: version.to_string(),
            kind: kind.to_string(),
            namespace: obj.metadata.namespace.clone(),
            name: obj.metadata.name.clone().unwrap_or_default(),
        }
    }
}

/// Resources present in `prev` but absent from `current` — the deletion set
/// derived purely from the persisted applied state. Pure: no API calls.
pub fn deletion_targets<'a>(
    prev: &'a [ResourceKey],
    current: &[ResourceKey],
) -> Vec<&'a ResourceKey> {
    let current_set: HashSet<&ResourceKey> = current.iter().collect();
    prev.iter().filter(|k| !current_set.contains(*k)).collect()
}

/// Delete resources that leancd previously applied but that Git no longer
/// declares. The deletion set combines two signals (design 付録B):
/// 1. The persisted applied set (`prev`) minus the current Git set — the
///    primary signal.
/// 2. A managed-label safety net: for each GVK seen in `prev`, live resources
///    bearing the managed-by label that are absent from Git are also pruned,
///    recovering orphans even when a single key was dropped from state.
///
/// GVKs never applied before are out of scope (state-light; a fully-empty
/// `prev` skips the safety net). Returns the keys actually deleted.
pub async fn prune(
    client: &kube::client::Client,
    prev: &[ResourceKey],
    current: &[ResourceKey],
    cfg: &Config,
) -> Result<Vec<ResourceKey>> {
    let current_set: HashSet<&ResourceKey> = current.iter().collect();

    // (1) Primary signal: applied set minus Git set.
    let mut targets: HashSet<ResourceKey> = deletion_targets(prev, current)
        .into_iter()
        .cloned()
        .collect();

    // (2) Safety net: list live managed resources for each previously applied
    //     GVK and add any that Git no longer declares.
    let label_sel = format!("{}={}", cfg.managed_label_key, cfg.managed_label_value);
    let mut discovery: HashMap<
        (String, String, String),
        (kube::core::ApiResource, kube::discovery::ApiCapabilities),
    > = HashMap::new();

    let prev_gvks: HashSet<(String, String, String)> = prev
        .iter()
        .map(|k| (k.group.clone(), k.version.clone(), k.kind.clone()))
        .collect();
    for gvk in &prev_gvks {
        let (group, version, kind) = gvk;
        let (ar, caps) = match discovery.get(gvk) {
            Some(c) => c.clone(),
            None => match kube_util::resolve(client, group, version, kind).await {
                Ok(c) => {
                    discovery.insert(gvk.clone(), c.clone());
                    c
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        gvk = ?(group, version, kind),
                        "prune: safety-net discovery failed, skipping list"
                    );
                    continue;
                }
            },
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
                tracing::warn!(
                    error = %e,
                    gvk = ?(group, version, kind),
                    "prune: safety-net list failed"
                );
                continue;
            }
        };
        for obj in live {
            let k = ResourceKey::from_dynamic(&obj, group, version, kind);
            if !current_set.contains(&k) {
                targets.insert(k);
            }
        }
    }

    // Delete the unified candidate set.
    let mut deleted = Vec::new();
    for key in targets {
        let gk = (key.group.clone(), key.version.clone(), key.kind.clone());
        let (ar, caps) = match discovery.get(&gk) {
            Some(c) => c.clone(),
            None => match kube_util::resolve(client, &key.group, &key.version, &key.kind).await {
                Ok(c) => {
                    discovery.insert(gk.clone(), c.clone());
                    c
                }
                Err(e) => {
                    tracing::warn!(error = %e, key = ?key, "prune: discovery failed, skipping");
                    continue;
                }
            },
        };
        match kube_util::delete(
            client,
            &ar,
            &caps.scope,
            key.namespace.as_deref(),
            &cfg.namespace,
            &key.name,
        )
        .await
        {
            Ok(()) => {
                tracing::info!(key = ?key, "pruned resource no longer in Git");
                deleted.push(key);
            }
            Err(e) => {
                tracing::warn!(error = %e, key = ?key, "prune: delete failed");
            }
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(group: &str, kind: &str, name: &str, namespace: Option<&str>) -> RawManifest {
        RawManifest {
            group: group.to_string(),
            version: "v1".to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            namespace: namespace.map(String::from),
            data: serde_json::json!({}),
        }
    }

    fn key(group: &str, kind: &str, name: &str, ns: Option<&str>) -> ResourceKey {
        ResourceKey {
            group: group.to_string(),
            version: "v1".to_string(),
            kind: kind.to_string(),
            namespace: ns.map(String::from),
            name: name.to_string(),
        }
    }

    #[test]
    fn keys_preserve_identity() {
        let ms = vec![
            raw("apps", "Deployment", "a", Some("ns")),
            raw("", "ConfigMap", "c", None),
        ];
        let keys = ResourceKey::keys_of(&ms);
        assert_eq!(keys.len(), 2);
        assert_eq!(
            keys[0],
            ResourceKey {
                group: "apps".into(),
                version: "v1".into(),
                kind: "Deployment".into(),
                namespace: Some("ns".into()),
                name: "a".into(),
            }
        );
        assert!(keys[1].namespace.is_none());
    }

    #[test]
    fn keys_are_hashable_for_set_diffs() {
        use std::collections::HashSet;
        let ms = vec![
            raw("", "ConfigMap", "a", Some("ns")),
            raw("", "ConfigMap", "a", Some("ns")), // duplicate
            raw("", "ConfigMap", "b", Some("ns")),
        ];
        let set: HashSet<ResourceKey> = ResourceKey::keys_of(&ms).into_iter().collect();
        // The collected set dedupes identical keys.
        assert_eq!(set.len(), 2);
    }

    // --- deletion_targets: pure set difference (prev minus current) ---

    #[test]
    fn deletion_targets_returns_prev_minus_current() {
        let prev = vec![
            key("apps", "Deployment", "a", Some("ns")),
            key("", "ConfigMap", "b", Some("ns")),
            key("", "ConfigMap", "c", Some("ns")),
        ];
        // "c" is still in Git; "a" and "b" are gone.
        let current = vec![key("", "ConfigMap", "c", Some("ns"))];
        let targets = deletion_targets(&prev, &current);
        let names: HashSet<&str> = targets.iter().map(|k| k.name.as_str()).collect();
        let expected: HashSet<&str> = ["a", "b"].into_iter().collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn deletion_targets_empty_when_all_kept() {
        let prev = vec![key("", "ConfigMap", "a", Some("ns"))];
        let current = vec![key("", "ConfigMap", "a", Some("ns"))];
        assert!(deletion_targets(&prev, &current).is_empty());
    }

    #[test]
    fn deletion_targets_all_when_current_empty() {
        let prev = vec![key("", "ConfigMap", "a", Some("ns"))];
        assert_eq!(deletion_targets(&prev, &[]).len(), 1);
    }

    // --- ResourceKey::from_dynamic: identity from a live object ---

    #[test]
    fn from_dynamic_extracts_identity() {
        let obj: DynamicObject =
            serde_json::from_value(json!({"metadata": {"name": "d", "namespace": "ns"}})).unwrap();
        let k = ResourceKey::from_dynamic(&obj, "apps", "v1", "Deployment");
        assert_eq!(k.group, "apps");
        assert_eq!(k.version, "v1");
        assert_eq!(k.kind, "Deployment");
        assert_eq!(k.name, "d");
        assert_eq!(k.namespace.as_deref(), Some("ns"));
    }

    #[test]
    fn from_dynamic_handles_cluster_scoped() {
        let obj: DynamicObject =
            serde_json::from_value(json!({"metadata": {"name": "leancd-bench"}})).unwrap();
        let k = ResourceKey::from_dynamic(&obj, "", "v1", "Namespace");
        assert_eq!(k.name, "leancd-bench");
        assert!(k.namespace.is_none());
    }
}
