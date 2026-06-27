//! Kubernetes helpers: API discovery, dynamic API construction, server-side
//! apply, list and delete — all on `DynamicObject` so any resource kind
//! (including CRDs) is handled uniformly.
//!
//! Per the memory strategy we never build an informer/cache; every call here
//! issues a direct API request and returns immediately.

use std::collections::HashMap;

use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::client::Client;
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::discovery::{ApiCapabilities, Scope};

use crate::error::{Error, Result};

/// Resolve `(group, version, kind)` to the dynamic-API metadata needed to talk
/// to that resource. Performs an API-discovery round-trip (cheap metadata only).
pub async fn resolve(
    client: &Client,
    group: &str,
    version: &str,
    kind: &str,
) -> Result<(ApiResource, ApiCapabilities)> {
    let gvk = GroupVersionKind::gvk(group, version, kind);
    let pair = kube::discovery::pinned_kind(client, &gvk)
        .await
        .map_err(|e| Error::Other(format!("api discovery for {gvk:?} failed: {e}")))?;
    Ok(pair)
}

/// Per-pass discovery cache: maps a resolved `(group, version, kind)` to its
/// `(ApiResource, ApiCapabilities)` so repeated lookups within one
/// reconcile/prune pass skip the discovery round-trip. Intentionally **not**
/// cluster-wide — construct one per pass and drop it when the pass ends, so the
/// memory strategy's "no kube-rs informer/cache" rule is respected.
#[derive(Default)]
pub struct DiscoveryCache {
    map: HashMap<(String, String, String), (ApiResource, ApiCapabilities)>,
}

impl DiscoveryCache {
    /// Create an empty per-pass cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve `(group, version, kind)`, caching the result. On a cache hit the
    /// discovery round-trip is skipped. Discovery failures are returned as-is
    /// and **not** cached, so a transient failure can be retried later in the
    /// same pass (callers currently `continue` on `Err`).
    pub async fn get_or_resolve(
        &mut self,
        client: &Client,
        group: &str,
        version: &str,
        kind: &str,
    ) -> Result<(ApiResource, ApiCapabilities)> {
        let key = (group.to_string(), version.to_string(), kind.to_string());
        if let Some(pair) = self.map.get(&key) {
            return Ok(pair.clone());
        }
        let pair = resolve(client, group, version, kind).await?;
        self.map.insert(key, pair.clone());
        Ok(pair)
    }
}

/// Build a namespaced-or-cluster [`Api`] handle for a dynamic resource.
pub fn api_for(
    client: &Client,
    ar: &ApiResource,
    scope: &Scope,
    namespace: Option<&str>,
    default_namespace: &str,
) -> Api<DynamicObject> {
    match scope {
        Scope::Cluster => Api::all_with(client.clone(), ar),
        // Namespaced (and any future variants) route through the namespace path.
        _ => Api::namespaced_with(client.clone(), namespace.unwrap_or(default_namespace), ar),
    }
}

/// Server-side-apply a manifest value (already carrying apiVersion/kind/
/// metadata). Always applies with `.force()` so conflicting fields owned by
/// other managers are reclaimed.
pub async fn apply(
    client: &Client,
    ar: &ApiResource,
    scope: &Scope,
    default_namespace: &str,
    manifest: serde_json::Value,
    field_manager: &str,
) -> Result<DynamicObject> {
    let obj: DynamicObject = serde_json::from_value(manifest).map_err(|e| {
        Error::Manifest(format!("failed to build DynamicObject from manifest: {e}"))
    })?;
    let name = obj
        .metadata
        .name
        .clone()
        .ok_or_else(|| Error::Manifest("manifest has no metadata.name".into()))?;
    let namespace = obj.metadata.namespace.as_deref();

    let api = api_for(client, ar, scope, namespace, default_namespace);

    let pp = PatchParams::apply(field_manager).force();
    let patched = api
        .patch(&name, &pp, &Patch::Apply(&obj))
        .await
        .map_err(Error::Kube)?;
    Ok(patched)
}

