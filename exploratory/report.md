# leancd vs Argo CD — VictoriaMetrics K8s Stack Comparison Report

Comparison of **leancd** and **Argo CD** reconciling the **VictoriaMetrics K8s
Stack** Helm chart (rendered via `helm template` into namespace `app`) from
the same Forgejo Git repository, with identical operations applied to the repo
and the live clusters.

**Judgement criteria (agreed):** the *primary* check is **final-state equality**
of the synced resources between the two clusters (normalized JSON diff).
Detection-timing differences (leancd polls on an interval; Argo CD watches) are
treated as design differences, **not** bugs. Where leancd's final state diverges
from Argo CD's, or where it fails to converge, that is recorded as a **leancd
bug** in `notes/bugs.md`.

## Environment
- Two kind clusters: `leancd-compare` (runs leancd, 30s poll, force-conflict
  Server-Side Apply) and `argocd-compare` (runs Argo CD: `quay.io/argoproj/argocd:v3.4.4`,
  automated.prune + selfHeal, ServerSideApply=true).
- One host-side Forgejo container on the `kind` Docker network holds the shared
  repo `leancd/compare.git`; both controllers sync repo root (`.`) into
  namespace **`app`**.
- Chart: `vm/victoria-metrics-k8s-stack@0.84.0` rendered to **135
  docs** (**24 CRDs** prepended via `helm show crds`, since `helm
  template` does not emit the chart's `crds/` dir; largest single doc
  **739756 bytes** vs the k8s 262144-byte annotation limit).
- **Operator pattern:** the `vmks-victoria-metrics-operator` Deployment creates
  child Deployments/Services for the VMSingle/VMAlert/VMAgent CRs at runtime.
  Those children are **NOT in Git** and carry **no managed-by label** — both
  controllers are expected to leave them alone (S6/S7).

(Scenarios below are appended as they run.)

## Scenario 1 — Initial full VictoriaMetrics stack deploy

Push the rendered VictoriaMetrics stack (~135 docs: CRDs + operator + grafana + KSM + node-exporter + VMSingle/VMAlert/VMAgent/VMAlertmanager CRs + 39 VMRules + dashboard ConfigMaps). Both controllers reconcile from scratch.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "3"
    last_sha: b5cb4f92cc10791969023dd7005580e765460b0c
    managed_count: "131"
    sync_count: "1453"
```
**[after] leancd recent log**:
```
  2026-06-19T04:01:17.659351Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "apps", version: "v1", kind: "Deployment", namespace: Some("app"), name: "vmks-kube-state-metrics" } reason=spec differs from desired state
  2026-06-19T04:01:17.659354Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=3
  2026-06-19T04:01:19.008241Z  INFO leancd::reconcile: reconciliation complete sha=b5cb4f92cc10791969023dd7005580e765460b0c full=false teardown=false managed=131 pruned=0 drift=3
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Representative-resource comparison**:
  [=] MATCH: customresourcedefinition/vmsingles.operator.victoriametrics.com
  [=] MATCH: app/deployment/vmks-victoria-metrics-operator
  [=] MATCH: app/deployment/vmks-grafana
  [=] MATCH: app/vmsingle/vmks
  [=] MATCH: app/vmalert/vmks
  [=] MATCH: app/vmagent/vmks
**Resource counts (app ns)**:
  [=] MATCH count: app/vmrule  lean=39 argo=39
  [~] DIFF count: app/configmap  lean=24 argo=23
  [=] MATCH count: app/secret  lean=8 argo=8

## Scenario 2 — Add a custom VMRule

Add a custom **VMRule extra-rule** on top of the stack.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "3"
    last_sha: 18c7cad253b2831cc3d0bcf4aa0048e622e4230e
    managed_count: "132"
    sync_count: "1455"
```
**[after] leancd recent log**:
```
  2026-06-19T04:02:21.177691Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "apps", version: "v1", kind: "Deployment", namespace: Some("app"), name: "vmks-kube-state-metrics" } reason=spec differs from desired state
  2026-06-19T04:02:21.177694Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=3
  2026-06-19T04:02:22.526257Z  INFO leancd::reconcile: reconciliation complete sha=18c7cad253b2831cc3d0bcf4aa0048e622e4230e full=false teardown=false managed=132 pruned=0 drift=3
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**VMRule comparison**:
  [=] MATCH: app/vmrule/extra-rule

## Scenario 3 — Update the custom VMRule

Update **extra-rule** recording-rule expr from `vector(1)` to `vector(2)`.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "3"
    last_sha: 3be66779c2ce83abc8a72c63ed82c8794c812729
    managed_count: "132"
    sync_count: "1457"
```
**[after] leancd recent log**:
```
  2026-06-19T04:03:24.688814Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "apps", version: "v1", kind: "Deployment", namespace: Some("app"), name: "vmks-kube-state-metrics" } reason=spec differs from desired state
  2026-06-19T04:03:24.688816Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=3
  2026-06-19T04:03:26.046816Z  INFO leancd::reconcile: reconciliation complete sha=3be66779c2ce83abc8a72c63ed82c8794c812729 full=false teardown=false managed=132 pruned=0 drift=3
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**VMRule comparison** (expect expr=vector(2) in both):
  [=] MATCH: app/vmrule/extra-rule

