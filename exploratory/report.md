
## Scenario 11 — Standard resource kinds (server-default drift parity)

Standard resource kinds under the minimal normalize profile. Server-default fields (clusterIP, pathType, storageClass, PDB one-of, container defaults) must not read as drift.

**[after] Lean CD state** (ConfigMap `app/leancd-state`):
```yaml
  last_sha: "acf9b6e6f26efd18aff0e0cff1a7056cc4d45168"
  sync_count: "91"
  managed_count: "6"
  drift_count: "0"
  last_error: ""
```
**[after] Lean CD recent log**:
```
  2026-06-20T02:57:51.789410Z  INFO leancd::reconcile: reconciliation complete sha=38ec2fe84a5a3394522bf641ccc515eacea0ee49 full=false teardown=false managed=1 pruned=0 drift=0
  2026-06-20T02:58:22.205249Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "pf-main" }
  2026-06-20T02:58:22.206860Z  INFO leancd::reconcile: reconciliation complete sha=acf9b6e6f26efd18aff0e0cff1a7056cc4d45168 full=true teardown=false managed=6 pruned=1 drift=0
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   OutOfSync     Progressing
```
**Resource comparison (expect MATCH in both clusters)**:
  [=] MATCH: app/statefulset/cmp-sts
  [=] MATCH: app/daemonset/cmp-ds
  [=] MATCH: app/service/cmp-svc
  [=] MATCH: app/ingress/cmp-ing
  [=] MATCH: app/poddisruptionbudget/cmp-pdb
  [=] MATCH (data key-set): app/secret/cmp-sec  keys=[key]
**Safety: Lean CD must have settled (drift_count==0, no re-apply loop)**:
  [PASS] drift settled (drift_count=0 across two reads)

## Scenario 20 — Helm hook-weight ordering (Argo CD parity)

PreSync hooks at distinct weights. Lean CD runs Helm hooks in ascending weight (hooks.rs::sort_by_weight), matching Argo CD. Both hooks run in both clusters and the main ConfigMap applies after.

**[after] Lean CD state** (ConfigMap `app/leancd-state`):
```yaml
  last_sha: "c9c240bc39e5234dc0a5d9943b7d2e4ae8c2e6b8"
  sync_count: "92"
  managed_count: "1"
  drift_count: "0"
  last_error: ""
```
**[after] Lean CD recent log**:
```
  2026-06-20T02:58:52.535076Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "networking.k8s.io", version: "v1", kind: "Ingress", namespace: Some("app"), name: "cmp-ing" }
  2026-06-20T02:58:52.538631Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "Service", namespace: Some("app"), name: "cmp-svc" }
  2026-06-20T02:58:52.542418Z  INFO leancd::reconcile: reconciliation complete sha=c9c240bc39e5234dc0a5d9943b7d2e4ae8c2e6b8 full=true teardown=false managed=1 pruned=6 drift=0
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   OutOfSync     Progressing
```
**Hook-run presence (both clusters)**:
  [!] Lean CD: hook-w-minus5 MISSING
  [=] argocd: hook-w-minus5 ran
  [!] Lean CD: hook-w-plus5 MISSING
  [!] argocd: hook-w-plus5 MISSING
**Main applied after hooks (expect MATCH)**:
  [!] MISSING in argocd: app/configmap/hw-main
**Safety: Lean CD ran hooks in non-decreasing weight (log scan)**:
  [FAIL] hook weights NOT ascending: -5 5 -5 5 -5 5 -5 5 

## Scenario 21 — PostSync hook failure keeps main

A PostSync hook that fails must NOT undo the main apply — reconcile records the failure (state.last_error) but the pass returns Ok (non-fatal), so the main ConfigMap stays applied in both clusters.

**[after] Lean CD state** (ConfigMap `app/leancd-state`):
```yaml
  last_sha: "1aa427c1531dffc6eab75d0339713f6add444650"
  sync_count: "93"
  managed_count: "1"
  drift_count: "0"
  last_error: "post-sync hook failed: hook completed with failure"
```
**[after] Lean CD recent log**:
```
  2026-06-20T02:59:26.838622Z  WARN leancd::hooks: helm hook failed phase=PostSync key=ResourceKey { group: "batch", version: "v1", kind: "Job", namespace: Some("app"), name: "pf-post" } reason=hook completed with failure
  2026-06-20T02:59:26.842814Z  INFO leancd::prune: pruned resource no longer in Git key=ResourceKey { group: "", version: "v1", kind: "ConfigMap", namespace: Some("app"), name: "hw-main" }
  2026-06-20T02:59:26.845848Z  INFO leancd::reconcile: reconciliation complete sha=1aa427c1531dffc6eab75d0339713f6add444650 full=true teardown=false managed=1 pruned=1 drift=0
```
**[after] Argo CD app**:
```
  NAME      SYNC STATUS   HEALTH STATUS
  compare   OutOfSync     Progressing
```
**Main survives the PostSync failure (expect MATCH)**:
  [!] MISSING in argocd: app/configmap/pf-main
**PostSync hook ran and failed (Lean CD)**:
  [=] Lean CD: pf-post applied (failed=1)
