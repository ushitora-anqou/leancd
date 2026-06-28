# Lean CD

[![CI](https://github.com/ushitora-anqou/leancd/actions/workflows/ci.yml/badge.svg)](https://github.com/ushitora-anqou/leancd/actions/workflows/ci.yml)

**Lean CD** is a minimal, low-memory Continuous Delivery controller for
Kubernetes. It syncs manifests from a Git repository into the cluster it runs
in, detects drift, and self-heals — like Argo CD or Flux CD, but engineered to
keep its process memory footprint minimal.

Keeping memory consumption down is the single most important goal and is verified
by an automated benchmark (see [bench/](bench/)).

## Features

- Applies plain YAML manifests from Git (no Kustomize / Helm / Jsonnet).
- Detects Git changes by polling (`git fetch`, shallow clone).
- Detects cluster-side drift and re-applies the desired state.
- Prunes resources removed from Git, using **foreground cascade** deletion
  (`propagationPolicy: Foreground`) so dependents are removed before their
  owners — the same policy used for Helm-hook and full-teardown deletions.
- **Fail-fast on malformed manifests**: a YAML parse error in one file fails
  the whole sync rather than silently skipping the file — a skipped file would
  drop its resources from the applied set and get them pruned, so a typo in one
  file must never delete a previously-applied resource. The error lands in
  `state.last_error` (visible via `leancd status` / `leancd health`).
- Honors **Helm hooks** in pre-rendered manifests with Argo CD-equivalent
  semantics (`pre-install`/`pre-upgrade` → before the apply, `post-install`/
  `post-upgrade` → after; `pre-delete`/`post-delete` on full teardown), plus
  `helm.sh/hook-weight`, `helm.sh/hook-delete-policy`, and
  `helm.sh/resource-policy: keep`. Job/Pod hooks are awaited to completion.
- CLI for manual sync and status. Server-side apply always claims conflicting fields.
- Backs off exponentially on consecutive sync failures and shuts down gracefully
  (finishing the in-flight pass on SIGTERM); `SIGHUP` reloads `RUST_LOG`.
- `leancd health` subcommand for `exec` liveness/readiness probes.
- **Resource health assessment** (Argo CD-style): evaluates the health of
  managed resources each pass and surfaces the worst status in `leancd status`
  and `leancd health`, plus the `leancd_health_status` metric. A sync still
  completes on a successful apply — health is an independent signal, and like
  Argo CD it does not descend `ownerReferences` (a Deployment's health reads its
  own `.status`, which already aggregates its ReplicaSet/Pod state). Toggle with
  `--health-mode`.
- `leancd --version` and the startup log report the embedded git SHA.
- Metrics exported over OTLP/HTTP (push), including `leancd_rss_bytes` and
  `leancd_health_status`; a ready Grafana dashboard ships in the chart
  ([`charts/leancd/dashboards/`](charts/leancd/dashboards/)).
- **Per-resource apply-failure visibility**: a server-side apply that fails on
  one resource (unknown kind, admission denial, …) is recorded in
  `state.apply_failures` and the `leancd_apply_failures_total` metric, and
  listed by `leancd status` — without aborting the pass or tripping the probe,
  since the resource self-heals on the next pass's drift check.
- **Operations CLI**: `leancd diff` prints the desired-vs-live drift (read-only),
  `sync --dry-run` validates via a server-side dry-run (no mutation), and
  `leancd rollback [--to <sha>]` re-syncs to a past commit.
- **Structured audit log** (`leancd.audit` target) of every apply/prune/hook
  outcome, plus `--log-format json` for aggregation in Loki/ELK.
- Handles **all** resource kinds, including CRDs and cluster-scoped resources.

## Non-goals (kept out to stay small and light)

Kustomize / Helm-chart *rendering* / Jsonnet, owner-reference traversal,
notifications, and a web UI — all deliberately omitted to stay small: Argo CD and
Flux CD ship these but run at hundreds of MiB to GiB of RSS, and Lean CD trades
them for a far smaller footprint. (Helm *hooks* in already-rendered YAML are
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
leancd diff    [flags]            print the desired-vs-live drift (read-only)
leancd rollback [flags] [--to S]  re-sync to a past commit (temporary rollback)
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
| `--watch-mode` | `LEANCD_WATCH_MODE` | cache | how cluster-side drift wakes the loop: `off` (poll only), `trigger` (watch poke, List drift-check), or `cache` (watch + size-bounded cache; small objects cached, large ones drift-checked via a per-GVK List) |
| `--watch-debounce` | `LEANCD_WATCH_DEBOUNCE` | 500ms | collapses a burst of watch events into one reconcile pass |
| `--cache-max-object-bytes` | `LEANCD_CACHE_MAX_OBJECT_BYTES` | 12288 | in `cache` mode, the max serialized size (bytes) of an object cached in full; larger objects are tracked by key only and drift-checked via a per-GVK `List` (size-based, any kind) |
| `--hook-timeout-secs` | `LEANCD_HOOK_TIMEOUT_SECS` | 300 | per-hook (Job/Pod) completion timeout before it is treated as failed |
| `--backoff-base` | `LEANCD_BACKOFF_BASE` | 5s | base delay for exponential backoff on consecutive sync failures |
| `--backoff-max` | `LEANCD_BACKOFF_MAX` | 10m | maximum backoff delay (resets to poll-interval on success) |
| `--shutdown-timeout-secs` | `LEANCD_SHUTDOWN_TIMEOUT_SECS` | 28 | grace period for the in-flight pass on SIGTERM (≤ Pod terminationGracePeriodSeconds) |
| `--health-mode` | `LEANCD_HEALTH_MODE` | on | `on` evaluates and publishes resource health each pass; `off` skips it and its metric (sync completion is unaffected — health is an independent signal) |
| `--health-stale-factor` | `LEANCD_HEALTH_STALE_FACTOR` | 10 | `leancd health` reports stale when the last sync is older than poll-interval × this |
| `--lock-lease-duration-secs` | `LEANCD_LOCK_LEASE_DURATION_SECS` | 60 | reconcile-exclusion Lease lifetime (s); concurrent controller+sync passes are serialized via a Lease (one at a time) |
| `--lock-wait-timeout-secs` | `LEANCD_LOCK_WAIT_TIMEOUT_SECS` | 30 | seconds to wait for the reconcile Lease when another pass holds it before skipping with a "busy" INFO log (not an error) |
| `--namespace` | `LEANCD_NAMESPACE` | default | Lean CD's namespace |
| `--dry-run` | — (flag only) | false | `sync` only: server-side dry-run apply, no mutation/state |
| `--log-format` | `LEANCD_LOG_FORMAT` | text | `text` or `json` (structured, one object per line) log output |

For the complete flag and environment-variable reference, authentication modes,
metrics, tuning, and troubleshooting, see
[doc/user-manual.md](doc/user-manual.md).

## Deploy

Lean CD ships as a Helm chart, published to GHCR as an OCI artifact. Install it
directly — OCI needs no `helm repo add`:

```sh
# Namespace + Git credentials (omit the Secret for a public repo).
kubectl create namespace leancd
kubectl -n leancd create secret generic leancd-git-credentials \
  --from-literal=GIT_USERNAME=<user> --from-literal=GIT_PASSWORD=<token>
# (For SSH: --from-file=GIT_SSH_KEY=$HOME/.ssh/id_ed25519.)

helm install leancd oci://ghcr.io/ushitora-anqou/charts/leancd \
  --version X.Y.Z \
  --namespace leancd --create-namespace \
  --set config.repoUrl=https://github.com/example/manifests.git
kubectl -n leancd wait --for=condition=Available deploy/leancd --timeout=240s
```

The chart installs the Namespace, ServiceAccount, RBAC, and Deployment, and
points Lean CD at your OpenTelemetry Collector over OTLP/HTTP (Lean CD runs no
HTTP listener of its own). The default image resolves to `Chart.appVersion`, so
no `image.*` override is needed for the published build. Override `config.*`
values for your repository.

For a tighter production posture:
- a **PodDisruptionBudget** (`minAvailable: 1`, on by default) protects the
  single replica during node drains — disable only with >1 replica;
- a **NetworkPolicy** (default-deny ingress; egress limited to kube-dns, the API
  server, Git, and the OTLP collector) is generated in **both** RBAC modes —
  tighten `networkPolicy.kubeApiCidr` / `networkPolicy.egressCidr` to your CIDRs;
- `--set rbac.namespaced=true` additionally binds Lean CD's permissions to its
  namespace only (a RoleBinding instead of a cluster-wide ClusterRoleBinding);
- `--set priorityClass.enabled=true` opts into a high PriorityClass so the
  controller is among the last to be evicted under node pressure;
- set `image.pullSecrets` for a private / air-gapped registry.

The Grafana dashboard ships as a `grafana_dashboard`-labeled ConfigMap
(`--set dashboards.enabled=true`, on by default).

To install from a local checkout (e.g. development), point `helm` at the chart
directory: `helm install leancd charts/leancd ...`. See
[doc/release.md](doc/release.md) for the release process and
[charts/leancd/README.md](charts/leancd/README.md) for the full values reference.

For a hands-on walkthrough deploying Lean CD into a local `kind` cluster
(including an optional in-cluster Forgejo Git server), see
[doc/tutorial.md](doc/tutorial.md).

## How it keeps memory low

Lean CD holds no cluster-wide cache: every reconciliation issues direct
`List`/`Get`/`Patch` calls for exactly the resources declared in Git. In
`off`/`trigger` `--watch-mode` it holds no object cache at all — each pass
fetches only what it needs and discards it. The default `cache` mode holds only
a per-GVK
size-bounded `LightweightStore` (small objects cached in full, larger ones by
key only, measured to stay well under budget) so a steady-state drift-check of
small objects needs no per-pass `List` (larger objects fall back to one `List`
per GVK);
it is still not a cluster-wide cache or background store. Git history is kept
shallow (depth 1), YAML is parsed one document at a time, runtime state is a
single ConfigMap plus a managed-by label, and the runtime is single-threaded
(`tokio` `current_thread`), which keeps the footprint flat regardless of cluster
size.

## Benchmark

```sh
make bench        # or: ./bench/bench.sh   — single run
make scale        # or: ./bench/scale.sh   — RSS across 8/15/20 namespaces
make health-heavy # or: ./bench/health-heavy.sh — RSS under health-assessment load (Deployments fanning out to Pods, health ON)
```

`bench` samples RSS from startup through steady state and asserts both the sync
**peak** and the **idle** value stay under the configured budget (tune with `RSS_BUDGET_MIB`,
`BENCH_SAMPLE_SECS`). `scale` repeats the run at increasing namespace counts and
prints a peak/idle table. Both need a `kind` cluster and are **not** part of
`nix flake check` (no Docker in the sandbox); run them manually or in an external
CI job — the scripts exit non-zero on a budget breach, so a regression fails the
run. See [bench/README.md](bench/README.md).

## End-to-end tests

```sh
make e2e        # kind cluster + in-cluster Forgejo + Lean CD
```

The e2e suite spins up an ephemeral `kind` cluster and runs **Forgejo and
Lean CD as in-cluster Pods** (Lean CD is built into a container image via the
root [`Dockerfile`](Dockerfile) and loaded into the kind node). It drives
Lean CD's intended behavior end-to-end across ~40 scenarios: initial apply, Git
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

Concurrency and field-conflict behavior — `controller` (long-lived) and `sync`
(manual, possibly in another Pod) may run at once, and server-side apply under a
single field manager keeps that safe and idempotent — are covered by unit tests
and are deliberately out of e2e scope: scenarios drive `sync` serially and run a
single controller at a time.

## Documentation

- [doc/user-manual.md](doc/user-manual.md) — complete feature reference
- [doc/tutorial.md](doc/tutorial.md) — hands-on kind cluster walkthrough
- [doc/architecture.md](doc/architecture.md) — how the implementation works
- [doc/migration-from-argocd.md](doc/migration-from-argocd.md) — phased guide to
  migrating an Argo CD-managed cluster to Lean CD

## License

Apache-2.0 — see [LICENSE](LICENSE).