## Scenario 4 — Delete the custom VMRule (prune)

Remove **extra-rule** from Git; both controllers should prune it.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "0"
    last_sha: 07fc3b7ceb6b5a698a7026ffb39618274b08673c
    managed_count: "131"
    sync_count: "1458"
```
**[after] leancd recent log**:
```
  2026-06-19T04:03:26.046816Z  INFO leancd::reconcile: reconciliation complete sha=3be66779c2ce83abc8a72c63ed82c8794c812729 full=false teardown=false managed=132 pruned=0 drift=3
  2026-06-19T04:03:57.750154Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "operator.victoriametrics.com", version: "v1beta1", kind: "VMRule", namespace: Some("app"), name: "extra-rule" }
  2026-06-19T04:03:57.753811Z  INFO leancd::reconcile: reconciliation complete sha=07fc3b7ceb6b5a698a7026ffb39618274b08673c full=true teardown=false managed=131 pruned=1 drift=0
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Prune comparison**:
  [=] leancd: extra-rule pruned
  [=] argocd: extra-rule pruned
**A stock VMRule should be unaffected**:
  [=] MATCH: app/vmrule/vmks-alertmanager.rules

## Scenario 5 — Drift self-heal (VMSingle spec mutation)

Live-mutate **VMSingle vmks** `spec.retentionPeriod` ("1" -> "99") in each cluster. Both should self-heal back to Git ("1") — leancd via force-conflict SSA, Argo CD via selfHeal.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "3"
    last_sha: 07fc3b7ceb6b5a698a7026ffb39618274b08673c
    managed_count: "131"
    sync_count: "1460"
```
**[after] leancd recent log**:
```
  2026-06-19T04:05:00.197449Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "apps", version: "v1", kind: "Deployment", namespace: Some("app"), name: "vmks-kube-state-metrics" } reason=spec differs from desired state
  2026-06-19T04:05:00.197451Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=3
  2026-06-19T04:05:01.534746Z  INFO leancd::reconcile: reconciliation complete sha=07fc3b7ceb6b5a698a7026ffb39618274b08673c full=false teardown=false managed=131 pruned=0 drift=3
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Self-heal comparison** (expect retentionPeriod="1" in both):
  [=] MATCH: app/vmsingle/vmks

## Scenario 6 — Operator-created children coexist

The victoria-metrics-operator creates child Deployments/Services for the VMSingle/VMAlert/VMAgent CRs at runtime. These are **not in Git** and carry **no managed-by label**, so both controllers must leave them alone (leancd prune safety-net + Argo CD prune both key off the managed-by label / tracked set).

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "3"
    last_sha: 07fc3b7ceb6b5a698a7026ffb39618274b08673c
    managed_count: "131"
    sync_count: "1461"
```
**[after] leancd recent log**:
```
  2026-06-19T04:05:31.962863Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "apps", version: "v1", kind: "Deployment", namespace: Some("app"), name: "vmks-kube-state-metrics" } reason=spec differs from desired state
  2026-06-19T04:05:31.962866Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=3
  2026-06-19T04:05:33.313983Z  INFO leancd::reconcile: reconciliation complete sha=07fc3b7ceb6b5a698a7026ffb39618274b08673c full=false teardown=false managed=131 pruned=0 drift=3
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Operator-created children in leancd cluster (app ns)**:
  NAME                             READY   UP-TO-DATE   AVAILABLE   AGE
  vmagent-vmks                     1/1     1            1           7m
  vmalert-vmks                     1/1     1            1           7m
  vmsingle-vmks                    1/1     1            1           7m
**Operator-created children in argocd cluster (app ns)**:
  NAME                             READY   UP-TO-DATE   AVAILABLE   AGE
  vmagent-vmks                     1/1     1            1           12h
  vmalert-vmks                     1/1     1            1           12h
  vmsingle-vmks                    1/1     1            1           12h
**Labels on child `deployment/deployment.apps/vmagent-vmks` (should NOT have managed-by=leancd)**:

## Scenario 7 — Operator child self-recreate

Delete the operator-managed child **Deployment `deployment.apps/vmagent-vmks`** live in each cluster. The victoria-metrics-operator (not leancd, not Argo CD) should recreate it. Confirms leancd/Argo do not own these children.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "3"
    last_sha: 07fc3b7ceb6b5a698a7026ffb39618274b08673c
    managed_count: "131"
    sync_count: "1463"
```
**[after] leancd recent log**:
```
  2026-06-19T04:06:35.539252Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "apps", version: "v1", kind: "Deployment", namespace: Some("app"), name: "vmks-kube-state-metrics" } reason=spec differs from desired state
  2026-06-19T04:06:35.539255Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=3
  2026-06-19T04:06:36.898224Z  INFO leancd::reconcile: reconciliation complete sha=07fc3b7ceb6b5a698a7026ffb39618274b08673c full=false teardown=false managed=131 pruned=0 drift=3
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Re-create comparison**:
  [!] leancd: deployment.apps/vmagent-vmks MISSING (no recreate)
  [!] argocd: deployment.apps/vmagent-vmks MISSING

