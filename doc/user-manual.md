# Lean CD User Manual

This is the complete reference for Lean CD: every subcommand, flag, environment
variable, Git-authentication mode, metric, and operational concern.

> This manual is the detailed companion to [`../README.md`](../README.md). It
> does not repeat the README's overview; it deepens it. For a 20-minute hands-on
> walkthrough see [`./tutorial.md`](./tutorial.md); for how the implementation
> works (and why it is shaped that way) see [`./architecture.md`](./architecture.md).

## 1. Introduction

Lean CD is a minimal, low-memory continuous-delivery controller for Kubernetes.
It syncs plain YAML manifests from a Git repository into the cluster it runs
in, detects drift, and self-heals — with a hard process-RSS budget it keeps
minimal.

One running process syncs exactly one Git repository (one branch, one set of
paths). To manage multiple repositories, run multiple Lean CD processes
(Deployments).

Reading order if you are new:

1. [`../README.md`](../README.md) — overview and quick start.
2. [`./tutorial.md`](./tutorial.md) — deploy Lean CD into a `kind` cluster.
3. This manual — full reference.
4. [`./architecture.md`](./architecture.md) — internals and the reasoning behind each mechanism.

## 2. Core concepts

### Git-to-cluster sync

Lean CD keeps a depth-1 shallow checkout of `<repo-url>` at `<branch>` under
`<work-dir>`, expands each `--path` glob pattern into the directories it
matches, parses every `*.yaml`/`*.yml` under them (recursively), and
server-side-applies each manifest into the cluster. One process = one repo +
one set of paths.

### The managed-by label

Every applied manifest gets a label injected (default
`app.kubernetes.io/managed-by=leancd`, configurable via `--managed-label-key`/
`--managed-label-value`). This label is used by drift detection and pruning to
find the resources Lean CD owns, so it must be stable for the lifetime of a
deployment. The state ConfigMap deliberately carries **no** managed-by label —
the prune safety-net lists live resources by that label, so an unlabeled state
ConfigMap is invisible to pruning and Lean CD never deletes its own state.

### The state ConfigMap

Lean CD persists its progress in a single ConfigMap named `<state-configmap>`
(default `leancd-state`) in `<namespace>`. It records the last applied commit
SHA, sync/drift/managed counts, and the set of resource keys applied last. This
is the source of truth for "what has Lean CD done"; Git is the source of truth
for "what should exist". If the ConfigMap is lost, Lean CD treats the next pass
as a first run and re-applies everything (safe, just less efficient).

### Reconciliation pass

Each pass: fetch Git → parse manifests → either **full-apply** or
**drift-check** → prune → write state.

Lean CD fully re-applies every manifest on the first run or when the Git HEAD
moved. In **steady state** (prior state present, HEAD unchanged) it instead
does a cheap drift check and only re-applies if drift is found. This keeps
steady-state API traffic minimal.

### Server-side apply (SSA)

All applies use Kubernetes server-side apply under a field manager (default
`leancd`, configurable via `--field-manager`). SSA is idempotent and lets
Lean CD coexist with other managers; applies always run with `.force()`, so SSA
claims ownership of conflicting fields.

### Helm hooks

Lean CD honors Helm hook annotations in **pre-rendered** manifests (i.e. YAML
already produced by `helm template` or equivalent — Lean CD does not render
charts). The semantics match Argo CD. Note that Lean CD does **not** read
`argocd.argoproj.io/hook` or `argocd.argoproj.io/sync-wave`; when migrating from
Argo CD, convert those to the `helm.sh/hook` equivalents (see
[`./migration-from-argocd.md`](./migration-from-argocd.md) §8). The mapping is:

- `helm.sh/hook: pre-install` / `pre-upgrade` run **before** the main apply;
  `post-install` / `post-upgrade` run **after** it (install and upgrade are
  indistinguishable in a single reconcile, so they collapse).
- `helm.sh/hook: pre-delete` / `post-delete` run on a **full teardown** — when
  every main resource has left Git while Lean CD still has an applied set. The
  order is pre-delete → prune all → post-delete.
