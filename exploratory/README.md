# Lean CD vs Argo CD — VictoriaMetrics K8s Stack exploratory sync comparison

Exploratory harness that stands up **two kind clusters** — one running
**Lean CD**, one running **Argo CD** — pointed at the same Forgejo Git repo, then
syncs the **VictoriaMetrics K8s Stack** Helm chart (rendered via `helm template`)
into both and drives identical operations against the repo and the live clusters
to see whether the two reconcile the same way. Any divergence where Lean CD's
final state differs from Argo CD's, or where Lean CD fails to converge, is
recorded as a Lean CD bug in `notes/bugs.md`.

This is a deliberately **hard** workload for a low-memory CD controller: ~10
operator CRDs (prepended via `helm show crds`, since `helm template` skips
`crds/`), the operator pattern (runtime-created child resources that are not in
Git), Grafana dashboard ConfigMaps that approach the 262144-byte annotation
limit, and a `helm.sh/hook: pre-delete` cleanup hook that Argo CD ignores but
Lean CD runs.

## Layout
- `setup.sh` / `teardown.sh` — bring up / tear down the 2 kind clusters + shared
  host-side Forgejo. `setup.sh` also installs the Argo CD controller (see
  `install-argocd.sh`) — previously this step was missing.
- `install-argocd.sh` — install the Argo CD controller (official v3 install
  manifest, `--server-side --force-conflicts`) into `argocd-compare`.
- `deploy-leancd.sh` / `deploy-argocd.sh` — point each controller at the shared
  repo, syncing repo root into namespace `app` with Server-Side Apply.
- `charts/<name>/` — one directory per compared Helm chart, each with a
  `render.sh` (calls `lib/chart.sh`) and a kind-tuned `values.yaml`:
  - `charts/vm-stack/` — VictoriaMetrics K8s Stack (`nameOverride: vmks`, Grafana
    persistence off + admin password pinned, VMSingle 2Gi, dashboards + VMRules
    ON).
  - `charts/cert-manager/` — cert-manager (CRDs + webhooks + install hooks); a
    second workload for hook-weight/delete-policy and CRD variety.
- `lib/chart.sh` — generic `helm show crds` + `helm template` renderer (CRDs
  first, then the rendered resources) shared by every `charts/*/render.sh`.
- `manifests/` — Lean CD Deployment/RBAC/Secret, Argo CD AppProject/Application +
  repository Secret. `__FORGEJO_GIT_URL__` is substituted at deploy time.
- `lib/common.sh` — shared constants, `kc_lean`/`kc_argo` wrappers, `wait_for`.
- `lib/git.sh` — host-side git push/clone against Forgejo + `wait_sync` (polls
  until BOTH controllers see HEAD and Lean CD's `drift_count==0`).
- `lib/compare.sh` — `normalize` (chart-profile-aware: `NORMALIZE_PROFILE=vm`
  drops operator/checksum annotations, `minimal` does not), `compare_resource`,
  `compare_secret`, `compare_count`.
- `lib/safety.sh` — safety assertions beyond final-state equality:
  `assert_drift_settled`, `assert_pruned`/`assert_kept`, `assert_field_reclaimed`
  (SSA), `assert_hook_order`.
- `run.sh` — runs the 10 VictoriaMetrics scenarios and writes `report.md`.
- `report.md` — the generated comparison report.
- `notes/bugs.md` — Lean CD bugs found during the run.

## Running
```sh
./setup.sh                                                  # clusters + Forgejo + Argo CD
docker build -t leancd:latest .
kind load docker-image leancd:latest --name leancd-compare
./deploy-leancd.sh
./deploy-argocd.sh
./run.sh            # renders the VM chart then writes report.md
./teardown.sh
```
(`run.sh` invokes `vm-stack/render.sh` itself, so the chart is rendered fresh
each run into the repo workdir before being pushed.)

## Judgment criteria (agreed with the user)
The **primary** check is **final-state equality** of the synced resources
between the two clusters (normalized JSON diff). Detection-timing differences
(Lean CD polls on an interval; Argo CD watches) are treated as **design
differences, not bugs**. Where Lean CD's final state diverges from Argo CD's, or
where it fails to converge, that is recorded as a Lean CD bug.

## Scenarios
1. Initial full VictoriaMetrics stack deploy (~135 docs, CRD + CR together)
2. Add a custom VMRule
3. Update the custom VMRule
4. Delete the custom VMRule (prune)
5. Drift self-heal (live mutation of a VMSingle CR spec)
6. Operator-created children coexist (neither controller prunes them)
7. Operator recreates a deleted child (neither controller owns it)
8. Large dashboard ConfigMaps under Server-Side Apply (262144-byte limit)
9. SSA field-manager conflict on a VMSingle CR (BUG 4 regression guard)
10. Full teardown + `pre-delete` hook divergence (Lean CD runs it, Argo CD ignores it)
