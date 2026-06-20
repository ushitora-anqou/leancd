# leancd

[![CI](https://github.com/ushitora-anqou/leancd/actions/workflows/ci.yml/badge.svg)](https://github.com/ushitora-anqou/leancd/actions/workflows/ci.yml)

**Lean CD** is a minimal, low-memory Continuous Delivery controller for
Kubernetes. It syncs manifests from a Git repository into the cluster it runs
in, detects drift, and self-heals — like Argo CD or Flux CD, but with a hard
RSS budget: **≤ 100MiB**.

This is the single most important goal and is verified by an automated
benchmark (see [bench/](bench/)).

## Features

- Applies plain YAML manifests from Git (no Kustomize / Helm / Jsonnet).
- Detects Git changes by polling (`git fetch`, shallow clone).
- Detects cluster-side drift and re-applies the desired state.
- Prunes resources removed from Git, using **foreground cascade** deletion
  (`propagationPolicy: Foreground`) so dependents are removed before their
  owners — the same policy used for Helm-hook and full-teardown deletions.
- Honours **Helm hooks** in pre-rendered manifests with Argo CD-equivalent
  semantics (`pre-install`/`pre-upgrade` → before the apply, `post-install`/
  `post-upgrade` → after; `pre-delete`/`post-delete` on full teardown), plus
  `helm.sh/hook-weight`, `helm.sh/hook-delete-policy`, and
  `helm.sh/resource-policy: keep`. Job/Pod hooks are awaited to completion.
- CLI for manual sync and status. Server-side apply always claims conflicting fields.
- Backs off exponentially on consecutive sync failures and shuts down gracefully
  (finishing the in-flight pass on SIGTERM); `SIGHUP` reloads `RUST_LOG`.
- `leancd health` subcommand for `exec` liveness/readiness probes.
- `leancd --version` and the startup log report the embedded git SHA.
- Metrics exported over OTLP/HTTP (push), including `leancd_rss_bytes`.
- Handles **all** resource kinds, including CRDs and cluster-scoped resources.

## Non-goals (kept out to stay small and light)

Kustomize / Helm-chart *rendering* / Jsonnet, owner-reference traversal,
notifications, and a web UI — all deliberately omitted to stay small: Argo CD and
Flux CD ship these but run at hundreds of MiB to GiB of RSS, and leancd trades
them for the ≤100MiB budget. (Helm *hooks* in already-rendered YAML are
supported; chart templating is not.)

## Build

```sh
cargo build --release
```

The resulting binary is a single static-ish executable run as a `Deployment`.

## Usage

```
leancd controller [flags]      run as a long-lived controller (deploy this)
leancd sync    [flags]            run one reconciliation pass, then exit
leancd status  [flags]            print the last recorded sync state
leancd health  [flags]            check sync health for exec probes, then exit
```

All configuration is supplied via flags (or `LEANCD_*` environment variables).
Only credentials are read from a Secret (in-cluster) or the environment:
`GIT_USERNAME` / `GIT_PASSWORD` for HTTPS basic auth, or `GIT_SSH_KEY` for an
SSH private key (with an `ssh://` or `git@host:` repository URL).

Key flags:

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--repo-url` | `LEANCD_REPO_URL` | — | Git repository URL |
| `--branch` | `LEANCD_BRANCH` | main | branch to track |
| `--path` | `LEANCD_PATH` | . | glob patterns of manifest directories (recursive; repeatable; comma-separated via env, e.g. `live/*/prod`) |
| `--poll-interval` | `LEANCD_POLL_INTERVAL` | 60s | reconciliation interval |
| `--hook-timeout-secs` | `LEANCD_HOOK_TIMEOUT_SECS` | 300 | per-hook (Job/Pod) completion timeout before it is treated as failed |
| `--namespace` | `LEANCD_NAMESPACE` | default | leancd's namespace |

For the complete flag and environment-variable reference, authentication modes,
metrics, tuning, and troubleshooting, see
[doc/user-manual.md](doc/user-manual.md).

## Deploy

```sh
kubectl apply -f deploy/leancd.yaml
```

The manifest installs the Namespace, ServiceAccount, RBAC, and Deployment, and
points leancd at your OpenTelemetry Collector over OTLP/HTTP (leancd runs no
HTTP listener of its own). Edit the `LEANCD_*` env values for your repository,
and create the `leancd-git-credentials` Secret for private repos.

A prebuilt multi-arch image is published to GHCR on each `v*` tag — set
`image:` to `ghcr.io/ushitora-anqou/leancd:<tag>` (or `:latest`) instead of the
local `leancd:latest` (see [doc/release.md](doc/release.md)). For a tighter
production posture, [`deploy/leancd-namespaced.yaml`](deploy/leancd-namespaced.yaml)
binds leancd's permissions to selected namespaces (RoleBindings) and ships a
default-deny `NetworkPolicy`.

For a hands-on walkthrough deploying leancd into a local `kind` cluster
(including an optional in-cluster Forgejo Git server), see
[doc/tutorial.md](doc/tutorial.md).

## How it stays under 100MiB

leancd never builds an informer/cache of the cluster: every reconciliation
issues direct `List`/`Get`/`Patch` calls for exactly the resources declared in
Git. Git history is kept shallow (depth 1), YAML is parsed one document at a
time, runtime state is a single ConfigMap plus a managed-by label, and the
runtime is single-threaded (`tokio` `current_thread`). There is no
cluster-wide cache and no background state: each pass fetches only what it needs
via direct `List`/`Get`/`Patch` calls and discards it, which keeps the footprint
flat regardless of cluster size.

## Benchmark

```sh
make bench        # or: ./bench/bench.sh   — single run
make scale        # or: ./bench/scale.sh   — RSS across 8/15/20 namespaces
```

`bench` samples RSS from startup through steady state and asserts both the sync
**peak** and the **idle** value stay under 100MiB (tune with `RSS_BUDGET_MIB`,
`BENCH_SAMPLE_SECS`). `scale` repeats the run at increasing namespace counts and
prints a peak/idle table. Both need a `kind` cluster and are **not** part of
`nix flake check` (no Docker in the sandbox); run them manually or in an external
CI job — the scripts exit non-zero on a budget breach, so a regression fails the
run. See [bench/README.md](bench/README.md).

## End-to-end tests

```sh
make e2e        # kind cluster + in-cluster Forgejo + leancd
```

The e2e suite spins up an ephemeral `kind` cluster and runs **Forgejo and
leancd as in-cluster Pods** (leancd is built into a container image via the
root [`Dockerfile`](Dockerfile) and loaded into the kind node). It drives
leancd's intended behaviour end-to-end across ~35 scenarios: initial apply, Git
change detection + steady-state drift-check, drift self-heal, prune, state
ConfigMap, the `sync`/`status` CLI, OTLP metrics, cluster- and
namespaced-scope resources, CRDs, the controller polling loop, HTTPS basic-auth
and SSH-key Git access, error recovery, and **Helm hooks** — PreSync/PostSync
Job/Pod execution and completion, hook weights, `hook-delete-policy`,
pre/post-delete teardown, `resource-policy: keep`, and **foreground cascade**
deletion (the `foregroundDeletion` finalizer is observed across prune,
teardown, and Helm-hook deletions).

Every scenario is `#[ignore]`d (needs Docker + kind), so the suite stays out of
`nix flake check` (no Docker in the sandbox). Unlike `make bench`, it *is* run in
CI: the `e2e` job in [`.github/workflows/ci.yml`](.github/workflows/ci.yml)
enters the flake devShell and runs `make e2e` on every push and pull request. A
failing scenario exits non-zero so a regression fails the run. See
[`tests/e2e.rs`](tests/e2e.rs) and [`tests/common/`](tests/common/).

Concurrency and field-conflict behaviour — `controller` (long-lived) and `sync`
(manual, possibly in another Pod) may run at once, and server-side apply under a
single field manager keeps that safe and idempotent — are covered by unit tests
and are deliberately out of e2e scope: scenarios drive `sync` serially and run a
single controller at a time.

## Documentation

- [doc/user-manual.md](doc/user-manual.md) — complete feature reference
- [doc/tutorial.md](doc/tutorial.md) — hands-on kind cluster walkthrough
- [doc/architecture.md](doc/architecture.md) — how the implementation works

## License

Apache-2.0 — see [LICENSE](LICENSE).
