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

/// Escape a string for a YAML double-quoted scalar (backslash and double-quote).
fn yaml_dq(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `metadata.annotations` block for a Helm hook: always `helm.sh/hook`, plus
/// optional `helm.sh/hook-weight` and `helm.sh/hook-delete-policy`. Indented to
/// sit under `  annotations:`.
fn hook_annotations(hook: &str, weight: Option<i64>, delete_policy: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str(&format!("    helm.sh/hook: \"{hook}\"\n"));
    if let Some(w) = weight {
        s.push_str(&format!("    helm.sh/hook-weight: \"{w}\"\n"));
    }
    if let Some(p) = delete_policy {
        s.push_str(&format!("    helm.sh/hook-delete-policy: \"{p}\"\n"));
    }
    s
}

/// A Helm-hook Job (`batch/v1`) in `ns`. Runs `/bin/sh -c <script>` in the
/// `leancd:latest` image (present on the kind node, `IfNotPresent`). The hook
/// annotations live on the Job's own `metadata` (leancd classifies on the
/// resource metadata, not the Pod template). `backoffLimit: 0` +
/// `restartPolicy: Never` fix a failure in a single attempt so the Job's
/// `.status.failed` settles at 1 (no retry churn while leancd polls).
pub fn job_hook(
    name: &str,
    ns: &str,
    hook: &str,
    weight: Option<i64>,
    delete_policy: Option<&str>,
    script: &str,
) -> String {
    let mut s = String::new();
    s.push_str("apiVersion: batch/v1\nkind: Job\nmetadata:\n");
    s.push_str(&format!("  name: {name}\n"));
    if !ns.is_empty() {
        s.push_str(&format!("  namespace: {ns}\n"));
    }
    s.push_str("  annotations:\n");
    s.push_str(&hook_annotations(hook, weight, delete_policy));
    s.push_str("spec:\n");
    s.push_str("  backoffLimit: 0\n");
    s.push_str("  template:\n");
    s.push_str("    spec:\n");
    s.push_str("      restartPolicy: Never\n");
    s.push_str("      containers:\n");
    s.push_str("        - name: hook\n");
    s.push_str("          image: leancd:latest\n");
    s.push_str("          imagePullPolicy: IfNotPresent\n");
    s.push_str("          command: [\"/bin/sh\", \"-c\"]\n");
    s.push_str(&format!("          args: [\"{}\"]\n", yaml_dq(script)));
    s
}

/// A Helm-hook Pod (core `v1`) with `restartPolicy: Never`. Same annotation /
/// script model as [`job_hook`]. A Pod has no `backoffLimit`; `Never` leaves a
/// failed Pod in `phase=Failed`, which is what leancd's completion poll reads.
pub fn pod_hook(
    name: &str,
    ns: &str,
    hook: &str,
    weight: Option<i64>,
    delete_policy: Option<&str>,
    script: &str,
) -> String {
    let mut s = String::new();
    s.push_str("apiVersion: v1\nkind: Pod\nmetadata:\n");
    s.push_str(&format!("  name: {name}\n"));
    if !ns.is_empty() {
        s.push_str(&format!("  namespace: {ns}\n"));
    }
    s.push_str("  annotations:\n");
    s.push_str(&hook_annotations(hook, weight, delete_policy));
    s.push_str("spec:\n");
    s.push_str("  restartPolicy: Never\n");
    s.push_str("  containers:\n");
    s.push_str("    - name: hook\n");
    s.push_str("      image: leancd:latest\n");
    s.push_str("      imagePullPolicy: IfNotPresent\n");
    s.push_str("      command: [\"/bin/sh\", \"-c\"]\n");
    s.push_str(&format!("      args: [\"{}\"]\n", yaml_dq(script)));
    s
}

/// A ConfigMap carrying `helm.sh/resource-policy: keep` — a "main" resource
/// (not a hook) that survives pruning when it leaves Git.
pub fn configmap_keep(name: &str, ns: &str, data: &[(&str, &str)]) -> String {
    let mut s = String::new();
    s.push_str("apiVersion: v1\nkind: ConfigMap\nmetadata:\n");
    s.push_str(&format!("  name: {name}\n"));
    if !ns.is_empty() {
        s.push_str(&format!("  namespace: {ns}\n"));
    }
    s.push_str("  annotations:\n");
    s.push_str("    helm.sh/resource-policy: \"keep\"\n");
    s.push_str("data:\n");
    for (k, v) in data {
        s.push_str(&format!("  {k}: \"{v}\"\n"));
    }
    s
}
