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

/// A namespaced StatefulSet. Carries `volumeClaimTemplates` — the field most
/// prone to server-default drift (resource defaults) — and a matching
/// `serviceName`/selector. The companion headless Service is NOT generated:
/// drift/prune comparison only needs the spec to apply, not the Pods to run.
pub fn statefulset(name: &str, ns: &str, image: &str, replicas: u32) -> String {
    let mut s = String::new();
    s.push_str("apiVersion: apps/v1\nkind: StatefulSet\nmetadata:\n");
    s.push_str(&format!("  name: {name}\n"));
    if !ns.is_empty() {
        s.push_str(&format!("  namespace: {ns}\n"));
    }
    s.push_str("spec:\n");
    s.push_str(&format!("  replicas: {replicas}\n"));
    s.push_str(&format!("  serviceName: \"{name}\"\n"));
    s.push_str(&format!(
        "  selector:\n    matchLabels:\n      app: \"{name}\"\n"
    ));
    s.push_str(&format!(
        "  template:\n    metadata:\n      labels:\n        app: \"{name}\"\n"
    ));
    s.push_str("    spec:\n      containers:\n");
    s.push_str(&format!(
        "        - name: app\n          image: \"{image}\"\n          imagePullPolicy: IfNotPresent\n"
    ));
    s.push_str("  volumeClaimTemplates:\n    - metadata:\n        name: data\n      spec:\n        accessModes: [\"ReadWriteOnce\"]\n        resources:\n          requests:\n            storage: 1Gi\n");
    s
}

/// A namespaced DaemonSet. `spec.updateStrategy` and node-affinity defaults are
/// the server-injected fields most likely to read as drift.
pub fn daemonset(name: &str, ns: &str, image: &str) -> String {
    let mut s = String::new();
    s.push_str("apiVersion: apps/v1\nkind: DaemonSet\nmetadata:\n");
    s.push_str(&format!("  name: {name}\n"));
    if !ns.is_empty() {
        s.push_str(&format!("  namespace: {ns}\n"));
    }
    s.push_str("spec:\n");
    s.push_str(&format!(
        "  selector:\n    matchLabels:\n      app: \"{name}\"\n"
    ));
    s.push_str(&format!(
        "  template:\n    metadata:\n      labels:\n        app: \"{name}\"\n"
    ));
    s.push_str("    spec:\n      containers:\n");
    s.push_str(&format!(
        "        - name: app\n          image: \"{image}\"\n          imagePullPolicy: IfNotPresent\n"
    ));
    s
}

/// A namespaced Service. Server-injected defaults (`clusterIP`, `ipFamilies`,
/// `internalTrafficPolicy`, `sessionAffinity`, `ports[].protocol`) are the
/// drift-prone fields; compare.sh::normalize strips the cluster-wide ones.
pub fn service(name: &str, ns: &str, port: u32) -> String {
    let mut s = String::new();
    s.push_str("apiVersion: v1\nkind: Service\nmetadata:\n");
    s.push_str(&format!("  name: {name}\n"));
    if !ns.is_empty() {
        s.push_str(&format!("  namespace: {ns}\n"));
    }
    s.push_str("spec:\n");
    s.push_str(&format!("  selector:\n    app: \"{name}\"\n"));
    s.push_str(&format!(
        "  ports:\n    - port: {port}\n      targetPort: {port}\n"
    ));
    s
}

/// A namespaced Ingress (networking.k8s.io/v1). `pathType` defaults to
/// `Prefix` server-side (k8s >= 1.18) — a classic drift trap when left unset.
pub fn ingress(name: &str, ns: &str, host: &str, svc: &str, port: u32) -> String {
    let mut s = String::new();
    s.push_str("apiVersion: networking.k8s.io/v1\nkind: Ingress\nmetadata:\n");
    s.push_str(&format!("  name: {name}\n"));
    if !ns.is_empty() {
        s.push_str(&format!("  namespace: {ns}\n"));
    }
    s.push_str("spec:\n");
    s.push_str(&format!(
        "  rules:\n    - host: \"{host}\"\n      http:\n        paths:\n          - path: /\n            pathType: Prefix\n            backend:\n              service:\n                name: \"{svc}\"\n                port:\n                  number: {port}\n"
    ));
    s
}

/// A namespaced PodDisruptionBudget specifying only `minAvailable` (not
/// `maxUnavailable`) to exercise the server's one-of handling. The selector is
/// non-empty (policy/v1 rejects an empty selector).
pub fn pdb(name: &str, ns: &str, min_available: &str) -> String {
    let mut s = String::new();
    s.push_str("apiVersion: policy/v1\nkind: PodDisruptionBudget\nmetadata:\n");
    s.push_str(&format!("  name: {name}\n"));
    if !ns.is_empty() {
        s.push_str(&format!("  namespace: {ns}\n"));
    }
    s.push_str("spec:\n");
    s.push_str(&format!("  minAvailable: \"{min_available}\"\n"));
    s.push_str(&format!(
        "  selector:\n    matchLabels:\n      app: \"{name}\"\n"
    ));
    s
}

/// A namespaced HorizontalPodAutoscaler (autoscaling/v2) targeting a
/// Deployment. `spec.metrics` gains server-injected defaults to watch for drift.
pub fn hpa(name: &str, ns: &str, target_deploy: &str, min: u32, max: u32) -> String {
    let mut s = String::new();
    s.push_str("apiVersion: autoscaling/v2\nkind: HorizontalPodAutoscaler\nmetadata:\n");
    s.push_str(&format!("  name: {name}\n"));
    if !ns.is_empty() {
        s.push_str(&format!("  namespace: {ns}\n"));
    }
    s.push_str("spec:\n");
    s.push_str(&format!(
        "  scaleTargetRef:\n    apiVersion: apps/v1\n    kind: Deployment\n    name: \"{target_deploy}\"\n"
    ));
    s.push_str(&format!("  minReplicas: {min}\n  maxReplicas: {max}\n"));
    s.push_str("  metrics:\n    - type: Resource\n      resource:\n        name: cpu\n        target:\n          type: Utilization\n          averageUtilization: 80\n");
    s
}

/// A namespaced Secret using `stringData` (the Git side). k8s converts it to
/// base64 `data` on apply; drift.rs strips `stringData` before comparing
/// (BUG 9). The harness compares Secret data key-sets, not values.
pub fn secret_stringdata(name: &str, ns: &str, data: &[(&str, &str)]) -> String {
    let mut s = String::new();
    s.push_str("apiVersion: v1\nkind: Secret\nmetadata:\n");
    s.push_str(&format!("  name: {name}\n"));
    if !ns.is_empty() {
        s.push_str(&format!("  namespace: {ns}\n"));
    }
    s.push_str("type: Opaque\nstringData:\n");
    for (k, v) in data {
        s.push_str(&format!("  {k}: \"{v}\"\n"));
    }
    s
}
