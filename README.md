# leancd

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
- Prunes resources removed from Git.
- CLI for manual sync (`--force` to claim conflicting fields) and status.
- Prometheus metrics at `/metrics`, including `leancd_rss_bytes`.
- Handles **all** resource kinds, including CRDs and cluster-scoped resources.

## Non-goals (kept out to stay small and light)

Kustomize / Helm / Jsonnet, owner-reference traversal, notifications, and a web
UI. See [doc/design.md](doc/design.md) for the full design and rationale.

## Build

```sh
cargo build --release
```

The resulting binary is a single static-ish executable run as a `Deployment`.

## Usage

```
leancd controller [flags]      run as a long-lived controller (deploy this)
leancd sync    [--force] [flags]   run one reconciliation pass, then exit
leancd status  [flags]            print the last recorded sync state
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
| `--path` | `LEANCD_PATH` | . | manifest directory (recursive) |
| `--poll-interval` | `LEANCD_POLL_INTERVAL` | 60s | reconciliation interval |
| `--namespace` | `LEANCD_NAMESPACE` | default | leancd's namespace |
| `--metrics-addr` | `LEANCD_METRICS_ADDR` | 0.0.0.0:9090 | metrics endpoint |

## Deploy

```sh
kubectl apply -f deploy/leancd.yaml
```

The manifest installs the Namespace, ServiceAccount, RBAC, Deployment, and a
Service for metrics. Edit the `LEANCD_*` env values for your repository, and
create the `leancd-git-credentials` Secret for private repos.

## How it stays under 100MiB

leancd never builds an informer/cache of the cluster: every reconciliation
issues direct `List`/`Get`/`Patch` calls for exactly the resources declared in
Git. Git history is kept shallow (depth 1), YAML is parsed one document at a
time, runtime state is a single ConfigMap plus a managed-by label, and the
runtime is single-threaded (`tokio` `current_thread`). See
[doc/design.md §4](doc/design.md) for the full memory strategy.

## Benchmark

```sh
make bench        # or: ./bench/bench.sh   — single run
make scale        # or: ./bench/scale.sh   — RSS across 100/300/500 resources
```

`bench` samples RSS from startup through steady state and asserts both the sync
**peak** and the **idle** value stay under 100MiB (tune with `RSS_BUDGET_MIB`,
`BENCH_SAMPLE_SECS`). `scale` repeats the run at increasing manifest counts and
prints a peak/idle table. Both need a `kind` cluster and are **not** part of
`nix flake check` (no Docker in the sandbox); run them manually or in an external
CI job — the scripts exit non-zero on a budget breach, so a regression fails the
run. See [bench/README.md](bench/README.md).

## License

MIT.
