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

/// Aggregate resource-health summary persisted in the state ConfigMap. Mirrors
/// Argo CD's "application health = worst managed-resource health" (see
/// `resource_health.rs`, a port of `gitops-engine/pkg/health`). Stores only the
/// worst status string and per-status counts — never per-resource bodies — so it
/// stays well within the 1MiB ConfigMap limit regardless of scale. `worst` is
/// `None` until the first health assessment runs, or when no managed resource
/// has a built-in health check (e.g. a repo of only ConfigMaps).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthSummary {
    /// Worst status across evaluated managed resources (`"Healthy"`,
    /// `"Progressing"`, `"Degraded"`, `"Suspended"`, `"Missing"`, `"Unknown"`),
    /// or `None` if none were evaluated.
    pub worst: Option<String>,
    /// Message from the worst resource — why it is not Healthy (e.g. a rollout
    /// or progress-deadline reason). `None` iff `worst` is `None`.
    pub worst_message: Option<String>,
    pub healthy: usize,
    pub progressing: usize,
    pub degraded: usize,
    pub suspended: usize,
    pub missing: usize,
    pub unknown: usize,
}

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
    /// Resource keys whose server-side apply failed on the last pass (API
    /// discovery or apply error). Self-healing: the next pass's drift check
    /// reports them missing and re-applies them, so this is a visibility signal
    /// only — it is never written to `last_error`, and so does not trip the
    /// health probe (liveness/readiness). Empty on a clean pass.
    #[serde(default)]
    pub apply_failures: Vec<ResourceKey>,
    /// Aggregate resource-health summary: the worst health across managed
    /// resources with a built-in health check, plus per-status counts.
    /// Populated by `reconcile`'s health assessment; default (all-zero,
    /// `worst = None`) until the first pass evaluates health.
    pub health: HealthSummary,
}

impl State {
    pub fn to_data(&self) -> BTreeMap<String, String> {
        let json = serde_json::to_string(self).unwrap_or_else(|_| "{}".into());
        let mut m = BTreeMap::new();
        m.insert("state".into(), json);
        m
    }

    pub fn from_data(data: Option<&BTreeMap<String, String>>) -> Self {
        data.and_then(|d| d.get("state"))
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }
}

fn api(client: &kube::client::Client, cfg: &Config) -> Api<ConfigMap> {
    Api::namespaced(client.clone(), &cfg.namespace)
}

/// Build the ConfigMap Lean CD persists its sync state into.
fn build_state_configmap(cfg: &Config, state: &State) -> ConfigMap {
    ConfigMap {
        metadata: ObjectMeta {
            name: Some(cfg.state_configmap.clone()),
            namespace: Some(cfg.namespace.clone()),
            // Deliberately no managed-by label: the prune safety-net lists live
            // resources by that label, so an unlabeled state ConfigMap is
            // invisible to prune and Lean CD will not delete its own state every
            // pass.
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
            apply_failures: vec![key("apps", "Deployment", "x", Some("default"))],
            health: HealthSummary {
                worst: Some("Progressing".into()),
                worst_message: Some("Waiting for rollout to finish".into()),
                progressing: 2,
                ..Default::default()
            },
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
        assert_eq!(back.apply_failures.len(), 1);
        assert_eq!(back.apply_failures[0].name, "x");
        assert_eq!(back.health.worst.as_deref(), Some("Progressing"));
        assert_eq!(back.health.progressing, 2);
        assert_eq!(
            back.health.worst_message.as_deref(),
            Some("Waiting for rollout to finish")
        );
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
    fn legacy_per_key_data_is_ignored() {
        // The unified "state" JSON key replaces the legacy per-field keys; an
        // old ConfigMap written field-by-field is now read as the default state.
        let mut data = BTreeMap::new();
        data.insert("last_sha".into(), "abc".into());
        data.insert("sync_count".into(), "5".into());
        let s = State::from_data(Some(&data));
        assert_eq!(s.last_sha, None);
        assert_eq!(s.sync_count, 0);
    }

    #[test]
    fn corrupt_state_json_falls_back_to_default() {
        let mut data = BTreeMap::new();
        data.insert("state".into(), "not-json".into());
        let s = State::from_data(Some(&data));
        assert!(s.applied.is_empty());
        assert_eq!(s.sync_count, 0);
    }

    #[test]
    fn legacy_state_without_apply_failures_defaults_empty() {
        // A state ConfigMap written by an older Lean CD (before apply_failures)
        // must deserialize with an empty apply_failures, not an error — the
        // field carries #[serde(default)] for exactly this rollout. Build the
        // legacy blob from a real State and strip only apply_failures.
        let full = State {
            last_sha: Some("abc".into()),
            sync_count: 1,
            applied: vec![key("", "ConfigMap", "c", None)],
            ..Default::default()
        };
        let mut json = serde_json::to_value(&full).unwrap();
        json.as_object_mut().unwrap().remove("apply_failures");
        let mut data = BTreeMap::new();
        data.insert("state".into(), json.to_string());
        let s = State::from_data(Some(&data));
        assert_eq!(s.last_sha.as_deref(), Some("abc"));
        assert_eq!(s.applied.len(), 1);
        assert!(
            s.apply_failures.is_empty(),
            "missing apply_failures must default to empty"
        );
    }

    #[test]
    fn state_configmap_carries_no_managed_label() {
        // The prune safety-net lists live resources by the managed-by label, so
        // the state ConfigMap must NOT carry it — otherwise Lean CD prunes its
        // own state every pass.
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
