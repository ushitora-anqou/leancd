//! Manifest fixtures shared across scenarios. These are plain YAML (no
//! managed-by label — leancd injects that at apply time).

/// A ConfigMap with string `data`.
pub fn configmap(name: &str, namespace: &str, data: &[(&str, &str)]) -> String {
    let mut s = String::new();
    s.push_str("apiVersion: v1\nkind: ConfigMap\nmetadata:\n");
    s.push_str(&format!("  name: {name}\n"));
    if !namespace.is_empty() {
        s.push_str(&format!("  namespace: {namespace}\n"));
    }
    s.push_str("data:\n");
    for (k, v) in data {
        s.push_str(&format!("  {k}: \"{v}\"\n"));
    }
    s
}

/// A cluster-scoped Namespace.
pub fn namespace(name: &str) -> String {
    format!("apiVersion: v1\nkind: Namespace\nmetadata:\n  name: {name}\n")
}

/// A minimal cluster-scoped ClusterRole.
pub fn clusterrole(name: &str) -> String {
    format!(
        "apiVersion: rbac.authorization.k8s.io/v1\nkind: ClusterRole\nmetadata:\n  name: {name}\nrules:\n  - apiGroups: [\"\"]\n    resources: [\"configmaps\"]\n    verbs: [\"get\"]\n"
    )
}
