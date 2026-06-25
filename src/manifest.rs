//! Parsing of Kubernetes manifests from YAML.
//!
//! Multi-document YAML is parsed one document at a time (streaming) so the
//! full manifest set is never held in memory at once. Manifests are kept as
//! untyped JSON values so they can be applied generically via `DynamicObject`
//! to any resource kind, including CRDs.

#![allow(deprecated)] // serde_yaml is maintenance-mode but is the stable,
                      // streaming-capable YAML parser we depend on.
use std::collections::{BTreeMap, BTreeSet};
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
    /// The whole manifest document as serialized YAML bytes. The managed-by
    /// label is injected into a freshly-deserialized `Value` at apply time (not
    /// held here), so this stays the size of the source YAML rather than a
    /// heavier `serde_json::Value` tree.
    pub data: Vec<u8>,
    /// `metadata.annotations` captured at parse time, so [`annotation`] reads
    /// from here without re-deserializing the document.
    pub annotations: BTreeMap<String, String>,
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

    // Capture metadata.annotations for cheap lookups (hook classification etc.).
    let annotations = metadata
        .and_then(|m| m.get("annotations"))
        .and_then(Value::as_object)
        .map(|a| {
            a.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let (group, version) = split_api_version(&api_version);
    let data = serde_yaml::to_string(&value)
        .map_err(|e| Error::Manifest(format!("failed to serialize manifest: {e}")))?
        .into_bytes();
    Ok(Some(RawManifest {
        group,
        version,
        kind,
        name,
        namespace,
        data,
        annotations,
    }))
}

/// Parse a YAML string that may contain multiple documents. A `kind: List`
/// resource is expanded into its `items`, so a `List` behaves like separate
/// documents.
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
    let files = collect_yaml_files(root)?;
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

/// Expand glob `patterns` (e.g. `live/*/prod`) relative to `base` into the
/// deduplicated, deterministically-ordered set of directories they match.
///
/// `*` matches a single path segment and `**` spans any number of segments.
/// Only directories are kept — each is intended to be passed to [`parse_dir`],
/// which already recurses into subdirectories. A literal path with no glob
/// metacharacters matches itself if it is an existing directory.
///
/// Returns [`Error::Config`] if a pattern fails to compile or if **no** pattern
/// matches any directory — a fail-fast so a typo never silently prunes every
/// resource on the next pass.
pub fn expand_roots(base: &Path, patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    for pat in patterns {
        let abs = base.join(pat);
        let matched = glob::glob(&abs.to_string_lossy())
            .map_err(|e| Error::Config(format!("invalid path pattern {pat:?}: {e}")))?;
        for entry in matched {
            let path = entry.map_err(|e| {
                Error::Config(format!("error reading a match of pattern {pat:?}: {e}"))
            })?;
            if path.is_dir() {
                seen.insert(path);
            }
        }
    }
    if seen.is_empty() {
        return Err(Error::Config(format!(
            "no directories matched path pattern(s) {patterns:?}; refusing to sync as \
             that would prune every managed resource"
        )));
    }
    Ok(seen.into_iter().collect())
}

/// Parse manifests from every root produced by [`expand_roots`], recursing
/// into each via [`parse_dir`] and concatenating the results.
pub async fn parse_paths(roots: &[PathBuf]) -> Result<Vec<RawManifest>> {
    let mut out = Vec::new();
    for root in roots {
        let ms = parse_dir(root).await?;
        out.extend(ms);
    }
    Ok(out)
}

/// Collect all `*.yaml`/`*.yml` files under `dir` recursively, via a glob, in a
/// sorted, deduplicated order so parsing is deterministic.
fn collect_yaml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = Vec::new();
    for ext in ["yaml", "yml"] {
        let pattern = dir.join(format!("**/*.{ext}"));
        let matched = glob::glob(&pattern.to_string_lossy())
            .map_err(|e| Error::Config(format!("invalid manifest glob pattern: {e}")))?;
        for entry in matched {
            let path = entry.map_err(|e| Error::Io(e.into_error()))?;
            if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Read a single `metadata.annotations` entry from a manifest, or `None` when
/// the annotation (or `metadata`/`annotations`) is absent or not a string.
/// Reads from the parse-time `annotations` cache — no deserialization.
pub fn annotation(m: &RawManifest, key: &str) -> Option<String> {
    m.annotations.get(key).cloned()
}

/// Inject the managed-by label into a manifest value's `metadata.labels` so the
/// resource can be safely pruned later. Operates on a freshly-deserialized
/// `Value` (the caller turns `RawManifest.data` bytes into a `Value` at apply
/// time, then injects, then applies).
pub fn inject_managed_label_value(value: &mut Value, key: &str, label_value: &str) {
    let obj = match value.as_object_mut() {
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
        labels.insert(key.to_string(), Value::String(label_value.to_string()));
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
    fn annotation_reads_metadata_annotations() {
        let v = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": "a",
                "annotations": { "helm.sh/hook": "pre-install" }
            }
        });
        let m = value_to_manifest(v).unwrap().unwrap();
        assert_eq!(
            annotation(&m, "helm.sh/hook").as_deref(),
            Some("pre-install")
        );
        assert_eq!(annotation(&m, "missing"), None);
    }

    #[test]
    fn annotation_none_when_no_annotations() {
        let v = json!({ "apiVersion": "v1", "kind": "ConfigMap", "metadata": { "name": "a" } });
        let m = value_to_manifest(v).unwrap().unwrap();
        assert_eq!(annotation(&m, "helm.sh/hook"), None);
    }

    #[test]
    fn inject_label_into_existing_metadata() {
        let mut value = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": { "name": "a" },
            "data": { "k": "v" }
        });
        inject_managed_label_value(&mut value, "app.kubernetes.io/managed-by", "leancd");
        assert_eq!(
            value["metadata"]["labels"]["app.kubernetes.io/managed-by"],
            "leancd"
        );
    }

    #[test]
    fn inject_label_creates_metadata() {
        // A manifest without metadata gets metadata.labels injected.
        let mut value = json!({ "apiVersion": "v1", "kind": "ConfigMap", "data": {} });
        inject_managed_label_value(&mut value, "managed-by", "leancd");
        assert_eq!(value["metadata"]["labels"]["managed-by"], "leancd");
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

    use std::collections::BTreeSet;

    /// Build a temp-dir tree for glob tests (unique `suffix` so parallel tests
    /// do not clash). Layout:
    /// ```text
    /// live/a/prod/x.yaml
    /// live/a/prod/sub/y.yaml
    /// live/a/nested/prod/deep.yaml
    /// live/b/prod/z.yaml
    /// live/c/staging/w.yaml
    /// ```
    fn make_glob_tree(suffix: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("leancd-glob-test-{suffix}"));
        let _ = std::fs::remove_dir_all(&base);
        let write_cm = |rel: &str| {
            let p = base.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: x\n").unwrap();
        };
        write_cm("live/a/prod/x.yaml");
        write_cm("live/a/prod/sub/y.yaml");
        write_cm("live/a/nested/prod/deep.yaml");
        write_cm("live/b/prod/z.yaml");
        write_cm("live/c/staging/w.yaml");
        base
    }

    fn as_set(roots: Vec<PathBuf>) -> BTreeSet<PathBuf> {
        roots.into_iter().collect()
    }

    #[test]
    fn expand_roots_single_level_glob_matches_two_dirs() {
        let base = make_glob_tree("single");
        let roots = expand_roots(&base, &["live/*/prod".into()]).unwrap();
        let got = as_set(roots);
        let want: BTreeSet<_> = ["live/a/prod", "live/b/prod"]
            .into_iter()
            .map(|r| base.join(r))
            .collect();
        assert_eq!(got, want, "live/*/prod must match a/prod and b/prod only");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn expand_roots_double_star_matches_nested() {
        let base = make_glob_tree("dstar");
        let roots = expand_roots(&base, &["live/**/prod".into()]).unwrap();
        let got = as_set(roots);
        let want: BTreeSet<_> = ["live/a/prod", "live/a/nested/prod", "live/b/prod"]
            .into_iter()
            .map(|r| base.join(r))
            .collect();
        assert_eq!(got, want, "live/**/prod must also match nested prod dirs");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn expand_roots_literal_dot_matches_base() {
        let base = make_glob_tree("dot");
        let roots = expand_roots(&base, &[".".into()]).unwrap();
        let canon_base = std::fs::canonicalize(&base).unwrap();
        let matched: Vec<_> = roots
            .into_iter()
            .filter_map(|p| std::fs::canonicalize(&p).ok())
            .collect();
        assert!(
            matched.contains(&canon_base),
            "literal '.' must match the base directory, got {matched:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn expand_roots_zero_match_is_error() {
        let base = make_glob_tree("zero");
        let err = expand_roots(&base, &["nope/*".into()]).unwrap_err();
        assert!(
            matches!(err, crate::error::Error::Config(_)),
            "zero match must be a Config error, got {err:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn expand_roots_invalid_pattern_is_error() {
        let base = make_glob_tree("invalid");
        // An unterminated character class is not a valid glob pattern.
        let err = expand_roots(&base, &["live/*/prod[".into()]).unwrap_err();
        assert!(
            matches!(err, crate::error::Error::Config(_)),
            "invalid pattern must be a Config error, got {err:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn expand_roots_dedups_overlapping_patterns() {
        let base = make_glob_tree("dedup");
        let roots = expand_roots(&base, &["live/*/prod".into(), "live/a/prod".into()]).unwrap();
        let got = as_set(roots);
        assert_eq!(
            got.iter().filter(|p| p.ends_with("live/a/prod")).count(),
            1,
            "live/a/prod must appear once despite two overlapping patterns"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn parse_paths_recurses_into_matched_dirs() {
        let base = make_glob_tree("parsepaths");
        let roots = expand_roots(&base, &["live/*/prod".into()]).unwrap();
        let manifests = parse_paths(&roots).await.unwrap();
        // live/a/prod -> x.yaml, sub/y.yaml ; live/b/prod -> z.yaml
        assert_eq!(
            manifests.len(),
            3,
            "all 3 ConfigMaps under matched prod dirs must be parsed"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