## Scenario 8 — Large dashboard ConfigMaps under Server-Side Apply

`defaultDashboards` generates Grafana dashboard ConfigMaps whose annotations can approach the k8s **262144-byte** annotation limit — a documented ArgoCD pain point. Both controllers use Server-Side Apply; this checks the dashboards landed in each cluster.

**Note on the annotation delta**: the rendered dashboard ConfigMaps carry labels only (no `metadata.annotations`). Any annotation seen only on the argocd side is Argo CD's injected `argocd.argoproj.io/tracking-id` (prune tracking; never in the source manifest), so leancd showing ~0B annotations here is the **expected** state — not a bug.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "3"
    last_sha: 07fc3b7ceb6b5a698a7026ffb39618274b08673c
    managed_count: "131"
    sync_count: "1463"
```
**[after] leancd recent log**:
```
  2026-06-19T04:06:35.539252Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "apps", version: "v1", kind: "Deployment", namespace: Some("app"), name: "vmks-kube-state-metrics" } reason=spec differs from desired state
  2026-06-19T04:06:35.539255Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=3
  2026-06-19T04:06:36.898224Z  INFO leancd::reconcile: reconciliation complete sha=07fc3b7ceb6b5a698a7026ffb39618274b08673c full=false teardown=false managed=131 pruned=0 drift=3
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Dashboard ConfigMap presence + total annotation bytes**:
  vmks-grafana-overview            leancd=yes (ann~0B)  argocd=yes (ann~44B)
  vmks-etcd                        leancd=yes (ann~0B)  argocd=yes (ann~32B)
  vmks-alertmanager-overview       leancd=yes (ann~0B)  argocd=yes (ann~49B)
  vmks-k8s-resources-cluster       leancd=no (ann~0B)  argocd=no (ann~0B)
  (k8s caps a single annotation value at 262144 bytes)

## Scenario 9 — SSA field-manager conflict (VMSingle)

Apply **VMSingle vmks** with a competing field manager (`conflict-manager`, retentionPeriod="7"). Both should reclaim the field to Git ("1") — leancd always applies with force-conflict SSA; Argo CD via selfHeal. (BUG 4 regression guard.)

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "3"
    last_sha: 07fc3b7ceb6b5a698a7026ffb39618274b08673c
    managed_count: "131"
    sync_count: "1465"
```
**[after] leancd recent log**:
```
  2026-06-19T04:07:39.142830Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "apps", version: "v1", kind: "Deployment", namespace: Some("app"), name: "vmks-kube-state-metrics" } reason=spec differs from desired state
  2026-06-19T04:07:39.142833Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=3
  2026-06-19T04:07:40.482842Z  INFO leancd::reconcile: reconciliation complete sha=07fc3b7ceb6b5a698a7026ffb39618274b08673c full=false teardown=false managed=131 pruned=0 drift=3
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Conflict comparison** (expect retentionPeriod="1" in both):
  [=] MATCH: app/vmsingle/vmks
  (if DIFF on spec.retentionPeriod -> that controller did NOT reclaim the conflicting field)

## Scenario 10 — Full teardown + pre-delete hook divergence

Remove the **entire stack** from Git (empty repo). leancd detects a full teardown (main empty + prior applied non-empty) and runs the chart's **pre-delete Helm hook** (cleanup Job) before pruning; **Argo CD ignores pre-delete hooks** and prunes only. Compare what each cluster is left with.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "0"
    last_sha: 04d1d41880dcef9ff2f0fbbb845257a431f65783
    managed_count: "0"
    sync_count: "1468"
```
**[after] leancd recent log**:
```
  2026-06-19T04:08:12.136748Z  INFO leancd::reconcile: reconciliation complete sha=04d1d41880dcef9ff2f0fbbb845257a431f65783 full=true teardown=true managed=0 pruned=129 drift=0
  2026-06-19T04:08:42.425557Z  INFO leancd::reconcile: reconciliation complete sha=04d1d41880dcef9ff2f0fbbb845257a431f65783 full=false teardown=false managed=0 pruned=0 drift=0
  2026-06-19T04:09:12.708124Z  INFO leancd::reconcile: reconciliation complete sha=04d1d41880dcef9ff2f0fbbb845257a431f65783 full=false teardown=false managed=0 pruned=0 drift=0
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   OutOfSync     Healthy
```
**Post-teardown managed-resource counts (app ns)**:
  leancd deploy: 0
  argocd deploy: 6
  leancd vmrule: 0
  argocd vmrule: 39
**CRDs (cluster-scoped) — both should prune them**:
  vmsingles.operator.victoriametrics.com        leancd=absent  argocd=present
  vmrules.operator.victoriametrics.com          leancd=absent  argocd=present
  vmagents.operator.victoriametrics.com         leancd=absent  argocd=present

**Expected divergence**: leancd runs the chart's `helm.sh/hook: pre-delete` cleanup resources; Argo CD does not. Both prune Git-managed objects; operator-created children linger until the operator notices their owning CR is gone.
