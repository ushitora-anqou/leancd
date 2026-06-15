#!/usr/bin/env bash
# Drive the exploratory comparison scenarios and append results to report.md.
# Scenarios are cumulative: each builds on the repo state left by the previous.
set -u   # NOTE: no -e and no pipefail — a scenario returning non-zero
         # (e.g. a DIFF from compare_resource) must not abort the whole run.
source "$(dirname "$0")/lib/common.sh"
source "$(dirname "$0")/lib/git.sh"
source "$(dirname "$0")/lib/compare.sh"

REPORT_FILE="$EXPLORATORY_DIR/report.md"
BUGS_FILE="$NOTES_DIR/bugs.md"

report_header() {
    cat > "$REPORT_FILE" <<'EOF'
# leancd vs Argo CD — Exploratory Sync Comparison Report

Comparison of the reconciliation behaviour of **leancd** and **Argo CD** when
driven against the same Forgejo Git repository, with identical operations
applied to the repo and the live clusters.

**Judgement criteria (agreed):** the *primary* check is **final-state equality**
of the synced resources between the two clusters. Detection-timing differences
(leancd polls on an interval; Argo CD watches) are treated as design
differences, **not** bugs. Where leancd's final state diverges from Argo CD's,
or where it fails to converge, that is recorded as a **leancd bug**.

## Environment
- Two kind clusters: `leancd-compare` (runs leancd) and `argocd-compare`
  (runs Argo CD v3.4.3).
- One host-side Forgejo container on Docker network `kind` holds the shared repo
  `leancd/compare.git`. Both controllers reach it at
  `http://<forgejo-ip>:3000/leancd/compare.git` (resolved at setup time).
- Both controllers sync repo root (`.`) into namespace **`app`** with
  Server-Side Apply. Argo CD: `automated.prune + selfHeal`. leancd: 5s poll.
- The test driver pushes to the repo over `http://127.0.0.1:3000` and compares
  each cluster with `kubectl` (normalized JSON diff, see `lib/compare.sh`).

EOF
    echo "(Scenarios below are appended as they run.)" >> "$REPORT_FILE"
}

# Append a scenario section. Args: number, title. Body read from stdin.
report_section() {
    local num="$1" title="$2"
    printf '\n## Scenario %s — %s\n\n' "$num" "$title" >> "$REPORT_FILE"
    cat >> "$REPORT_FILE"
}

# Snapshot both controllers' status into the report (markdown).
snapshot() {
    local label="$1"
    echo
    echo "**[$label] leancd state** (ConfigMap \`app/leancd-state\`):"
    echo '```yaml'
    kc_lean get configmap leancd-state -n app -o yaml 2>/dev/null \
        | sed -n '/^data:/,/^status:/p' | grep -E 'last_sha|sync_count|managed_count|drift_count|last_error' \
        | sed 's/^/  /' || echo "  (state ConfigMap absent)"
    echo '```'
    echo "**[$label] leancd recent log**:"
    echo '```'
    kc_lean logs deploy/leancd -n leancd --tail=3 2>/dev/null \
        | sed 's/\x1b\[[0-9;]*m//g' | sed 's/^/  /'
    echo '```'
    echo "**[$label] Argo CD app**:"
    echo '```'
    kc_argo get application compare -n argocd 2>/dev/null | sed 's/^/  /'
    echo '```'
}

# ===========================================================================
# Scenario 1 — Initial apply (ConfigMap + Deployment + Service)
# ===========================================================================
scenario_01_initial() {
    reset_repo
    write_file app/cm-a.yaml <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: cm-a
  namespace: app
data:
  greeting: hello
  version: "1"
EOF
    write_file app/deploy.yaml <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: demo
  namespace: app
spec:
  replicas: 1
  selector: { matchLabels: { app: demo } }
  template:
    metadata: { labels: { app: demo } }
    spec:
      containers:
        - name: demo
          image: nginxinc/nginx-unprivileged:alpine
          ports: [{ containerPort: 8080 }]
EOF
    write_file app/svc.yaml <<'EOF'
apiVersion: v1
kind: Service
metadata:
  name: demo
  namespace: app
spec:
  selector: { app: demo }
  ports: [{ port: 80, targetPort: 8080 }]
EOF
    push_repo "scenario 1: initial apply"
    wait_sync 45
    {
        echo "Push **ConfigMap cm-a**, **Deployment demo**, **Service demo** to Git (empty repo → 3 resources). Both controllers reconcile from scratch."
        snapshot after
        echo "**Normalized comparison**:"
        compare_resource configmap cm-a app
        compare_resource deployment demo app
        compare_resource service demo app
    } | report_section 1 "Initial apply (ConfigMap + Deployment + Service)"
}

# ===========================================================================
# Scenario 2 — Add a resource (cm-b)
# ===========================================================================
scenario_02_add() {
    write_file app/cm-b.yaml <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: cm-b
  namespace: app
data:
  note: added-later
EOF
    push_repo "scenario 2: add cm-b"
    wait_sync 45
    {
        echo "Add a new **ConfigMap cm-b**; existing resources unchanged."
        snapshot after
        echo "**Normalized comparison**:"
        compare_resource configmap cm-b app
        compare_resource configmap cm-a app
        compare_resource deployment demo app
    } | report_section 2 "Add a resource (cm-b)"
}