- `helm.sh/hook-weight` orders hooks within a phase (ascending; ties by name;
  default `0`).
- `helm.sh/hook-delete-policy` controls hook deletion (`before-hook-creation`
  [default], `hook-succeeded`, `hook-failed`); multiple comma-separated values
  are honored.
- `helm.sh/resource-policy: keep` exempts a resource from pruning entirely.

Job (`batch/Job`) and Pod hooks are **awaited to completion** within
`--hook-timeout-secs` (default 300s): a hook reaching `Complete`/`Succeeded` is
treated as success, `Failed` as failure, and a timeout as failure. Other kinds
are considered complete — and successful — on apply: Lean CD never observes
failure for them, so `hook-failed` never fires, while `hook-succeeded` always
does (and `before-hook-creation`, the default, applies to every kind). A failed
PreSync (or pre-delete) hook aborts
the pass before the main apply; a failed PostSync hook leaves the already-applied
main resources in place and records the error in the state ConfigMap.

A resource whose `helm.sh/hook` lists **only unsupported types** (e.g. `test`,
`rollback`, `crd-install`) is ignored — it is neither applied nor pruned.

Removing a hook from Git does **not** delete its already-applied instance. Hooks
are deliberately excluded from the prune set — a hook's lifetime is governed by
`helm.sh/hook-delete-policy`, not Git membership — so a retired hook stays in the
cluster unless its delete policy already removed it on the final run that still
declared it (e.g. `hook-succeeded` after a successful run), or until it is
deleted manually. This mirrors Argo CD.

## 3. Installation

There is no published binary or image yet; build from source.

### Build the binary

```sh
cargo build --release
# binary: target/release/leancd
```

### Build the container image

The project ships a multi-stage [`../Dockerfile`](../Dockerfile):

```sh
docker build -t leancd:latest .
```

The runtime image is `debian:bookworm-slim` with `git`, `ca-certificates`, and
`openssh-client` installed (`git` because Lean CD shells out to it; the latter
two for HTTPS and SSH transports). The entrypoint is `leancd`.

### Runtime requirements

- A Linux host with `/proc` available (the RSS metric reads it).
- `git` on `PATH` (for the container image, the Dockerfile installs it; for a
  bare binary, install `git` yourself).
- Network reachability to the Kubernetes API and the Git repository.

## 4. Subcommands

