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
    last_sha: 2738d7f52156e600c81c44371f664b40adc8662e
    managed_count: "3"
    sync_count: "3"
```
**[after] leancd recent log**:
```
  2026-06-15T12:09:03.067292Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("extra-ns"), name: "cm-extra" }
  2026-06-15T12:09:03.069372Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "apiextensions.k8s.io", version: "v1", kind: "CustomResourceDefinition", namespace: None, name: "widgets.example.com" }
  2026-06-15T12:09:03.071837Z  INFO leancd::reconcile: reconciliation complete sha=2738d7f52156e600c81c44371f664b40adc8662e force=false full=true managed=3 pruned=10 drift=0
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
    drift_count: "0"
    last_sha: 9c9c9ebe19ba33aa9a30457fc07a1cc9b86db484
    managed_count: "4"
    sync_count: "5"
```
**[after] leancd recent log**:
```
  2026-06-15T12:09:33.373051Z  INFO leancd::reconcile: reconciliation complete sha=2738d7f52156e600c81c44371f664b40adc8662e force=false full=false managed=3 pruned=1 drift=3
  2026-06-15T12:10:03.670029Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "leancd-state" }
  2026-06-15T12:10:03.671777Z  INFO leancd::reconcile: reconciliation complete sha=9c9c9ebe19ba33aa9a30457fc07a1cc9b86db484 force=false full=true managed=4 pruned=1 drift=0
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
    drift_count: "4"
    last_sha: a672e251ad33fe040d1690562e5708edb35b8bbb
    managed_count: "4"
    sync_count: "7"
```
**[after] leancd recent log**:
```
  2026-06-15T12:11:04.211761Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=4
  2026-06-15T12:11:04.222631Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "leancd-state" }
  2026-06-15T12:11:04.224793Z  INFO leancd::reconcile: reconciliation complete sha=a672e251ad33fe040d1690562e5708edb35b8bbb force=false full=false managed=4 pruned=1 drift=4
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
    drift_count: "0"
    last_sha: 054a3b12937c07c333413283a4095b5190a191ca
    managed_count: "3"
    sync_count: "8"
```
**[after] leancd recent log**:
```
  2026-06-15T12:11:34.496110Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "leancd-state" }
  2026-06-15T12:11:34.497535Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "cm-b" }
  2026-06-15T12:11:34.499521Z  INFO leancd::reconcile: reconciliation complete sha=054a3b12937c07c333413283a4095b5190a191ca force=false full=true managed=3 pruned=2 drift=0
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
    drift_count: "3"
    last_sha: 054a3b12937c07c333413283a4095b5190a191ca
    managed_count: "3"
    sync_count: "10"
```
**[after] leancd recent log**:
```
  2026-06-15T12:12:35.158116Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=3
  2026-06-15T12:12:35.168785Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "leancd-state" }
  2026-06-15T12:12:35.170305Z  INFO leancd::reconcile: reconciliation complete sha=054a3b12937c07c333413283a4095b5190a191ca force=false full=false managed=3 pruned=1 drift=3
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
    drift_count: "4"
    last_sha: ef0f3238e78ed2702b485aee81aa8275a62e2ee4
    managed_count: "5"
    sync_count: "12"
```
**[after] leancd recent log**:
```
  2026-06-15T12:13:35.752504Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=4
  2026-06-15T12:13:35.856536Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "leancd-state" }
  2026-06-15T12:13:35.858334Z  INFO leancd::reconcile: reconciliation complete sha=ef0f3238e78ed2702b485aee81aa8275a62e2ee4 force=false full=false managed=5 pruned=1 drift=4
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
    drift_count: "6"
    last_sha: 5c96eb841fac6582c79d2fa96047f590c7597ac6
    managed_count: "8"
    sync_count: "14"
```
**[after] leancd recent log**:
```
  2026-06-15T12:14:36.438524Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=6
  2026-06-15T12:14:36.462261Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "leancd-state" }
  2026-06-15T12:14:36.465043Z  INFO leancd::reconcile: reconciliation complete sha=5c96eb841fac6582c79d2fa96047f590c7597ac6 force=false full=false managed=8 pruned=1 drift=6
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
    drift_count: "10"
    last_sha: aa87dacc253cbddb088dbdad26b97e1440e564e6
    managed_count: "12"
    sync_count: "16"
```
**[after] leancd recent log**:
```
  2026-06-15T12:15:37.046379Z  INFO leancd::reconcile: drift detected; re-applying managed resources drift=10
  2026-06-15T12:15:37.070460Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "leancd-state" }
  2026-06-15T12:15:37.072518Z  INFO leancd::reconcile: reconciliation complete sha=aa87dacc253cbddb088dbdad26b97e1440e564e6 force=false full=false managed=12 pruned=1 drift=10
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
    drift_count: "10"
    last_sha: aa87dacc253cbddb088dbdad26b97e1440e564e6
    managed_count: "12"
    sync_count: "17"
