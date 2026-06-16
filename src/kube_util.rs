//! Kubernetes helpers: API discovery, dynamic API construction, server-side
//! apply, list and delete — all on `DynamicObject` so any resource kind
//! (including CRDs) is handled uniformly.
//!
//! Per the memory strategy we never build an informer/cache; every call here
//! issues a direct API request and returns immediately.

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
/// metadata). `force` claims ownership of conflicting fields.
pub async fn apply(
    client: &Client,
    ar: &ApiResource,
    scope: &Scope,
    default_namespace: &str,
    manifest: &serde_json::Value,
    field_manager: &str,
    force: bool,
) -> Result<DynamicObject> {
    let obj: DynamicObject = serde_json::from_value(manifest.clone()).map_err(|e| {
        Error::Manifest(format!("failed to build DynamicObject from manifest: {e}"))
    })?;
    let name = obj
        .metadata
        .name
        .clone()
        .ok_or_else(|| Error::Manifest("manifest has no metadata.name".into()))?;
    let namespace = obj.metadata.namespace.as_deref();

    let api = api_for(client, ar, scope, namespace, default_namespace);

    let pp = PatchParams::apply(field_manager);
    let pp = if force { pp.force() } else { pp };
    let patched = api
        .patch(&name, &pp, &Patch::Apply(&obj))
        .await
        .map_err(Error::Kube)?;
    Ok(patched)
}

/// List live resources of a kind across **all** namespaces (namespaced
/// resources) or cluster-wide (cluster-scoped resources), optionally filtered by
/// a label selector. Used by drift detection and prune so that resources leancd
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
    let _ = api.delete(name, &DeleteParams::default()).await?;
    Ok(())
}
