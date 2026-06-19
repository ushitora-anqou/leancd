#!/usr/bin/env bash
# Drive the exploratory VictoriaMetrics comparison scenarios and write report.md.
# Replaces the prior generic (ConfigMap) scenarios. Scenarios are cumulative:
# each builds on the repo state left by the previous, except S9/S10 which are
# last because they mutate/teardown state.
set -u   # no -e / no pipefail: a scenario returning non-zero (e.g. a DIFF from
         # compare_resource, or a kubectl timeout) must not abort the whole run.
source "$(dirname "$0")/lib/common.sh"
source "$(dirname "$0")/lib/git.sh"
source "$(dirname "$0")/lib/compare.sh"

REPORT_FILE="$EXPLORATORY_DIR/report.md"
BUGS_FILE="$NOTES_DIR/bugs.md"
VM_STACK_DIR="$EXPLORATORY_DIR/vm-stack"

report_header() {
    local doc_count=0 crd_count=0 biggest=0 argocd_image=unknown
    if [[ -f "$REPO_WORKDIR/vm-stack.yaml" ]]; then
        doc_count="$(grep -c '^---' "$REPO_WORKDIR/vm-stack.yaml" || true)"
        crd_count="$(grep -c '^kind: CustomResourceDefinition' "$REPO_WORKDIR/vm-stack.yaml" || true)"
        biggest="$(awk 'BEGIN{RS="\n---\n"} {n=length($0); if(n>max){max=n}} END{print max+0}' "$REPO_WORKDIR/vm-stack.yaml")"
    fi
    argocd_image="$(kc_argo get deploy argocd-server -n argocd \
        -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || echo unknown)"
    cat > "$REPORT_FILE" <<EOF
# leancd vs Argo CD — VictoriaMetrics K8s Stack Comparison Report

Comparison of **leancd** and **Argo CD** reconciling the **VictoriaMetrics K8s
Stack** Helm chart (rendered via \`helm template\` into namespace \`app\`) from
the same Forgejo Git repository, with identical operations applied to the repo
and the live clusters.

**Judgement criteria (agreed):** the *primary* check is **final-state equality**
of the synced resources between the two clusters (normalized JSON diff).
Detection-timing differences (leancd polls on an interval; Argo CD watches) are
treated as design differences, **not** bugs. Where leancd's final state diverges
from Argo CD's, or where it fails to converge, that is recorded as a **leancd
bug** in \`notes/bugs.md\`.

## Environment
- Two kind clusters: \`leancd-compare\` (runs leancd, 30s poll, force-conflict
  Server-Side Apply) and \`argocd-compare\` (runs Argo CD: \`${argocd_image}\`,
  automated.prune + selfHeal, ServerSideApply=true).
- One host-side Forgejo container on the \`kind\` Docker network holds the shared
  repo \`leancd/compare.git\`; both controllers sync repo root (\`.\`) into
  namespace **\`app\`**.
- Chart: \`vm/victoria-metrics-k8s-stack@0.84.0\` rendered to **${doc_count}
  docs** (**${crd_count} CRDs** prepended via \`helm show crds\`, since \`helm
  template\` does not emit the chart's \`crds/\` dir; largest single doc
  **${biggest} bytes** vs the k8s 262144-byte annotation limit).
- **Operator pattern:** the \`vmks-victoria-metrics-operator\` Deployment creates
  child Deployments/Services for the VMSingle/VMAlert/VMAgent CRs at runtime.
  Those children are **NOT in Git** and carry **no managed-by label** — both
  controllers are expected to leave them alone (S6/S7).
EOF
    echo "" >> "$REPORT_FILE"
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

# Resolve an operator-managed child Deployment name (NOT in Git) in a cluster.
# Args: <lean|argo>
operator_child_deploy() {
    local side="$1"
    if [[ "$side" == "lean" ]]; then
        kc_lean get deploy -n app -o name 2>/dev/null \
            | grep -ivE 'operator|grafana|kube-state|node-exporter' | head -1 | sed 's#deployment/##'
    else
        kc_argo get deploy -n app -o name 2>/dev/null \
            | grep -ivE 'operator|grafana|kube-state|node-exporter' | head -1 | sed 's#deployment/##'
    fi
}

# ===========================================================================
# Scenario 1 — Initial full VictoriaMetrics stack deploy
# ===========================================================================
scenario_01_initial() {
    # vm-stack.yaml was rendered into REPO_WORKDIR by render.sh before the run.
    push_repo "scenario 1: initial VM stack deploy"
    wait_sync 180   # CRD establishment + operator boot + child reconcile
    {
        echo "Push the rendered VictoriaMetrics stack (~135 docs: CRDs + operator + grafana + KSM + node-exporter + VMSingle/VMAlert/VMAgent/VMAlertmanager CRs + 39 VMRules + dashboard ConfigMaps). Both controllers reconcile from scratch."
        snapshot after
        echo "**Representative-resource comparison**:"
        compare_resource customresourcedefinition vmsingles.operator.victoriametrics.com
        compare_resource deployment vmks-victoria-metrics-operator app
        compare_resource deployment vmks-grafana app
        compare_resource vmsingle vmks app
        compare_resource vmalert vmks app
        compare_resource vmagent vmks app
        echo "**Resource counts (app ns)**:"
        compare_count vmrule app
        compare_count configmap app
        compare_count secret app
    } | report_section 1 "Initial full VictoriaMetrics stack deploy"
}

# ===========================================================================
# Scenario 2 — Add a custom VMRule
# ===========================================================================
scenario_02_add_vmrule() {
    write_file app-rules.yaml <<'EOF'
apiVersion: operator.victoriametrics.com/v1beta1
kind: VMRule
metadata:
  name: extra-rule
  namespace: app
spec:
  groups:
    - name: extra
      rules:
        - record: extra:up
          expr: vector(1)
EOF
    push_repo "scenario 2: add custom VMRule extra-rule"
    wait_sync 60
    {
        echo "Add a custom **VMRule extra-rule** on top of the stack."
        snapshot after
        echo "**VMRule comparison**:"
        compare_resource vmrule extra-rule app
    } | report_section 2 "Add a custom VMRule"
}

# ===========================================================================
# Scenario 3 — Update the custom VMRule
# ===========================================================================
scenario_03_update_vmrule() {
    write_file app-rules.yaml <<'EOF'
apiVersion: operator.victoriametrics.com/v1beta1
kind: VMRule
metadata:
  name: extra-rule
  namespace: app
spec:
  groups:
    - name: extra
      rules:
        - record: extra:up
          expr: vector(2)
EOF
    push_repo "scenario 3: update extra-rule expr 1->2"
    wait_sync 60
    {
        echo "Update **extra-rule** recording-rule expr from \`vector(1)\` to \`vector(2)\`."
        snapshot after
        echo "**VMRule comparison** (expect expr=vector(2) in both):"
        compare_resource vmrule extra-rule app
    } | report_section 3 "Update the custom VMRule"
}

# ===========================================================================
# Scenario 4 — Delete the custom VMRule (prune)
# ===========================================================================
scenario_04_delete_vmrule() {
    remove_file app-rules.yaml
    push_repo "scenario 4: remove extra-rule (prune)"
    wait_sync 60
    {
        echo "Remove **extra-rule** from Git; both controllers should prune it."
        snapshot after
        echo "**Prune comparison**:"
        if exists_in lean vmrule extra-rule app; then echo "  [!] leancd: extra-rule STILL EXISTS (prune failed)"; else echo "  [=] leancd: extra-rule pruned"; fi
        if exists_in argo vmrule extra-rule app; then echo "  [!] argocd: extra-rule STILL EXISTS"; else echo "  [=] argocd: extra-rule pruned"; fi
        echo "**A stock VMRule should be unaffected**:"
        compare_resource vmrule vmks-alertmanager.rules app
    } | report_section 4 "Delete the custom VMRule (prune)"
}

# ===========================================================================
# Scenario 5 — Drift self-heal (VMSingle spec mutation)
# ===========================================================================
scenario_05_drift_vmsingle() {
    # Live-mutate the VMSingle CR spec.retentionPeriod ("1" -> "99") in BOTH.
    kc_lean patch vmsingle vmks -n app --type merge \
        -p '{"spec":{"retentionPeriod":"99"}}' >/dev/null 2>&1 || true
    kc_argo patch vmsingle vmks -n app --type merge \
        -p '{"spec":{"retentionPeriod":"99"}}' >/dev/null 2>&1 || true
    kc_argo patch application compare -n argocd --type merge \
        -p '{"metadata":{"annotations":{"argocd.argoproj.io/refresh":"hard"}}}' >/dev/null 2>&1 || true
    log "mutated VMSingle retentionPeriod live (1->99); waiting for self-heal..."
    sleep 60
    {
        echo "Live-mutate **VMSingle vmks** \`spec.retentionPeriod\` (\"1\" -> \"99\") in each cluster. Both should self-heal back to Git (\"1\") — leancd via force-conflict SSA, Argo CD via selfHeal."
        snapshot after
        echo "**Self-heal comparison** (expect retentionPeriod=\"1\" in both):"
        compare_resource vmsingle vmks app
    } | report_section 5 "Drift self-heal (VMSingle spec mutation)"
}

# ===========================================================================
# Scenario 6 — Operator-created children coexist (no prune fight)
# ===========================================================================
scenario_06_operator_children() {
    sleep 30   # ensure the operator has reconciled its children
    {
        echo "The victoria-metrics-operator creates child Deployments/Services for the VMSingle/VMAlert/VMAgent CRs at runtime. These are **not in Git** and carry **no managed-by label**, so both controllers must leave them alone (leancd prune safety-net + Argo CD prune both key off the managed-by label / tracked set)."
        snapshot after
        echo "**Operator-created children in leancd cluster (app ns)**:"
        kc_lean get deploy -n app 2>/dev/null | grep -ivE 'operator|grafana|kube-state|node-exporter' | sed 's/^/  /' | head -15
        echo "**Operator-created children in argocd cluster (app ns)**:"
        kc_argo get deploy -n app 2>/dev/null | grep -ivE 'operator|grafana|kube-state|node-exporter' | sed 's/^/  /' | head -15
        local child
        child="$(operator_child_deploy lean)"
        if [[ -n "$child" ]]; then
            echo "**Labels on child \`deployment/$child\` (should NOT have managed-by=leancd)**:"
            kc_lean get deploy "$child" -n app -o jsonpath='{.metadata.labels}' 2>/dev/null | jq -c . 2>/dev/null | sed 's/^/    leancd: /'
            kc_argo get deploy "$child" -n app -o jsonpath='{.metadata.labels}' 2>/dev/null | jq -c . 2>/dev/null | sed 's/^/    argocd:  /'
        fi
    } | report_section 6 "Operator-created children coexist"
}

# ===========================================================================
# Scenario 7 — Operator recreates a deleted child (neither controller involved)
# ===========================================================================
scenario_07_child_recreate() {
    local child
    child="$(operator_child_deploy lean)"
    if [[ -z "$child" ]]; then
        { echo "(no operator-managed child Deployment found; skipping.)"; } | report_section 7 "Operator child self-recreate (skipped)"
        return 0
    fi
    kc_lean delete deploy "$child" -n app >/dev/null 2>&1 || true
    kc_argo delete deploy "$child" -n app >/dev/null 2>&1 || true
    log "deleted operator child deployment/$child live; waiting for operator to recreate..."
    sleep 45
    {
        echo "Delete the operator-managed child **Deployment \`$child\`** live in each cluster. The victoria-metrics-operator (not leancd, not Argo CD) should recreate it. Confirms leancd/Argo do not own these children."
        snapshot after
        echo "**Re-create comparison**:"
        if exists_in lean deployment "$child" app; then echo "  [=] leancd: $child recreated (by operator)"; else echo "  [!] leancd: $child MISSING (no recreate)"; fi
        if exists_in argo deployment "$child" app; then echo "  [=] argocd: $child recreated (by operator)"; else echo "  [!] argocd: $child MISSING"; fi
    } | report_section 7 "Operator child self-recreate"
}

# ===========================================================================
# Scenario 8 — Large dashboard ConfigMaps under Server-Side Apply
# ===========================================================================
scenario_08_dashboards() {
    {
        echo "\`defaultDashboards\` generates Grafana dashboard ConfigMaps whose annotations can approach the k8s **262144-byte** annotation limit — a documented ArgoCD pain point. Both controllers use Server-Side Apply; this checks the dashboards landed in each cluster."
        echo ""
        echo "**Note on the annotation delta**: the rendered dashboard ConfigMaps carry labels only (no \`metadata.annotations\`). Any annotation seen only on the argocd side is Argo CD's injected \`argocd.argoproj.io/tracking-id\` (prune tracking; never in the source manifest), so leancd showing ~0B annotations here is the **expected** state — not a bug."
        snapshot after
        echo "**Dashboard ConfigMap presence + total annotation bytes**:"
        for name in vmks-grafana-overview vmks-etcd vmks-alertmanager-overview vmks-k8s-resources-cluster; do
            local lean_present="no" argo_present="no" lean_ann=0 argo_ann=0
            exists_in lean configmap "$name" app && lean_present="yes"
            exists_in argo configmap "$name" app && argo_present="yes"
            lean_ann="$(kc_lean get cm "$name" -n app -o json 2>/dev/null \
                | jq -r '[(.metadata.annotations // {} | to_entries[] | (.value | length))] | add // 0' 2>/dev/null || echo 0)"
            argo_ann="$(kc_argo get cm "$name" -n app -o json 2>/dev/null \
                | jq -r '[(.metadata.annotations // {} | to_entries[] | (.value | length))] | add // 0' 2>/dev/null || echo 0)"
            printf '  %-32s leancd=%s (ann~%sB)  argocd=%s (ann~%sB)\n' "$name" "$lean_present" "$lean_ann" "$argo_present" "$argo_ann"
        done
        echo "  (k8s caps a single annotation value at 262144 bytes)"
    } | report_section 8 "Large dashboard ConfigMaps under Server-Side Apply"
}

# ===========================================================================
# Scenario 9 — SSA field-manager conflict (BUG 4 regression guard)
# ===========================================================================
scenario_09_ssa_conflict() {
    # A competing field-manager takes retentionPeriod on the VMSingle CR.
    local body='apiVersion: operator.victoriametrics.com/v1beta1
kind: VMSingle
metadata:
  name: vmks
  namespace: app
spec:
  retentionPeriod: "7"
'
    echo "$body" | kc_lean apply --server-side --field-manager conflict-manager -f - >/dev/null 2>&1 || true
    echo "$body" | kc_argo apply --server-side --field-manager conflict-manager -f - >/dev/null 2>&1 || true
    kc_argo patch application compare -n argocd --type merge \
        -p '{"metadata":{"annotations":{"argocd.argoproj.io/refresh":"hard"}}}' >/dev/null 2>&1 || true
    log "applied VMSingle with field-manager=conflict-manager (retentionPeriod=7); waiting..."
    sleep 60
    {
        echo "Apply **VMSingle vmks** with a competing field manager (\`conflict-manager\`, retentionPeriod=\"7\"). Both should reclaim the field to Git (\"1\") — leancd always applies with force-conflict SSA; Argo CD via selfHeal. (BUG 4 regression guard.)"
        snapshot after
        echo "**Conflict comparison** (expect retentionPeriod=\"1\" in both):"
        compare_resource vmsingle vmks app
        echo "  (if DIFF on spec.retentionPeriod -> that controller did NOT reclaim the conflicting field)"
    } | report_section 9 "SSA field-manager conflict (VMSingle)"
}

# ===========================================================================
# Scenario 10 — Full teardown + pre-delete hook divergence
# ===========================================================================
scenario_10_teardown() {
    # Empty the repo: every YAML (vm-stack.yaml + app-rules.yaml) is removed.
    reset_repo
    push_repo "scenario 10: full teardown (empty repo)"
    # Nudge BOTH controllers: leancd polls every 30s, but Argo CD defaults to a
    # 3-min git poll — without a hard refresh it would not tear down in the
    # window, masking the pre-delete-hook comparison.
    kc_argo patch application compare -n argocd --type merge \
        -p '{"metadata":{"annotations":{"argocd.argoproj.io/refresh":"hard"}}}' >/dev/null 2>&1 || true
    sleep 90   # leancd: pre-delete hook + prune; Argo CD: prune (no pre-delete hook)
    {
        echo "Remove the **entire stack** from Git (empty repo). leancd detects a full teardown (main empty + prior applied non-empty) and runs the chart's **pre-delete Helm hook** (cleanup Job) before pruning; **Argo CD ignores pre-delete hooks** and prunes only. Compare what each cluster is left with."
        snapshot after
        echo "**Post-teardown managed-resource counts (app ns)**:"
        echo "  leancd deploy: $(kc_lean get deploy -n app -o name 2>/dev/null | wc -l | tr -d ' ')"
        echo "  argocd deploy: $(kc_argo get deploy -n app -o name 2>/dev/null | wc -l | tr -d ' ')"
        echo "  leancd vmrule: $(kc_lean get vmrule -n app -o name 2>/dev/null | wc -l | tr -d ' ')"
        echo "  argocd vmrule: $(kc_argo get vmrule -n app -o name 2>/dev/null | wc -l | tr -d ' ')"
        echo "**CRDs (cluster-scoped) — both should prune them**:"
        for crd in vmsingles.operator.victoriametrics.com vmrules.operator.victoriametrics.com vmagents.operator.victoriametrics.com; do
            local lean_s="absent" argo_s="absent"
            kc_lean get crd "$crd" >/dev/null 2>&1 && lean_s="present"
            kc_argo get crd "$crd" >/dev/null 2>&1 && argo_s="present"
            printf '  %-45s leancd=%s  argocd=%s\n' "$crd" "$lean_s" "$argo_s"
        done
        echo ""
        echo "**Expected divergence**: leancd runs the chart's \`helm.sh/hook: pre-delete\` cleanup resources; Argo CD does not. Both prune Git-managed objects; operator-created children linger until the operator notices their owning CR is gone."
    } | report_section 10 "Full teardown + pre-delete hook divergence"
}

# ---- main ----
# Order: cumulative scenarios first (S1-S8); then the state-mutating S9
# (field-manager conflict on the VMSingle CR) and the terminal S10 (teardown).
log "=== VictoriaMetrics comparison run starting ==="
init_repo
reset_repo
bash "$VM_STACK_DIR/render.sh"
report_header
scenario_01_initial  || true
scenario_02_add_vmrule || true
scenario_03_update_vmrule || true
scenario_04_delete_vmrule || true
scenario_05_drift_vmsingle || true
scenario_06_operator_children || true
scenario_07_child_recreate || true
scenario_08_dashboards || true
scenario_09_ssa_conflict || true
scenario_10_teardown  || true
log "=== run complete; report at $REPORT_FILE ==="
