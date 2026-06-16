//! Sync state persisted in a single ConfigMap (no CRD, no database).
//!
//! Holds the last applied commit, a summary of the last sync, and the set of
//! applied resource keys (used for pruning). Everything is stored as plain
//! string data so it fits comfortably inside the 1MiB ConfigMap limit for
//! realistic scale.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, ObjectMeta, Patch, PatchParams};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::prune::ResourceKey;

/// Persisted sync state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub last_sha: Option<String>,
    pub last_sync_epoch: Option<u64>,
    pub sync_count: u64,
    pub last_error: Option<String>,
    pub drift_count: usize,
    pub managed_count: usize,
    pub applied: Vec<ResourceKey>,
}

impl State {
    pub fn to_data(&self) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        if let Some(s) = &self.last_sha {
            m.insert("last_sha".into(), s.clone());
        }
        if let Some(e) = self.last_sync_epoch {
            m.insert("last_sync_epoch".into(), e.to_string());
        }
        m.insert("sync_count".into(), self.sync_count.to_string());
        if let Some(e) = &self.last_error {
            m.insert("last_error".into(), e.clone());
        }
        m.insert("drift_count".into(), self.drift_count.to_string());
        m.insert("managed_count".into(), self.managed_count.to_string());
        m.insert(
            "applied".into(),
            serde_json::to_string(&self.applied).unwrap_or_else(|_| "[]".into()),
        );
        m
    }

    pub fn from_data(data: Option<&BTreeMap<String, String>>) -> Self {
        let g = |k: &str| data.and_then(|d| d.get(k)).cloned();
        Self {
            last_sha: g("last_sha").filter(|s| !s.is_empty()),
            last_sync_epoch: g("last_sync_epoch").and_then(|s| s.parse().ok()),
            sync_count: g("sync_count").and_then(|s| s.parse().ok()).unwrap_or(0),
            last_error: g("last_error").filter(|s| !s.is_empty()),
            drift_count: g("drift_count").and_then(|s| s.parse().ok()).unwrap_or(0),
            managed_count: g("managed_count").and_then(|s| s.parse().ok()).unwrap_or(0),
            applied: g("applied")
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
        }
    }
}

fn api(client: &kube::client::Client, cfg: &Config) -> Api<ConfigMap> {
    Api::namespaced(client.clone(), &cfg.namespace)
}

/// Build the ConfigMap leancd persists its sync state into.
fn build_state_configmap(cfg: &Config, state: &State) -> ConfigMap {
    ConfigMap {
        metadata: ObjectMeta {
            name: Some(cfg.state_configmap.clone()),
            namespace: Some(cfg.namespace.clone()),
            // Deliberately no managed-by label: the prune safety-net lists live
            // resources by that label, so an unlabelled state ConfigMap is
            // invisible to prune and leancd will not delete its own state every
            // pass. (design.md §5.5 / 付録B.)
            ..Default::default()
        },
        data: Some(state.to_data()),
        binary_data: None,
        immutable: None,
    }
}

/// Persist `state` into the configured ConfigMap (server-side apply upsert).
pub async fn write(client: &kube::client::Client, cfg: &Config, state: &State) -> Result<()> {
    let cms = api(client, cfg);
    let cm = build_state_configmap(cfg, state);
    let pp = PatchParams::apply(&cfg.field_manager);
    cms.patch(&cfg.state_configmap, &pp, &Patch::Apply(&cm))
        .await?;
    Ok(())
}

/// Read the persisted state, or `None` if the ConfigMap does not exist yet.
pub async fn read(client: &kube::client::Client, cfg: &Config) -> Result<Option<State>> {
    let cms = api(client, cfg);
    match cms.get(&cfg.state_configmap).await {
        Ok(cm) => Ok(Some(State::from_data(cm.data.as_ref()))),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(None),
        Err(e) => Err(Error::Kube(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prune::ResourceKey;

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
    fn roundtrip_preserves_all_fields() {
        let s = State {
            last_sha: Some("abc123".into()),
            last_sync_epoch: Some(1_700_000_000),
            sync_count: 42,
            last_error: Some("boom".into()),
            drift_count: 3,
            managed_count: 100,
            applied: vec![
                key("apps", "Deployment", "d", Some("default")),
                key("", "ConfigMap", "c", None),
            ],
        };
        let back = State::from_data(Some(&s.to_data()));
        assert_eq!(back.last_sha.as_deref(), Some("abc123"));
        assert_eq!(back.last_sync_epoch, Some(1_700_000_000));
        assert_eq!(back.sync_count, 42);
        assert_eq!(back.last_error.as_deref(), Some("boom"));
        assert_eq!(back.drift_count, 3);
        assert_eq!(back.managed_count, 100);
        assert_eq!(back.applied.len(), 2);
        assert_eq!(back.applied[0].name, "d");
        assert_eq!(back.applied[0].namespace.as_deref(), Some("default"));
        assert!(back.applied[1].namespace.is_none());
    }

    #[test]
    fn empty_state_roundtrips() {
        let s = State::default();
        let back = State::from_data(Some(&s.to_data()));
        assert_eq!(back.last_sha, None);
        assert_eq!(back.last_error, None);
        assert_eq!(back.sync_count, 0);
        assert!(back.applied.is_empty());
    }

    #[test]
    fn from_none_data_is_default() {
        let s = State::from_data(None);
        assert_eq!(s.sync_count, 0);
        assert!(s.applied.is_empty());
        assert_eq!(s.drift_count, 0);
    }

    #[test]
    fn empty_last_sha_is_treated_as_absent() {
        let mut data = BTreeMap::new();
        data.insert("last_sha".into(), "".into());
        data.insert("sync_count".into(), "1".into());
        let s = State::from_data(Some(&data));
        // An empty SHA must not be mistaken for a real commit.
        assert_eq!(s.last_sha, None);
        assert_eq!(s.sync_count, 1);
    }

    #[test]
    fn corrupt_applied_falls_back_to_empty() {
        let mut data = BTreeMap::new();
        data.insert("applied".into(), "not-json".into());
        let s = State::from_data(Some(&data));
        assert!(s.applied.is_empty());
    }

    #[test]
    fn state_configmap_carries_no_managed_label() {
        // The prune safety-net lists live resources by the managed-by label, so
        // the state ConfigMap must NOT carry it — otherwise leancd prunes its
        // own state every pass. (design.md §5.5 / 付録B.)
        let cfg = Config {
            namespace: "default".into(),
            state_configmap: "leancd-state".into(),
            managed_label_key: "app.kubernetes.io/managed-by".into(),
            managed_label_value: "leancd".into(),
            field_manager: "leancd".into(),
            ..Default::default()
        };
        let cm = build_state_configmap(&cfg, &State::default());
        let has_label = cm
            .metadata
            .labels
            .as_ref()
            .map(|m| m.contains_key(&cfg.managed_label_key))
            .unwrap_or(false);
        assert!(
            !has_label,
            "state ConfigMap must not carry the managed-by label"
        );
    }
}