Lean CD has four subcommands. All flags in [§5](#5-configuration-reference) are
accepted by every subcommand (they share `CommonArgs`); flags that only matter
for `controller` are noted there.

### `leancd controller`

Runs as a long-lived controller: initializes the OTel meter provider, then
reconciles on `<poll-interval>` forever. **Deploy this** as a `Deployment`.

```sh
leancd controller --repo-url https://github.com/org/manifests.git
```

On `SIGINT`/`SIGTERM` it stops **cooperatively**: the in-flight reconciliation
pass finishes, then the loop exits and the OTel meter provider is flushed (one
final export). If a pass does not finish within `--shutdown-timeout-secs`, the
task is force-aborted as a fallback so Pod termination is not blocked. A
failing pass is retried after an exponential backoff (`--backoff-base`/
`--backoff-max`), reset to `--poll-interval` on success; the backoff delay is
jittered to `[0.75x, 1.0x)` so repeated failures across instances do not
synchronize. `SIGHUP` reloads the log filter from `RUST_LOG` without restarting.

### `leancd sync`

Runs exactly one reconciliation pass, then exits. Exit code is non-zero if the
pass failed. Use this from CI, cron, or a one-off `kubectl exec`.

```sh
leancd sync                          # one pass
```

Applies always run with force-conflict server-side apply, so Lean CD takes
ownership of fields currently owned by another field manager — field conflicts
never block a sync.

### `leancd status`

Read-only: prints the contents of the state ConfigMap (last SHA, sync count,
managed/drift counts, last sync time, last error). No reconciliation or metrics
instrumentation.

```sh
leancd status
```

Output:

```
leancd status (default/leancd-state)
  last sha:   a1b2c3d...
  sync count: 42
  managed:    17
  drift:      0
  last sync:  unix 1700000000
```

If no state is recorded yet it prints `no sync state recorded yet`.

### `leancd health`

Read-only health check for liveness/readiness `exec` probes: reads the state
ConfigMap and classifies the last sync. Exits with:

| Code | Meaning |
|---|---|
| `0` | fresh — last sync recent (within `poll_interval × --health-stale-factor`) and no error |
| `1` | never — no state recorded yet (e.g. before the first sync) |
| `2` | stale — last sync older than the staleness threshold |
| `3` | failing — the last sync recorded an error (takes priority over staleness) |

```sh
leancd health                         # exit code reflects sync health
```

It exposes no HTTP listener — wire it to a Deployment `livenessProbe`/
`readinessProbe` as `exec: [leancd, health]` (see the chart's
[`templates/deployment.yaml`](../charts/leancd/templates/deployment.yaml)).

## 5. Configuration reference

### 5.1 Flags and environment variables

Precedence is **flag > env > default**. A flag always wins over its env var.

| Flag | Env | Default | Applies to | Description |
|---|---|---|---|---|
| `--repo-url` | `LEANCD_REPO_URL` | — (required) | all | Git repository URL |
| `--branch` | `LEANCD_BRANCH` | `main` | all | branch / ref to track |
| `--path` | `LEANCD_PATH` | `.` | all | glob patterns of manifest directories, scanned recursively; repeatable, comma-separated via env (e.g. `live/*/prod`) |
| `--poll-interval` | `LEANCD_POLL_INTERVAL` | `60s` | controller | reconciliation interval (see [§5.2](#52-duration-parser)) |
| `--namespace` | `LEANCD_NAMESPACE` | `default` | all | Lean CD's namespace (state ConfigMap; default ns for ns-less resources) |
| `--state-configmap` | `LEANCD_STATE_CONFIGMAP` | `leancd-state` | all | state ConfigMap name |
| `--work-dir` | `LEANCD_WORK_DIR` | `/tmp/leancd-work` | all | local checkout directory |
| `--git-username-env` | `LEANCD_GIT_USERNAME_ENV` | `GIT_USERNAME` | all | name of the env var holding the HTTPS username (see [§5.3](#53-git-credential-indirection)) |
| `--git-password-env` | `LEANCD_GIT_PASSWORD_ENV` | `GIT_PASSWORD` | all | name of the env var holding the HTTPS password/token |
| `--git-ssh-key-env` | `LEANCD_GIT_SSH_KEY_ENV` | `GIT_SSH_KEY` | all | name of the env var holding the SSH private key (PEM) |
| `--managed-label-key` | — | `app.kubernetes.io/managed-by` | all | managed-by label key |
| `--managed-label-value` | — | `leancd` | all | managed-by label value |
| `--field-manager` | — | `leancd` | all | SSA field manager name |
| `--hook-timeout-secs` | `LEANCD_HOOK_TIMEOUT_SECS` | `300` | all | per-hook (Job/Pod) completion timeout before it is treated as failed (see [Helm hooks](#helm-hooks)) |
| `--backoff-base` | `LEANCD_BACKOFF_BASE` | `5s` | controller | base delay for exponential backoff on consecutive sync failures (see [§5.2](#52-duration-parser)) |
| `--backoff-max` | `LEANCD_BACKOFF_MAX` | `10m` | controller | maximum backoff delay (cap); resets to `--poll-interval` on success |
| `--shutdown-timeout-secs` | `LEANCD_SHUTDOWN_TIMEOUT_SECS` | `28` | controller | grace period for the in-flight pass to finish before force-abort on shutdown (keep ≤ Pod `terminationGracePeriodSeconds`) |
| `--health-stale-factor` | `LEANCD_HEALTH_STALE_FACTOR` | `10` | all | `leancd health` reports stale when the last sync is older than `poll_interval × this` |
| `--lock-lease-duration-secs` | `LEANCD_LOCK_LEASE_DURATION_SECS` | `60` | all | reconcile-exclusion Lease lifetime (s); a crashed holder's lease is reclaimed after this. Concurrent `controller`/`sync` passes are serialized via a Lease so only one runs at a time. |
| `--lock-wait-timeout-secs` | `LEANCD_LOCK_WAIT_TIMEOUT_SECS` | `30` | all | seconds to wait for the reconcile Lease when another pass holds it before skipping with a "busy" INFO log (not an error) |
| `--watch-mode` | `LEANCD_WATCH_MODE` | `cache` | controller | how cluster-side drift wakes the loop: `off` (periodic poll only), `trigger` (a `watcher` per managed GVK pokes the loop on any change; drift checked via `List`), or `cache` (a `watcher` + reflector `Store` per GVK; drift read from the Store, so no per-pass `List`). Default `cache`: measured (`bench/`) to match `trigger` on RSS while removing per-pass `List` apiserver load. |
| `--watch-debounce` | `LEANCD_WATCH_DEBOUNCE` | `500ms` | controller | collapses a burst of watch events (a reconnect `InitApply` burst, or a rapid edit storm) into one reconcile pass |

`--poll-interval` and `--git-*-env` are accepted by all subcommands (they are
part of `CommonArgs`) but only `controller` uses `--poll-interval` in a
meaningful way — `sync`/`status` run one pass and do not poll.

### 5.2 Duration parser

`--poll-interval` (and `LEANCD_POLL_INTERVAL`) use Lean CD's own parser, which
accepts an integer followed by one of these unit suffixes:

| Suffix | Meaning |
|---|---|
| `ms` | milliseconds |
| `s` | seconds |
| `m` | minutes |
| `h` | hours |

Examples: `30s`, `5m`, `2h`, `500ms`. A bare number (no suffix) is an error.

### 5.3 Git credential indirection

This is the most commonly misunderstood part of the configuration. **There is
no `LEANCD_GIT_USERNAME`, `LEANCD_GIT_PASSWORD`, or `LEANCD_GIT_SSH_KEY`
variable.** Instead:

- `--git-username-env` names the environment variable that Lean CD reads the
  HTTPS username **from** (default: it reads `GIT_USERNAME`).
- `--git-password-env` names the variable read for the HTTPS password/token
  (default: `GIT_PASSWORD`).
- `--git-ssh-key-env` names the variable read for the SSH private key (default:
  `GIT_SSH_KEY`).

So the default flow is: put `GIT_USERNAME`/`GIT_PASSWORD` (or `GIT_SSH_KEY`)
into the environment — typically via a Kubernetes Secret mounted with
`envFrom` — and Lean CD picks them up. If your Secret uses different key names,
point these flags at them. The variables that hold the `--git-*-env` defaults
themselves (`LEANCD_GIT_USERNAME_ENV`, etc.) just let you rename the credential
variable; they do not hold credentials.

Example with non-default Secret keys:

```sh
leancd controller \
  --repo-url https://github.com/org/manifests.git \
  --git-username-env GH_USER \
  --git-password-env GH_TOKEN
```

This tells Lean CD to read the HTTPS user from `GH_USER` and the token from
`GH_TOKEN`.

## 6. Git authentication

### 6.1 URL transport detection

Lean CD infers the transport from the repository URL:

| URL prefix | Transport |
|---|---|
| `git@...` or `ssh://` | SSH |
| `https://` or `http://` | HTTPS |
| `file://`, `/abs`, or `./rel` | File |
| anything else | Other (passed through) |

### 6.2 HTTPS basic auth

For HTTPS URLs, if both the username and password variables are present and
non-empty, Lean CD percent-encodes them and embeds them in the URL
(`https://user:pass@host/...`) before invoking `git`. The authed URL is never
logged. If only one is set, or both are empty, the URL is passed through
unchanged (use this for public HTTPS repos).

In Kubernetes, put the credentials in a Secret and mount it with `envFrom`:

```sh
kubectl -n leancd create secret generic leancd-git-credentials \
  --from-literal=GIT_USERNAME=<user> \
  --from-literal=GIT_PASSWORD=<token>
```

### 6.3 SSH key

For SSH URLs (`git@...` or `ssh://`), Lean CD reads a PEM private key from the
configured variable (default `GIT_SSH_KEY`). It materialises the key to a
per-process file (`<work-dir parent>/.leancd_ssh_key_<pid>`, mode `0600`, with a
trailing newline so OpenSSH parses the PEM) and points `git` at it via
`GIT_SSH_COMMAND`:

```
ssh -i <key> -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=<pid file>
```

The known-hosts store is a separate per-process file
(`.leancd_known_hosts_<pid>`), so Lean CD never touches your `~/.ssh/known_hosts`.
New host keys are accepted on first contact (`accept-new`). Both files are
deleted when the sync finishes.

```sh
kubectl -n leancd create secret generic leancd-git-credentials \
  --from-file=GIT_SSH_KEY=$HOME/.ssh/id_ed25519
```

### 6.4 Public repositories

For a public repo, omit the Secret entirely. The shipped Deployment mounts
`leancd-git-credentials` with `optional: true`, so a missing Secret is fine.

### 6.5 Local `file://` repositories

A `file://`, absolute, or relative path is passed straight to `git` with no
authentication. This is useful for testing and air-gapped clusters — but the
path must be reachable from wherever Lean CD runs. A path on your host is **not**
visible inside a Pod, so `file://` is mainly for running Lean CD as a host
process (as the RSS benchmark does); for in-cluster use, prefer an HTTPS/SSH
repo.

## 7. Metrics reference

Lean CD exposes no HTTP endpoint. It instruments metrics with the OpenTelemetry
SDK and pushes them over **OTLP/HTTP** (protobuf, port 4318) to a collector at
fixed intervals (`PeriodicReader`, default 60s). Configuration is via the
standard `OTEL_EXPORTER_OTLP_*` environment variables — there is no metrics
flag. Only the `controller`/`sync` subcommands export (the provider is flushed
on exit). Point an OpenTelemetry Collector's OTLP/HTTP receiver at
`OTEL_EXPORTER_OTLP_ENDPOINT` (and scrape *its* Prometheus exporter if you want
text-format output).

| Metric | Type | Labels | Description |
|---|---|---|---|
| `leancd_sync_total` | counter | — | Number of reconciliation passes |
| `leancd_sync_errors_total` | counter | — | Number of failed reconciliations |
| `leancd_hooks_total` | counter | `phase`, `result` | Helm hooks executed (`phase` ∈ presync/postsync/predelete/postdelete; `result` ∈ succeeded/failed) |
| `leancd_sync_last_success_timestamp_seconds` | observable gauge | — | Unix timestamp of the last successful sync |
| `leancd_drift_detected` | observable gauge | `group`, `version`, `kind` | Drifted resources, broken down by GVK (reset each pass) |
| `leancd_managed_resources` | observable gauge | — | Number of resources managed by Lean CD |
| `leancd_rss_bytes` | observable gauge | — | Process resident set size in bytes (read at each collection) |

`leancd_rss_bytes` is the headline metric: it must stay under the RSS budget
at both the sync peak and idle. It is read fresh at each collection from
`/proc/<pid>/statm` via an observable-gauge callback, so it reflects the live
footprint.

`leancd_drift_detected` is reset to zero at the start of each pass and then set
per GVK from that pass's results, so a drift that was fixed clears on the next
reconcile rather than sticking.

Sample PromQL:

```promql
# Steady-state RSS over the last 5 minutes
avg_over_time(leancd_rss_bytes[5m])

# Sync error ratio
rate(leancd_sync_errors_total[5m]) / rate(leancd_sync_total[5m])

# Currently-drifting resources by kind
sum by (kind) (leancd_drift_detected)
```

### 7.1 Getting metrics into Prometheus

Lean CD only pushes OTLP/HTTP and exposes no scrape endpoint. There are two ways
to land the metrics in Prometheus — pick one, no Lean CD change is needed.

**A. OTel Collector → Prometheus exporter (works with any Prometheus).** Run a
collector whose OTLP/HTTP receiver accepts Lean CD's push and whose Prometheus
exporter exposes `/metrics` for Prometheus to scrape:

```yaml
# otel-collector-config.yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
exporters:
  prometheus:
    endpoint: 0.0.0.0:8889
    resource_to_telemetry_conversion:
      enabled: true
service:
  pipelines:
    metrics:
      receivers: [otlp]
      exporters: [prometheus]
```

Then scrape the collector's exporter (`otel-collector:8889`) with a `ServiceMonitor`
or a static `scrape_config`. This is the path the e2e suite uses
(`tests/leancd.yaml`).

**B. Prometheus ≥ 3.0 native OTLP receiver (no collector).** Prometheus 3.x can
ingest OTLP directly. Enable it with a startup flag and an `otlp:` block, then
point Lean CD at the Prometheus OTLP endpoint:

```yaml
# prometheus.yml (Prometheus ≥ 3.0) — root-level block, not under scrape_configs
otlp:
  promote_resource_attributes:
    - service.name
```

```sh
prometheus --web.enable-otlp-receiver --config.file=prometheus.yml
```

The receiver listens on `/api/v1/otlp/v1/metrics`; set Lean CD's endpoint to the
`/api/v1/otlp` prefix so the SDK appends the standard `/v1/metrics` path:

```sh
LEANCD_...  # controller flags
OTEL_EXPORTER_OTLP_ENDPOINT=http://prometheus:9090/api/v1/otlp
OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
```

See the [Prometheus OpenTelemetry guide](https://prometheus.io/docs/guides/opentelemetry/)
for the full `otlp:` block and resource-attribute promotion.

### 7.2 Grafana dashboard

A ready-made Grafana dashboard for Lean CD ships in the chart at
[`charts/leancd/dashboards/leancd-overview.json`](../charts/leancd/dashboards/leancd-overview.json).
With `dashboards.enabled=true` (the default) the chart renders it as a ConfigMap
labeled `grafana_dashboard: "1"`, which a Grafana running the kiwigrid dashboard
sidecar (e.g. the VictoriaMetrics or `grafana` helm charts with sidecar dashboards
enabled) imports automatically. To import it manually instead, use
**Dashboards → New → Import → Upload JSON file**, pick your Prometheus data source
for the `DS_PROMETHEUS` variable, and the panels below appear. The data source is
a variable, so the same dashboard binds to whatever Prometheus you point it at.

It covers all of Lean CD's metrics in one view:

| Panel | Metric(s) | What it shows |
|---|---|---|
| RSS (memory budget) | `leancd_rss_bytes` | Current RSS, with threshold lines at the warning and budget levels |
| RSS over time | `leancd_rss_bytes` | RSS trend with a budget threshold line |
| Sync error ratio (5m) | `leancd_sync_errors_total`, `leancd_sync_total` | `rate(errors[5m]) / rate(total[5m])` |
| Time since last successful sync | `leancd_sync_last_success_timestamp_seconds` | `time() - last_success` — the same freshness signal `leancd health` uses |
| Managed resources | `leancd_managed_resources` | Current managed-resource count |
| Sync & error rate (5m) | `leancd_sync_total`, `leancd_sync_errors_total` | Per-second sync and error rate |
| Drifted resources by kind | `leancd_drift_detected` | `sum by (kind) (...)` |
| Helm hooks run in last hour | `leancd_hooks_total` | `sum by (phase, result) (increase(...[1h]))` |

**Metric-name note.** The dashboard queries use the metric names Lean CD emits
(`leancd_sync_total`, `leancd_rss_bytes`, …) directly. With the standard OTel
Collector Prometheus exporter path (§7.1 A) those names appear unchanged. If
your collector uses a non-default `translation_strategy` the suffixes may differ
(e.g. unit suffixes or `_total` handling) — adjust the queries, or set
`translation_strategy: UnderscoreEscapingWithSuffixes`. When
`resource_to_telemetry_conversion` is enabled (as in the §7.1 example) the
`service.name=leancd` resource attribute becomes a `service_name` label, so in a
multi-service Prometheus add `{service_name="leancd"}` to each query to isolate
Lean CD's series.

## 8. Tuning

### 8.1 Poll interval

`--poll-interval` trades responsiveness for API traffic. Lower values detect
Git changes faster but issue more reconcile passes. Each steady-state pass
lists live managed resources — one `List` per GVK in `off`/`trigger`
`--watch-mode`, or a `Store` read (no per-pass `List`) in `cache` (the
default). Note `cache`/`trigger` modes also wake the loop on a cluster-side
change within `--watch-debounce`, so `--poll-interval` bounds Git-side
detection latency, not drift self-heal. The default `60s` is what the RSS
benchmark validates; if you change it significantly at scale, re-run `make bench`.

### 8.2 Namespace and multi-tenancy

`--namespace` sets Lean CD's **own** namespace — where the state ConfigMap lives
and the default namespace for manifests that omit one. Lean CD applies manifests
into whatever namespaces those manifests declare, so it can manage resources
across namespaces (and cluster-scoped resources) from one deployment.

One Lean CD process manages one repository. To manage several repositories, run
several Deployments — each with its own `--repo-url`, `--namespace`,
`--state-configmap`, and (if they share a cluster) distinct
`--managed-label-value`/`--field-manager` so their resources don't collide.

### 8.3 Managed-by label customization

Change `--managed-label-key`/`--managed-label-value` when running multiple
Lean CD instances in one cluster that must not touch each other's resources.
Drift detection and pruning both filter by this label, so two instances with
different label values are isolated. Renaming the label on an existing
deployment orphans its previously-applied resources (they keep the old label).

### 8.4 Field manager

`--field-manager` (default `leancd`) is the SSA field-manager identity. SSA
tracks field ownership by manager name; renaming it means Lean CD loses
ownership of fields it applied under the old name (a subsequent sync
re-claims them). Keep it stable across the lifetime of a deployment.

### 8.5 Resource limits

The shipped Deployment requests 32Mi/50m and limits 128Mi/200m. The RSS
budget is for the **process**; the 128Mi limit leaves headroom. If you tighten
the limit too close to the process budget you risk OOM-killing Lean CD during a
sync peak; if you raise it you lose the safety net the budget provides. See [`../bench/`](../bench/)
to verify on your hardware and manifest scale.

## 9. Operations

### 9.1 Logs

Lean CD uses `tracing`, controlled by `RUST_LOG` (an `EnvFilter`); the default
level is `info`. Set `RUST_LOG=debug` for verbose output, or target a module:

```sh
RUST_LOG=leancd=debug,kube=info
```

Structured fields include `error = %e`, `sha`, `full`, `managed`,
`pruned`, `drift`, and the resource `key`. Each pass logs
`reconciliation complete` on success.

### 9.2 Reading sync state

```sh
# In a Pod (or via kubectl exec):
leancd status

# Raw ConfigMap:
kubectl -n <namespace> get configmap <state-configmap> -o yaml
```

### 9.3 Triggering a manual sync

```sh
kubectl -n leancd exec deploy/leancd -- leancd sync          # one pass
```

This runs one pass immediately without waiting for the next poll cycle. It uses
the same engine as the controller, so the result is identical.

### 9.4 RBAC

The shipped `ClusterRole` is intentionally broad — `apiGroups: ["*"]`,
`resources: ["*"]`, plus non-resource URLs — because Lean CD applies arbitrary
manifests including CRDs and cluster-scoped resources. **Narrow it in
production** to the `apiGroups`/`resources`/namespaces Lean CD should manage.
Because drift detection and pruning list resources filtered by the managed-by
label, Lean CD needs `list` (and `get`) on the kinds it manages, plus
`create`/`update`/`patch` (apply) and `delete` (prune).

### 9.5 Upgrading

Rebuild the image, load it into your cluster, and restart the Deployment
(`strategy: Recreate`):

```sh
docker build -t leancd:latest .
# load into your cluster's image store (e.g. kind)
kind load docker-image leancd:latest --name <cluster>
kubectl -n leancd rollout restart deploy/leancd
```

### 9.6 Backups / disaster recovery

State lives in a single ConfigMap. Losing it is **safe**: Lean CD treats the
next pass as a first run and re-applies everything (a full apply), then prunes
nothing (the safety-net prune only covers GVKs seen in prior state, so a
re-created deployment will not delete pre-existing resources until it has
applied and then seen them removed from Git). Git is always the source of
truth; back up your repository, not the state ConfigMap.

## 10. Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Pod `CrashLoopBackOff` | `LEANCD_REPO_URL` unset, or a hard config/git error | `kubectl logs`; check env and that the URL is reachable |
| `git ... failed: ... could not read Username` | HTTPS credentials missing or empty | Create the Secret with `GIT_USERNAME`/`GIT_PASSWORD`; `GIT_TERMINAL_PROMPT=0` prevents a hang |
| `git ... failed: ... Permission denied (publickey)` | SSH key missing or wrong | Ensure `GIT_SSH_KEY` holds a valid PEM key and the URL is `git@`/`ssh://` |
| Drift never detected on an old resource | The resource predates Lean CD and lacks the managed-by label | Re-apply via `sync`, or label it manually |
| Prune deletes nothing after state loss | Expected: the safety-net prune only covers previously-applied GVKs | Re-apply, then remove from Git; the next pass prunes |
| First reconcile re-applies everything | Expected: no prior state | None — this is correct first-run behavior |
| `leancd_rss_bytes` absent or 0 | `/proc` unavailable (non-Linux host), or metrics not reaching the collector | Run on Linux; check `OTEL_EXPORTER_OTLP_ENDPOINT` and that the collector is up |
| `status` says `no sync state recorded yet` | First run hasn't completed, or state ConfigMap was deleted | Wait one pass; or run `leancd sync` |

## 11. Security considerations

- **RBAC**: the default `ClusterRole` is broad. Narrow it to the resources and
  namespaces Lean CD should touch (see [§9.4](#94-rbac)).
- **Credentials**: only Git credentials are read from the environment (typically
  a Secret via `envFrom`). They are never accepted as flags and the authed HTTPS
  URL is never logged.
- **SSH key on disk**: Lean CD writes the SSH key to a per-process `0600` file
  for the duration of a sync and deletes it afterwards. It never lives in
  process memory long-term. The host must protect `/tmp` (or wherever
  `--work-dir`'s parent is).
- **TLS**: Lean CD uses `rustls` (no OpenSSL). It talks to the Git server over
  HTTPS using the system CA bundle (`ca-certificates` in the container image).

## 12. Non-goals

These are deliberately out of scope (see [`../README.md`](../README.md)):

- No Kustomize, Helm, or Jsonnet — plain YAML only.
- No owner-reference traversal for pruning.
- No notifications (Slack, email, etc.).
- No web UI and no webhook receiver.
- One repository per process (no multi-app management).

## 13. Glossary

- **RSS** — resident set size; the process memory Lean CD keeps minimal.
- **SSA** — server-side apply; Kubernetes merges manifests server-side under a
  field manager.
- **Drift** — the live cluster state diverges from the Git-declared state.
- **Prune** — delete resources that Lean CD applied but that Git no longer
  declares.
- **GVK** — group/version/kind; the resource type of a manifest.
- **managed-by label** — `app.kubernetes.io/managed-by=leancd` by default;
  marks resources Lean CD owns.
- **state ConfigMap** — `leancd-state` by default; Lean CD's persisted progress.
- **field manager** — SSA identity (`leancd` by default) that owns applied
  fields.

## 14. Further reading

- [`../README.md`](../README.md) — project overview and quick start.
- [`./tutorial.md`](./tutorial.md) — hands-on deployment into a `kind` cluster.
- [`./architecture.md`](./architecture.md) — how the implementation works and
  why it is shaped that way.
- [`./migration-from-argocd.md`](./migration-from-argocd.md) — migrating an
  Argo CD-managed cluster to Lean CD.
- [`../bench/README.md`](../bench/README.md) — RSS benchmark.