# ===========================================================================
# Scenario 3 — Update a resource (cm-a data)
# ===========================================================================
scenario_03_update() {
    write_file app/cm-a.yaml <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: cm-a
  namespace: app
data:
  greeting: hello
  version: "2"
EOF
    push_repo "scenario 3: update cm-a version 1->2"
    wait_sync 45
    {
        echo "Update **cm-a** \`data.version\` from \"1\" to \"2\"."
        snapshot after
        echo "**Normalized comparison** (expect version=2 in both):"
        compare_resource configmap cm-a app
    } | report_section 3 "Update a resource (cm-a data)"
}

# ===========================================================================
# Scenario 4 — Delete a resource (prune cm-b)
# ===========================================================================
scenario_04_delete() {
    remove_file app/cm-b.yaml
    push_repo "scenario 4: remove cm-b (prune)"
    wait_sync 45
    {
        echo "Remove **cm-b** from Git; both controllers should prune it."
        snapshot after
        echo "**Prune comparison**:"
        if exists_in lean configmap cm-b app; then echo "  [!] leancd: cm-b STILL EXISTS (prune did not happen)"; else echo "  [=] leancd: cm-b pruned"; fi
        if exists_in argo configmap cm-b app; then echo "  [!] argocd: cm-b STILL EXISTS"; else echo "  [=] argocd: cm-b pruned"; fi
        echo "**Survivors unchanged**:"
        compare_resource configmap cm-a app
        compare_resource deployment demo app
    } | report_section 4 "Delete a resource (prune cm-b)"
}

# ===========================================================================
# Scenario 5 — Drift self-heal (live spec mutation)
# ===========================================================================
scenario_05_drift_spec() {
    # Mutate cm-a live in BOTH clusters with kubectl; neither Git HEAD changes.
    kc_lean patch configmap cm-a -n app --type merge \
        -p '{"data":{"version":"99","mutated-by":"kubectl"}}' >/dev/null 2>&1 || true
    kc_argo patch configmap cm-a -n app --type merge \
        -p '{"data":{"version":"99","mutated-by":"kubectl"}}' >/dev/null 2>&1 || true
    log "mutated cm-a live (version=99); waiting for self-heal..."
    kc_argo patch application compare -n argocd --type merge \
        -p '{"metadata":{"annotations":{"argocd.argoproj.io/refresh":"hard"}}}' >/dev/null 2>&1 || true
    sleep 45  # leancd 15s poll + Argo CD self-heal
    {
        echo "Live-mutate cm-a in each cluster (version 2→99, add \`mutated-by: kubectl\`). Both should self-heal back to Git (version=2, no mutated-by)."
        snapshot after
        echo "**Self-heal comparison**:"
        compare_resource configmap cm-a app
    } | report_section 5 "Drift self-heal (live spec mutation)"
}

# ===========================================================================
# Scenario 6 — Drift self-heal (live resource deletion)
# ===========================================================================
scenario_06_drift_delete() {
    kc_lean delete deployment demo -n app >/dev/null 2>&1 || true
    kc_argo delete deployment demo -n app >/dev/null 2>&1 || true
    log "deleted deployment demo live; waiting for re-create..."
    kc_argo patch application compare -n argocd --type merge \
        -p '{"metadata":{"annotations":{"argocd.argoproj.io/refresh":"hard"}}}' >/dev/null 2>&1 || true
    sleep 45
    {
        echo "Delete **Deployment demo** live in each cluster. Both should recreate it."
        snapshot after
        echo "**Re-create comparison**:"
        if exists_in lean deployment demo app; then echo "  [=] leancd: demo recreated"; else echo "  [!] leancd: demo MISSING (no self-heal)"; fi
        if exists_in argo deployment demo app; then echo "  [=] argocd: demo recreated"; else echo "  [!] argocd: demo MISSING"; fi
        compare_resource deployment demo app
    } | report_section 6 "Drift self-heal (live resource deletion)"
}

# ===========================================================================
# Scenario 7 — SSA field-manager conflict
# ===========================================================================
scenario_07_conflict() {
    # Apply a competing owner of cm-a in both clusters (version=7, taken-by).
    local body='apiVersion: v1
kind: ConfigMap
metadata:
  name: cm-a
  namespace: app
data:
  version: "7"
  taken-by: conflict-manager
'
    echo "$body" | kc_lean apply --server-side --field-manager conflict-manager -f - >/dev/null 2>&1 || true
    echo "$body" | kc_argo apply --server-side --field-manager conflict-manager -f - >/dev/null 2>&1 || true
    log "applied cm-a with field-manager=conflict-manager (version=7); waiting for controllers..."
    kc_argo patch application compare -n argocd --type merge \
        -p '{"metadata":{"annotations":{"argocd.argoproj.io/refresh":"hard"}}}' >/dev/null 2>&1 || true
    sleep 45
    {
        echo "Apply cm-a with a competing field manager (\`conflict-manager\`, version=7, taken-by). leancd syncs with Server-Side Apply **without** --force; Argo CD uses Server-Side Apply. Question: does each reclaim the field to Git (version=2, no taken-by)?"
        snapshot after
        echo "**Conflict comparison**:"
        compare_resource configmap cm-a app
        echo "  (if DIFF on data.version/taken-by → that controller did NOT reclaim the conflicting field)"
    } | report_section 7 "SSA field-manager conflict"
}

