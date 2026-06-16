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

(Scenarios below are appended as they run.)

## Scenario 1 — Initial apply (ConfigMap + Deployment + Service)

Push **ConfigMap cm-a**, **Deployment demo**, **Service demo** to Git (empty repo → 3 resources). Both controllers reconcile from scratch.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "0"
    last_sha: 4abb07f7bcb66ce7a78d0250e7c00313b2176b0f
    managed_count: "3"
    sync_count: "3"
```
**[after] leancd recent log**:
```
  2026-06-16T00:10:23.942051Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "example.com", version: "v1", kind: "Widget", namespace: Some("app"), name: "w1" }
  2026-06-16T00:10:23.943962Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("extra-ns"), name: "cm-extra" }
  2026-06-16T00:10:23.945425Z  INFO leancd::reconcile: reconciliation complete sha=4abb07f7bcb66ce7a78d0250e7c00313b2176b0f force=false full=true managed=3 pruned=9 drift=0
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Normalized comparison**:
  [=] MATCH: app/configmap/cm-a
  [=] MATCH: app/deployment/demo
  [=] MATCH: app/service/demo

## Scenario 2 — Add a resource (cm-b)

Add a new **ConfigMap cm-b**; existing resources unchanged.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "2"
    last_sha: 23b9a511c4588b09ce3b2b7fbfbe8545a8fa5aaf
    managed_count: "4"
    sync_count: "5"
```
**[after] leancd recent log**:
```
  2026-06-16T00:11:24.503830Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "cm-b" } reason=spec differs from desired state
  2026-06-16T00:11:24.503832Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=2
  2026-06-16T00:11:24.514240Z  INFO leancd::reconcile: reconciliation complete sha=23b9a511c4588b09ce3b2b7fbfbe8545a8fa5aaf force=false full=false managed=4 pruned=0 drift=2
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Normalized comparison**:
  [=] MATCH: app/configmap/cm-b
  [=] MATCH: app/configmap/cm-a
  [=] MATCH: app/deployment/demo

## Scenario 3 — Update a resource (cm-a data)

Update **cm-a** `data.version` from "1" to "2".

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "0"
    last_sha: dda7a1a73a4366684f0762cf874d4fd8cf0fe9e8
    managed_count: "4"
    sync_count: "6"
```
**[after] leancd recent log**:
```
  2026-06-16T00:11:24.503832Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=2
  2026-06-16T00:11:24.514240Z  INFO leancd::reconcile: reconciliation complete sha=23b9a511c4588b09ce3b2b7fbfbe8545a8fa5aaf force=false full=false managed=4 pruned=0 drift=2
  2026-06-16T00:11:54.816835Z  INFO leancd::reconcile: reconciliation complete sha=dda7a1a73a4366684f0762cf874d4fd8cf0fe9e8 force=false full=true managed=4 pruned=0 drift=0
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Normalized comparison** (expect version=2 in both):
  [=] MATCH: app/configmap/cm-a

## Scenario 4 — Delete a resource (prune cm-b)

Remove **cm-b** from Git; both controllers should prune it.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "1"
    last_sha: bc7b4abcfa4e4ebdb2373ea6359b25fa4d9b0d3c
    managed_count: "3"
    sync_count: "8"
```
**[after] leancd recent log**:
```
  2026-06-16T00:12:55.367644Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "cm-a" } reason=spec differs from desired state
  2026-06-16T00:12:55.367657Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=1
  2026-06-16T00:12:55.377900Z  INFO leancd::reconcile: reconciliation complete sha=bc7b4abcfa4e4ebdb2373ea6359b25fa4d9b0d3c force=false full=false managed=3 pruned=0 drift=1
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Prune comparison**:
  [=] leancd: cm-b pruned
  [=] argocd: cm-b pruned
**Survivors unchanged**:
  [=] MATCH: app/configmap/cm-a
  [=] MATCH: app/deployment/demo

## Scenario 6 — Drift self-heal (live resource deletion)

Delete **Deployment demo** live in each cluster. Both should recreate it.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "2"
    last_sha: bc7b4abcfa4e4ebdb2373ea6359b25fa4d9b0d3c
    managed_count: "3"
    sync_count: "9"
```
**[after] leancd recent log**:
```
  2026-06-16T00:13:25.648500Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "apps", version: "v1", kind: "Deployment", namespace: Some("app"), name: "demo" } reason=missing in cluster
  2026-06-16T00:13:25.648502Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=2
  2026-06-16T00:13:25.721791Z  INFO leancd::reconcile: reconciliation complete sha=bc7b4abcfa4e4ebdb2373ea6359b25fa4d9b0d3c force=false full=false managed=3 pruned=0 drift=2
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Re-create comparison**:
  [=] leancd: demo recreated
  [=] argocd: demo recreated
  [=] MATCH: app/deployment/demo

## Scenario 8 — CRD + custom resource

Push a CRD (widgets.example.com) and a Widget CR (w1). Both controllers must discover the new kind and apply it.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "2"
    last_sha: f5ec60d03ec949d5caaa58cf0971bcc04859e6c5
    managed_count: "5"
    sync_count: "11"