/// List live resources of a kind across **all** namespaces (namespaced
/// resources) or cluster-wide (cluster-scoped resources), optionally filtered by
/// a label selector. Used by drift detection and prune so that resources Lean CD
/// applied in *any* namespace are visible — not only those in
/// `default_namespace`. (`Api::all_with` lists namespaced kinds across every
/// namespace.)
pub async fn list_all(
    client: &Client,
    ar: &ApiResource,
    label_selector: Option<&str>,
) -> Result<Vec<DynamicObject>> {
    let api = Api::all_with(client.clone(), ar);
    let mut lp = ListParams::default();
    if let Some(sel) = label_selector {
        lp = lp.labels(sel);
    }
    let list = api.list(&lp).await.map_err(Error::Kube)?;
    Ok(list.items)
}

/// The [`DeleteParams`] Lean CD uses for every deletion: Foreground cascade
/// (`propagationPolicy: Foreground`). An owner resource is held behind a
/// `foregroundDeletion` finalizer until all of its dependents — resources
/// carrying an `ownerReferences` entry pointing at it — are removed first,
/// giving a predictable, dependent-first deletion order.
fn delete_params() -> DeleteParams {
    DeleteParams::foreground()
}

/// Delete a single resource by name.
pub async fn delete(
    client: &Client,
    ar: &ApiResource,
    scope: &Scope,
    namespace: Option<&str>,
    default_namespace: &str,
    name: &str,
) -> Result<()> {
    let api = api_for(client, ar, scope, namespace, default_namespace);
    let _ = api.delete(name, &delete_params()).await?;
    Ok(())
}

/// Fetch a single resource by name. Returns `None` on 404 (already gone) so
/// callers can poll hook resources without distinguishing a deleted object
/// from a transient miss. `DynamicObject.data` is `#[serde(flatten)]`, so a
/// resource's `status` lives at `obj.data["status"]`.
pub async fn get(
    client: &Client,
    ar: &ApiResource,
    scope: &Scope,
    namespace: Option<&str>,
    default_namespace: &str,
    name: &str,
) -> Result<Option<DynamicObject>> {
    let api = api_for(client, ar, scope, namespace, default_namespace);
    match api.get(name).await {
        Ok(obj) => Ok(Some(obj)),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(None),
        Err(e) => Err(Error::Kube(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::api::PropagationPolicy;

    #[test]
    fn delete_params_uses_foreground_cascade() {
        let dp = delete_params();
        assert_eq!(
            dp.propagation_policy,
            Some(PropagationPolicy::Foreground),
            "leancd must cascade-delete in the foreground so an owner resource \
             waits for its dependents to be removed first"
        );
    }

    #[test]
    fn delete_params_leaves_other_fields_default() {
        // Foreground cascade does not imply dry-run, a grace-period override,
        // or preconditions: only the propagation policy changes.
        let dp = delete_params();
        assert!(!dp.dry_run);
        assert_eq!(dp.grace_period_seconds, None);
        assert!(dp.preconditions.is_none());
    }

    /// `kube_util::apply` builds the SSA patch body by deserializing the manifest
    /// into a `DynamicObject` and letting `Patch::Apply` re-serialize it. A
    /// ConfigMap has both `metadata.annotations` and a top-level `data` field, so
    /// this test pins that both survive that round-trip — guarding against any
    /// future regression where annotations (or `data`) would be silently dropped
    /// on apply. (Background: the VM-stack comparison once flagged dashboard
    /// ConfigMaps as missing annotations vs Argo CD; that delta turned out to be
    /// Argo's own tracking-id, never present in the source manifest — so this
    /// test asserts the Lean CD side is correct and stays correct.)
    #[test]
    fn apply_round_trip_preserves_metadata_annotations() {
        use kube::core::DynamicObject;
        let manifest = serde_json::json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "vmks-grafana-overview",
                "namespace": "app",
                "annotations": {
                    "grafana_dashboard": "1",
                    "my.example/ann": "value"
                }
            },
            "data": { "grafana-overview.json": "{\"title\":\"x\"}" }
        });
        let obj: DynamicObject =
            serde_json::from_value(manifest.clone()).expect("parse manifest into DynamicObject");
        // This is what Patch::Apply(&obj) sends over the wire.
        let wire: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&obj).expect("serialize DynamicObject"))
                .expect("parse wire body");
        assert_eq!(
            wire["metadata"]["annotations"], manifest["metadata"]["annotations"],
            "SSA patch body must preserve metadata.annotations through the DynamicObject round-trip"
        );
        assert_eq!(
            wire["data"], manifest["data"],
            "ConfigMap data must survive the round-trip"
        );
    }
}