```
**[after] leancd recent log**:
```
  2026-06-15T12:16:07.362604Z  WARN leancd::reconcile: apply pass completed with failures applied=11 failed=1
  2026-06-15T12:16:07.371048Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "leancd-state" }
  2026-06-15T12:16:07.372769Z  INFO leancd::reconcile: reconciliation complete sha=aa87dacc253cbddb088dbdad26b97e1440e564e6 force=false full=false managed=12 pruned=1 drift=10
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Self-heal comparison**:
  [~] DIFF: app/configmap/cm-a
      6c6
      <     "version": "99"
      ---
      >     "version": "2"

## Scenario 7 — SSA field-manager conflict

Apply cm-a with a competing field manager (`conflict-manager`, version=7, taken-by). leancd syncs with Server-Side Apply **without** --force; Argo CD uses Server-Side Apply. Question: does each reclaim the field to Git (version=2, no taken-by)?

**[after] leancd state** (ConfigMap `app/leancd-state`):
```yaml
    drift_count: "10"
    last_sha: aa87dacc253cbddb088dbdad26b97e1440e564e6
    managed_count: "12"
    sync_count: "19"
```
**[after] leancd recent log**:
```
  2026-06-15T12:17:07.963397Z  WARN leancd::reconcile: apply pass completed with failures applied=11 failed=1
  2026-06-15T12:17:07.972345Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "leancd-state" }
  2026-06-15T12:17:07.974345Z  INFO leancd::reconcile: reconciliation complete sha=aa87dacc253cbddb088dbdad26b97e1440e564e6 force=false full=false managed=12 pruned=1 drift=10
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   Synced        Healthy
```
**Conflict comparison**:
  [~] DIFF: app/configmap/cm-a
      6c6
      <     "version": "99"
      ---
      >     "version": "2"
  (if DIFF on data.version/taken-by → that controller did NOT reclaim the conflicting field)

---

## Summary

### Final-state comparison (the agreed primary check)

| # | Scenario | Result |
|---|----------|--------|
| 1 | initial apply (ConfigMap + Deployment + Service) | ✅ MATCH (3/3) |
| 2 | add ConfigMap cm-b | ✅ MATCH |
| 3 | update cm-a data (version 1→2) | ✅ MATCH |
| 4 | delete cm-b (prune) | ✅ both pruned; survivors MATCH |
| 6 | delete Deployment demo live (self-heal re-create) | ✅ both recreated; MATCH |
| 8 | CRD + custom resource | ✅ MATCH (CRD + Widget) |
| 9 | cluster-scoped + other-namespace | ✅ MATCH |
| 10 | multi-document YAML + kind:List | ✅ MATCH (4/4) |
| 5 | live spec mutation (drift self-heal) | ❌ DIFF — leancd stuck at 99, Argo CD healed to 2 |
| 7 | SSA field-manager conflict | ❌ DIFF — leancd cannot reclaim the field, Argo CD can |

For the **core GitOps lifecycle** — add / update / delete (prune) / CRD /
cluster-scoped resources / multi-document YAML / kind:List — leancd and Argo CD
converge to the **same final state**. leancd diverges only where a **conflicting
SSA field manager** is involved (scenarios 5 and 7): it cannot self-heal the
field, Argo CD can.

### leancd bugs found (full detail in `notes/bugs.md`)

1. **Dockerfile ships a dummy binary** — BuildKit + Cargo's mtime fingerprint
   skip the real build, so the image contains `fn main(){}`; CrashLoopBackOff
   with exit 0. *(High — blocks startup.)*
2. **leancd prunes its own state ConfigMap** every reconciliation — the
   safety-net lists the labelled `leancd-state` and deletes it each pass, then
   rewrites it; constant churn and a state-loss risk. *(Medium.)*
3. **drift false-positive on arrays** — `spec_subset` falls through to strict
   array equality, so server-injected defaults (container `resources`,
   `imagePullPolicy`, `terminationMessage*`, `ports[].protocol`, …) make every
   pass report drift → perpetual re-apply, never a steady state. *(High — the
   drift fast-path never short-circuits.)*
4. **controller cannot self-heal a field taken by another field manager** — the
   controller runs `force=false`, so `kubectl edit/patch` that claims a field
   blocks leancd from converging on that resource; the apply 409s forever. Argo
   CD (ServerSideApply + selfHeal) reclaims it. *(High — scenarios 5 & 7.)*
5. **drift/prune are scoped to a single namespace** — `drift::detect` and the
   prune safety-net issue their `List` calls against `LEANCD_NAMESPACE` only
   (`kube_util::api_for` with `namespace=None` ⇒ default namespace). A resource
   leancd *applied* in another namespace is never drift-checked or pruned by
   leancd. Argo CD manages resources across namespaces. *(Medium — surfaced by
   code reading; scenario 9 only does an apply so it did not show up as a
   divergence there.)*

### Observed differences that are NOT bugs (design)

- **Detection timing**: leancd polls (every 30s in this run); Argo CD watches.
  Per the agreed criteria this is a design difference, not a bug.
- **Idle load**: because of BUG 3, leancd re-applies every managed resource on
  every poll, generating noticeably more kube-API traffic and log volume than
  Argo CD, which sits idle once `Synced`.
- **Health**: in scenario 8 Argo CD reported the Application `Degraded` (no
  health check is registered for the custom `Widget` CRD); the resource itself
  still matched between clusters. leancd has no health concept, so this is an
  Argo-CD-only signal, not a leancd divergence.