```
**[after] leancd recent log**:
```
  2026-06-16T00:14:26.273690Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "example.com", version: "v1", kind: "Widget", namespace: Some("app"), name: "w1" } reason=missing in cluster
  2026-06-16T00:14:26.273693Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=2
  2026-06-16T00:14:26.376617Z  INFO leancd::reconcile: reconciliation complete sha=f5ec60d03ec949d5caaa58cf0971bcc04859e6c5 force=false full=false managed=5 pruned=0 drift=2
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**CRD comparison** (cluster-scoped):
  [=] MATCH: customresourcedefinition/widgets.example.com
**Widget CR comparison** (needs CRD established first):
  [=] MATCH: app/widget/w1

## Scenario 9 — Cluster-scoped + other-namespace resources

Push a Namespace (extra-ns), a ConfigMap in extra-ns (cm-extra — a namespace *other* than the sync target app), and a ClusterRole (extra-role). Tests cluster-scoped apply and resources outside LEANCD_NAMESPACE.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "4"
    last_sha: 0e627862c8337d0a21fb95b781929d0dc51cffeb
    managed_count: "8"
    sync_count: "13"
```
**[after] leancd recent log**:
```
  2026-06-16T00:15:26.949254Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("extra-ns"), name: "cm-extra" } reason=spec differs from desired state
  2026-06-16T00:15:26.949256Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=4
  2026-06-16T00:15:26.969873Z  INFO leancd::reconcile: reconciliation complete sha=0e627862c8337d0a21fb95b781929d0dc51cffeb force=false full=false managed=8 pruned=0 drift=4
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Cluster-scoped comparison**:
  [=] MATCH: namespace/extra-ns
  [=] MATCH: clusterrole/extra-role
**Cross-namespace ConfigMap (extra-ns)**:
  [=] MATCH: extra-ns/configmap/cm-extra

## Scenario 10 — Multi-document YAML + kind:List

Push a file with two YAML documents (multi-a, multi-b) and a kind:List with two items (list-a, list-b). Both controllers must expand and apply all four ConfigMaps.

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "8"
    last_sha: 059315e207c91e1ae8ed07de9bef181ea9d05ccc
    managed_count: "12"
    sync_count: "15"
```
**[after] leancd recent log**:
```
  2026-06-16T00:16:27.532128Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("extra-ns"), name: "cm-extra" } reason=spec differs from desired state
  2026-06-16T00:16:27.532130Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=8
  2026-06-16T00:16:27.559582Z  INFO leancd::reconcile: reconciliation complete sha=059315e207c91e1ae8ed07de9bef181ea9d05ccc force=false full=false managed=12 pruned=0 drift=8
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Multi-doc / List comparison**:
  [=] MATCH: app/configmap/multi-a
  [=] MATCH: app/configmap/multi-b
  [=] MATCH: app/configmap/list-a
  [=] MATCH: app/configmap/list-b

## Scenario 5 — Drift self-heal (live spec mutation)

Live-mutate cm-a in each cluster (version 2→99, add `mutated-by: kubectl`). Both should self-heal back to Git (version=2, no mutated-by).

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "8"
    last_sha: 059315e207c91e1ae8ed07de9bef181ea9d05ccc
    managed_count: "12"
    sync_count: "17"
```
**[after] leancd recent log**:
```
  2026-06-16T00:17:28.220289Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("extra-ns"), name: "cm-extra" } reason=spec differs from desired state
  2026-06-16T00:17:28.220291Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=8
  2026-06-16T00:17:28.244928Z  INFO leancd::reconcile: reconciliation complete sha=059315e207c91e1ae8ed07de9bef181ea9d05ccc force=false full=false managed=12 pruned=0 drift=8
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Self-heal comparison**:
  [=] MATCH: app/configmap/cm-a

## Scenario 7 — SSA field-manager conflict

Apply cm-a with a competing field manager (`conflict-manager`, version=7, taken-by). leancd syncs with Server-Side Apply **without** --force; Argo CD uses Server-Side Apply. Question: does each reclaim the field to Git (version=2, no taken-by)?

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "8"
    last_sha: 059315e207c91e1ae8ed07de9bef181ea9d05ccc
    managed_count: "12"
    sync_count: "18"
```
**[after] leancd recent log**:
```
  2026-06-16T00:17:58.516844Z  WARN leancd::reconcile: drift detected key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("extra-ns"), name: "cm-extra" } reason=spec differs from desired state
  2026-06-16T00:17:58.516846Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=8
  2026-06-16T00:17:58.543545Z  INFO leancd::reconcile: reconciliation complete sha=059315e207c91e1ae8ed07de9bef181ea9d05ccc force=false full=false managed=12 pruned=0 drift=8
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Conflict comparison**:
  [=] MATCH: app/configmap/cm-a
  (if DIFF on data.version/taken-by → that controller did NOT reclaim the conflicting field)
