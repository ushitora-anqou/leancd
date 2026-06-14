//! Parsing of Kubernetes manifests from YAML.
//!
//! Multi-document YAML is parsed one document at a time (streaming) so the
//! full manifest set is never held in memory at once. Manifests are kept as
//! untyped JSON values so they can be applied generically via `DynamicObject`
//! to any resource kind, including CRDs.

#![allow(deprecated)] // serde_yaml is maintenance-mode but is the stable,
                      // streaming-capable YAML parser we depend on (see design.doc).
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use serde_yaml::Deserializer;

use crate::error::{Error, Result};

/// A single parsed manifest, kept as an untyped value plus the identity bits
/// extracted from `apiVersion`/`kind`/`metadata`.
#[derive(Debug, Clone)]
pub struct RawManifest {
    /// API group (empty for the core API group).
    pub group: String,
    /// API version, e.g. `v1`.
    pub version: String,
    /// Resource kind, e.g. `Deployment`.
    pub kind: String,
    /// `metadata.name`.
    pub name: String,
    /// `metadata.namespace`, if present.
    pub namespace: Option<String>,
    /// The whole manifest document as a JSON value (with the managed-by label
    /// injected before apply).
    pub data: Value,
}

impl RawManifest {
    /// `(group, version, kind)` triple identifying the resource type.
    pub fn gvk(&self) -> (String, String, String) {
        (self.group.clone(), self.version.clone(), self.kind.clone())
    }
}

/// Split `apiVersion` into `(group, version)` (`""` for the core group).
pub fn split_api_version(api_version: &str) -> (String, String) {
    match api_version.split_once('/') {
        Some((g, v)) => (g.to_string(), v.to_string()),
        None => (String::new(), api_version.to_string()),
    }
}

fn as_str(v: &Value) -> Option<String> {
    v.as_str().map(|s| s.to_string())
}

fn value_to_manifest(value: Value) -> Result<Option<RawManifest>> {
    if value.is_null() {
        return Ok(None);
    }
    let obj = match value.as_object() {
        Some(o) => o,
        None => return Ok(None), // skip non-mapping documents
    };
    let api_version = match obj.get("apiVersion").and_then(as_str) {
        Some(s) => s,
        None => return Ok(None), // not a k8s manifest (e.g. a List is handled below)
    };
    let kind = match obj.get("kind").and_then(as_str) {
        Some(s) => s,
        None => return Ok(None),
    };
    let metadata = obj.get("metadata");
    let name = match metadata.and_then(|m| m.get("name")).and_then(as_str) {
        Some(s) => s,
        None => return Ok(None),
    };
    let namespace = metadata.and_then(|m| m.get("namespace")).and_then(as_str);

    let (group, version) = split_api_version(&api_version);
    Ok(Some(RawManifest {
        group,
        version,
        kind,
        name,
        namespace,
        data: value,
    }))
}

/// Parse a YAML string that may contain multiple documents. A `kind: List`
/// resource is expanded into its `items` (design §5.3).
pub fn parse_str(yaml: &str) -> Result<Vec<RawManifest>> {
    let mut out = Vec::new();
    for doc in Deserializer::from_str(yaml) {
        let value: Value = match Value::deserialize(doc) {
            Ok(v) => v,
            Err(e) => {
                // Skip blank or unparseable documents rather than aborting.
                tracing::debug!(error = %e, "skipping unparseable YAML document");
                continue;
            }
        };
        expand_value(value, &mut out)?;
    }
    Ok(out)
}

/// Push the manifest(s) encoded in `value` into `out`. A `kind: List` document
/// is recursively expanded into its `items`; any other document becomes a
/// single manifest via [`value_to_manifest`].
fn expand_value(value: Value, out: &mut Vec<RawManifest>) -> Result<()> {
    if let Some(obj) = value.as_object() {
        if obj.get("kind").and_then(Value::as_str) == Some("List") {
            if let Some(items) = obj.get("items").and_then(Value::as_array) {
                for item in items {
                    expand_value(item.clone(), out)?;
                }
            }
            return Ok(());
        }
    }
    if let Some(m) = value_to_manifest(value)? {
        out.push(m);
    }
    Ok(())
}

