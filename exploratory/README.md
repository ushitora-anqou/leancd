# leancd vs Argo CD — exploratory sync comparison

Exploratory test harness that stands up **two kind clusters** — one running
**leancd**, one running **Argo CD** — pointed at the same Forgejo Git repo, then
drives identical operations against the repo and the live clusters to see
whether the two reconcile the same way. Any divergence where leancd does not
match Argo CD's outcome is recorded as a leancd bug.

## Layout
- `setup.sh` / `teardown.sh` — bring up / tear down the 2 kind clusters plus a
  shared host-side Forgejo container on the `kind` Docker network (both
  controllers reach it at the container's Docker IP).
- `deploy-leancd.sh` / `deploy-argocd.sh` — deploy each controller against the
  shared repo, syncing repo root into namespace `app` with Server-Side Apply.
- `manifests/` — leancd Deployment/RBAC/Secret, Argo CD AppProject/Application +
  repository Secret. `__FORGEJO_GIT_URL__` is substituted at deploy time.
- `Dockerfile.build` — builds leancd **inside** the container. This exists
  because the project's production `Dockerfile` has a build-reproducibility bug
  (see `notes/bugs.md` BUG 1) that shipped a `fn main(){}` dummy binary, and
  because the NixOS-host-built binary's dynamic linker is missing in
  `debian:bookworm-slim`.
- `lib/common.sh` — shared constants, `kc_lean`/`kc_argo` wrappers, `wait_for`.
- `lib/git.sh` — host-side git push/clone against Forgejo + `wait_sync`
  (converges both controllers onto a repo HEAD).
- `lib/compare.sh` — `normalize` (strip status / server-defaults / manager
  labels) and `compare_resource` (normalized JSON diff between clusters).
- `run.sh` — runs the 10 scenarios and appends to `report.md`.
- `report.md` — the generated comparison report.
- `notes/bugs.md` — leancd bugs found during the run.

## Running
```sh
./setup.sh
docker build -t leancd:latest -f exploratory/Dockerfile.build .
kind load docker-image leancd:latest --name leancd-compare
./deploy-leancd.sh
./deploy-argocd.sh
./run.sh            # writes report.md
./teardown.sh
```

## Judgement criteria (agreed with the user)
The **primary** check is **final-state equality** of the synced resources
between the two clusters (normalized JSON diff). Detection-timing differences
(leancd polls on an interval; Argo CD watches) are treated as **design
differences, not bugs**. Where leancd's final state diverges from Argo CD's, or
where it fails to converge, that is recorded as a leancd bug.

## Scenarios
1. Initial apply (ConfigMap + Deployment + Service)
2. Add a resource (cm-b)
3. Update a resource (cm-a data)
4. Delete a resource (prune cm-b)
5. Drift self-heal (live spec mutation)
6. Drift self-heal (live resource deletion)
7. SSA field-manager conflict
8. CRD + custom resource
9. Cluster-scoped + other-namespace resources
10. Multi-document YAML + kind:List