# ===========================================================================
# Scenario 8 — CRD + custom resource
# ===========================================================================
scenario_08_crd() {
    write_file app/crd.yaml <<'EOF'
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: widgets.example.com
spec:
  group: example.com
  names: { kind: Widget, plural: widgets, singular: widget }
  scope: Namespaced
  versions:
    - name: v1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
          properties:
            spec:
              type: object
              properties:
                size: { type: integer }
EOF
    write_file app/widget.yaml <<'EOF'
apiVersion: example.com/v1
kind: Widget
metadata:
  name: w1
  namespace: app
spec:
  size: 3
EOF
    push_repo "scenario 8: CRD + custom resource"
    wait_sync 60  # CRD establishment + discovery takes longer
    {
        echo "Push a CRD (widgets.example.com) and a Widget CR (w1). Both controllers must discover the new kind and apply it."
        snapshot after
        echo "**CRD comparison** (cluster-scoped):"
        compare_resource customresourcedefinition widgets.example.com
        echo "**Widget CR comparison** (needs CRD established first):"
        compare_resource widget w1 app
    } | report_section 8 "CRD + custom resource"
}

# ===========================================================================
# Scenario 9 — Cluster-scoped + other-namespace resources
# ===========================================================================
scenario_09_clusterscope() {
    write_file app/ns-extra.yaml <<'EOF'
apiVersion: v1
kind: Namespace
metadata:
  name: extra-ns
EOF
    write_file app/cm-extra.yaml <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: cm-extra
  namespace: extra-ns
data:
  place: other-namespace
EOF
    write_file app/clusterrole.yaml <<'EOF'
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: extra-role
rules:
  - apiGroups: [""]
    resources: ["configmaps"]
    verbs: ["get"]
EOF
    push_repo "scenario 9: cluster-scoped + cross-namespace"
    wait_sync 60
    {
        echo "Push a Namespace (extra-ns), a ConfigMap in extra-ns (cm-extra — a namespace *other* than the sync target app), and a ClusterRole (extra-role). Tests cluster-scoped apply and resources outside LEANCD_NAMESPACE."
        snapshot after
        echo "**Cluster-scoped comparison**:"
        compare_resource namespace extra-ns
        compare_resource clusterrole extra-role
        echo "**Cross-namespace ConfigMap (extra-ns)**:"
        compare_resource configmap cm-extra extra-ns
    } | report_section 9 "Cluster-scoped + other-namespace resources"
}

# ===========================================================================
# Scenario 10 — Multi-document YAML + kind:List
# ===========================================================================
scenario_10_multidoc() {
    write_file app/multi.yaml <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: multi-a
  namespace: app
data:
  from: multidoc
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: multi-b
  namespace: app
data:
  from: multidoc
EOF
    write_file app/list.yaml <<'EOF'
apiVersion: v1
kind: List
items:
  - apiVersion: v1
    kind: ConfigMap
    metadata:
      name: list-a
      namespace: app
    data: { from: list }
  - apiVersion: v1
    kind: ConfigMap
    metadata:
      name: list-b
      namespace: app
    data: { from: list }
EOF
    push_repo "scenario 10: multidoc + List"
    wait_sync 60
    {
        echo "Push a file with two YAML documents (multi-a, multi-b) and a kind:List with two items (list-a, list-b). Both controllers must expand and apply all four ConfigMaps."
        snapshot after
        echo "**Multi-doc / List comparison**:"
        compare_resource configmap multi-a app
        compare_resource configmap multi-b app
        compare_resource configmap list-a app
        compare_resource configmap list-b app
    } | report_section 10 "Multi-document YAML + kind:List"
}

# ---- main ----
# Order: the non-conflicting scenarios first, then the SSA-conflict scenarios
# (5, 7) last — they leave cm-a in a field-manager-conflict state that would
# skew later comparisons.
log "=== exploratory comparison run starting ==="
report_header
init_repo
# Each scenario is run with `|| true` so that one returning non-zero (e.g. a
# DIFF from compare_resource, or a kubectl timeout under leancd's re-apply load)
# does not abort the whole run.
scenario_01_initial     || true
scenario_02_add         || true
scenario_03_update      || true
scenario_04_delete      || true
scenario_06_drift_delete || true
scenario_08_crd         || true
scenario_09_clusterscope || true
scenario_10_multidoc    || true
scenario_05_drift_spec  || true
scenario_07_conflict    || true
log "=== run complete; report at $REPORT_FILE ==="