/// Recursively scan a directory for `*.yaml`/`*.yml` files and parse them all.
pub async fn parse_dir(root: &Path) -> Result<Vec<RawManifest>> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_yaml_files(root, &mut files)?;
    let mut out = Vec::new();
    for file in files {
        let contents = tokio::fs::read_to_string(&file).await?;
        match parse_str(&contents) {
            Ok(ms) => out.extend(ms),
            Err(e) => {
                tracing::warn!(path = %file.display(), error = %e, "failed to parse manifest file");
            }
        }
    }
    Ok(out)
}

fn collect_yaml_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect_yaml_files(&path, out)?;
        } else if ft.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if ext == "yaml" || ext == "yml" {
                    out.push(path);
                }
            }
        }
    }
    Ok(())
}

/// Inject the managed-by label into a manifest's `metadata.labels` so the
/// resource can be safely pruned later.
pub fn inject_managed_label(m: &mut RawManifest, key: &str, value: &str) {
    let obj = match m.data.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    let meta = obj
        .entry("metadata".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let meta_map = match meta.as_object_mut() {
        Some(m) => m,
        None => return,
    };
    let labels_val = meta_map
        .entry("labels".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(labels) = labels_val.as_object_mut() {
        labels.insert(key.to_string(), Value::String(value.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn split_core_vs_grouped() {
        assert_eq!(split_api_version("v1"), ("".to_string(), "v1".to_string()));
        assert_eq!(
            split_api_version("apps/v1"),
            ("apps".to_string(), "v1".to_string())
        );
        assert_eq!(
            split_api_version("monitoring.coreos.com/v1"),
            ("monitoring.coreos.com".to_string(), "v1".to_string())
        );
    }

    #[test]
    fn parse_multidoc_skips_blanks() {
        let yaml = "\
apiVersion: v1
kind: ConfigMap
metadata:
  name: a
  namespace: default
---
# a comment-only document
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: d
  namespace: default
spec:
  replicas: 1
";
        let ms = parse_str(yaml).unwrap();
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].kind, "ConfigMap");
        assert!(ms[0].group.is_empty());
        assert_eq!(ms[1].group, "apps");
        assert_eq!(ms[1].kind, "Deployment");
        assert_eq!(ms[1].name, "d");
    }

    #[test]
    fn inject_label_into_existing_metadata() {
        let v = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": { "name": "a" },
            "data": { "k": "v" }
        });
        let mut m = value_to_manifest(v).unwrap().unwrap();
        inject_managed_label(&mut m, "app.kubernetes.io/managed-by", "leancd");
        assert_eq!(
            m.data["metadata"]["labels"]["app.kubernetes.io/managed-by"],
            "leancd"
        );
    }

    #[test]
    fn inject_label_creates_metadata() {
        // A manifest without metadata gets metadata.labels injected.
        let v = json!({ "apiVersion": "v1", "kind": "ConfigMap", "data": {} });
        // Inject name so it is recognized as a manifest.
        let mut v = v;
        v["metadata"] = json!({ "name": "x" });
        let mut m = value_to_manifest(v).unwrap().unwrap();
        inject_managed_label(&mut m, "managed-by", "leancd");
        assert_eq!(m.data["metadata"]["labels"]["managed-by"], "leancd");
    }

    #[test]
    fn kind_list_is_expanded_into_items() {
        let yaml = "\
apiVersion: v1
kind: List
items:
  - apiVersion: v1
    kind: ConfigMap
    metadata:
      name: a
      namespace: default
  - apiVersion: apps/v1
    kind: Deployment
    metadata:
      name: d
      namespace: default
";
        let ms = parse_str(yaml).unwrap();
        assert_eq!(ms.len(), 2, "List items should be expanded");
        assert_eq!(ms[0].kind, "ConfigMap");
        assert_eq!(ms[0].name, "a");
        assert_eq!(ms[1].kind, "Deployment");
        assert_eq!(ms[1].group, "apps");
    }

    #[test]
    fn kind_list_skips_non_manifest_items() {
        let yaml = "\
apiVersion: v1
kind: List
items:
  - apiVersion: v1
    kind: ConfigMap
    metadata:
      name: keep
  - notAManifest: true
";
        let ms = parse_str(yaml).unwrap();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].name, "keep");
    }

    #[test]
    fn kind_list_with_empty_items_yields_nothing() {
        let yaml = "\
apiVersion: v1
kind: List
items: []
";
        let ms = parse_str(yaml).unwrap();
        assert!(ms.is_empty());
    }
}
