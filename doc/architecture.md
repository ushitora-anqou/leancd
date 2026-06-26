# Lean CD Architecture

This document describes **how the current implementation works** and, where it
matters, *why* it works that way.

**Correctness first.** The single non-negotiable invariant is that `sync` never
leaves the cluster in an incorrect state — in particular, concurrent
`controller` and `sync` passes (in the same Pod via `kubectl exec`, or in a
separate Pod) must not race on the git checkout or clobber sync state. This
takes absolute priority over the memory budget: a mechanism required for
correctness is adopted even at an RSS cost. In practice each reconcile pass is
serialized by a Kubernetes Lease (`lock.rs`) so only one pass runs at a time;
the cost is constant-order and adds no crate dependencies, so the budget is
not actually breached — but if the two ever conflict, correctness wins.

The overriding goal — *subject to the correctness invariant above* — is keeping
process RSS minimal; every mechanism below exists to preserve that budget, and
the trade-offs it forces — no cluster-wide cache and no background
state — are noted inline where relevant.

For a quick overview see [`../README.md`](../README.md); for the complete
feature reference (every flag, env var, metric) see
[`./user-manual.md`](./user-manual.md); for a hands-on walkthrough see
[`./tutorial.md`](./tutorial.md).

## 1. Overview

Lean CD is a single static binary with four subcommands. The `controller` and
`sync` subcommands share one reconciliation engine; `status` and `health` are
read-only. One running process syncs exactly one Git repository (one branch,
one path) into the cluster it runs in.

```
        ┌─────────────────────────────────────────────┐
        │                  leancd                     │
        │                                             │
        │ controller / sync / status / health ► Reconciler │
        │                                      │      │
        │   ┌──────────┬──────────┬──────────┬───────┼───┐
        │   ▼          ▼          ▼          ▼       ▼   │
        │ git_sync  manifest  kube_util   drift/prune  state │
        │   │          │          │          │       │   │
        │   ▼          ▼          ▼          ▼       ▼   │
        │  git CLI serde_saphyr kube API  kube API  CM  │
        └─────────────────────────────────────────────┘
                         │                  ▲
                         ▼                  │
                   Git repository      Kubernetes API
```

The overriding invariant is that process RSS stays **strictly small** at all times
(idle and at sync peak). That budget is the project's headline goal — favored
over feature breadth, real-time responsiveness, and implementation convenience
— so every mechanism below exists to preserve it: no cluster-wide cache, no
background state, shallow clones, streaming parses, and a single-threaded runtime.

## 2. Module map

`src/` contains sixteen library modules plus the `main` entry point. `reconcile` is the hub; `kube_util` is the only
boundary that touches the Kubernetes API; `main` wires the runtime.

| Module | Responsibility |
|---|---|
| `main.rs` | Entry point: `tokio` `current_thread` runtime, tracing, subcommand dispatch, graceful shutdown |
| `cli.rs` | `clap` subcommands (`controller`, `sync`, `status`, `health`) and shared `CommonArgs` → `Config` |
| `health.rs` | `health` subcommand: classifies the last sync state (fresh/never/stale/failing) for `exec` liveness/readiness probes |
| `config.rs` | Validated `Config`; Git-transport classification; credential resolution; duration parser |
| `git_sync.rs` | Shallow `fetch`/`clone` and HEAD-SHA change detection via the `git` CLI |
| `manifest.rs` | Streaming multi-document YAML parse; GVK/ns/name extraction; `kind: List` expansion; managed-label injection; annotation read helper |
| `kube_util.rs` | API discovery (`pinned_kind`), dynamic `Api` construction (cluster vs namespaced), SSA `apply`, `list`, `get`, `delete` |
| `hooks.rs` | Helm-hook classification + execution (Argo CD-equivalent phases, weights, delete-policy, Job/Pod completion wait) |
| `reconcile.rs` | The `Reconciler` engine shared by `controller` and `sync` |
| `lock.rs` | Reconcile-pass mutual exclusion via a `coordination.k8s.io/v1` Lease (one pass at a time, cluster-wide); stale-lease reclaim |
| `watch.rs` | Optional watch trigger (`--watch-mode`): per-GVK `watcher` drivers (consumed directly, no reflector) that wake `run_loop` on a cluster-side change, collapsing drift-detection latency below `poll_interval`; in `cache` mode each driver maintains a size-bounded `LightweightStore` |
| `drift.rs` | Per-GVK `List` (or `Store` read in cache mode) + subset comparison for drift detection |
| `prune.rs` | Two-signal deletion of resources removed from Git; honors `resource-policy: keep` and Helm hooks |
| `state.rs` | Single ConfigMap persistence (`State` ↔ `BTreeMap<String,String>`) |
| `metrics.rs` | OpenTelemetry OTLP/HTTP (push) metrics; exposes `leancd_rss_bytes`. No HTTP listener. |
| `error.rs` | `thiserror` `Error` enum (`Git`, `Manifest`, `Kube`, `Config`, `Hook`, `Io`, `Other`) and `Result` alias |
| `version.rs` | Build-time version info: embeds the git SHA (via `build.rs`) for `--version` and the startup log |

## 3. The single binary and its four subcommands

`cli.rs` defines four subcommands. `controller` and `sync` are dispatched to
**the same `Reconciler`** — `controller` is just `sync` called repeatedly.

| Subcommand | Behavior | Entry |
|---|---|---|
| `controller` | Long-lived: initializes the OTel meter provider, then runs `run_loop()` until shutdown | `run_controller` |
| `sync` | One reconciliation pass (`run_once()`), then exits | `run_sync` |
| `status` | Reads the state ConfigMap and prints it, then exits (no reconciliation) | `run_status` |
| `health` | Reads the state ConfigMap, classifies freshness/staleness/failure for `exec` probes, then exits (no reconciliation) | `health::run_health` |

Because manual and automatic sync share `run_once`, the apply logic is
identical in both paths.

`main.rs` runs under `#[tokio::main(flavor = "current_thread")]` — a
single-threaded async runtime — to avoid per-thread stack memory.
`tracing_subscriber` is initialized from
`RUST_LOG` (default `info`). In `controller`, the reconciliation loop is spawned
as one task and the OTel meter provider is initialized; on `SIGINT` or `SIGTERM`
(`shutdown_signal`) a cooperative `stop` flag is set, the in-flight pass is
allowed to finish (the loop checks the flag between passes and short-circuits
its inter-pass sleep), and the meter provider is `shutdown()` to flush a final
export. If a pass does not finish within `shutdown_timeout`, the task is
`abort()`-ed as a fallback. `SIGHUP` reloads the `EnvFilter` from `RUST_LOG`.

`sync` and `status` are fire-and-forget: they construct a `Reconciler` (or just
a `Client` for `status`), do one pass, and return. `sync` also builds a meter
provider and flushes it on exit; `status` instruments nothing.

## 4. Reconciliation flow

`Reconciler::reconcile` (`reconcile.rs`) is the heart of the system. One pass:

1. **Read prior state.** `state::read` returns `Option<State>` from the state
   ConfigMap (`None` ⇒ first run). It carries the previous HEAD SHA and the
   previously-applied resource keys. An empty SHA is treated as absent.
2. **Git sync.** `git_sync::sync` shallow-fetches/clone, then compares the
   freshly-resolved HEAD SHA to the prior SHA → `changed: bool`. Short-circuits
   heavy work when nothing moved.
3. **Parse manifests.** `manifest::parse_dir` walks `work_dir/path`
   recursively, parsing every `*.yaml`/`*.yml` into untyped `RawManifest`s
   (streaming, one document at a time). `kind: List` is expanded recursively.
4. **Inject the managed label.** Every manifest gets
   `app.kubernetes.io/managed-by=leancd` (configurable) injected into
   `metadata.labels`.
5. **Decide full-apply vs drift-check** via `should_full_apply`:

   | `has_prev` | `changed` | full-apply? |
   |:---:|:---:|:---:|
   | false | false | **yes** (first run) |
   | false | true  | **yes** (first run) |
   | true  | false | **no** — drift-check only |
   | true  | true  | **yes** (HEAD moved) |

   i.e. `!has_prev || changed`. The only path that *skips* a full apply is
   steady state (prior state present, HEAD unchanged), which takes the
   drift-check branch instead.

6. **Apply or drift-check.**
   - Full-apply path: hooks and main resources are split by `hooks::classify`.
     `pre-install`/`pre-upgrade` hooks run (`hooks::run_phase`, PreSync) — awaited
     to completion for Job/Pod — then `apply_all` applies the **main** (non-hook)
     manifests, then `post-install`/`post-upgrade` hooks run (PostSync). A failed
     PreSync hook aborts the pass before the main apply.
   - Drift-check path: `drift::detect` lists live managed resources (main set
     only) and compares them; if any drift is found, the PreSync → main → PostSync
     sequence runs as above.
   - **Full teardown** (the main set is empty but a prior applied set exists):
     `pre-delete` hooks run, all previously-applied resources are pruned, then
     `post-delete` hooks run. An emptied repo/path is treated as teardown rather
     than a fatal "would prune everything" (only fatal when nothing was applied).
7. **Prune.** `prune::prune` deletes resources present in the prior applied set
   (and, as a safety net, live managed-by resources) that are absent from the
   current Git set. A live object carrying `helm.sh/resource-policy: keep` or
   `helm.sh/hook` is kept (hook resources are managed by the hook engine).
8. **Persist state.** A new `State` (new SHA, applied keys, counts, drift
   count, timestamp) is written back to the ConfigMap via SSA.
9. **Update metrics.** `managed_resources`, `drift_detected{group,version,kind}`
   (reset first so resolved drifts clear), `last_success_epoch` on success, or
   `sync_errors` on failure.

**Individual apply failures are logged, not fatal.** `apply_all` continues on
error and only logs a summary; a hard `Err` is only surfaced for git, state,
or discovery-stopping errors. `run_once` increments `sync_errors` and records
`last_error` into state when `reconcile` returns `Err`.

`run_loop` simply calls `run_once(false)` in a loop, sleeping `poll_interval`
between passes and logging (never aborting on) per-pass errors.

## 5. Git and the git CLI

Lean CD shells out to `git` (`tokio::process::Command`) rather than embedding a
Git library. The `git` CLI gives reliable repeated shallow fetches and resets
through a simple, battle-tested API; an embedded library such as `gix` would
re-implement that machinery and expose its low-level repeated-fetch surface for
no gain. (The benchmark samples both Lean CD's own RSS and the whole process
tree — Lean CD plus its git/ssh subprocesses — so git's footprint is accounted
for either way.)

`git_sync::sync` keeps a depth-1 shallow checkout:

- **Existing checkout** (`work_dir/.git` exists): `git fetch --depth 1 <url>
  <branch>` then `git reset --hard FETCH_HEAD`.
- **Fresh**: `git clone --depth 1 --branch <branch> <url> <work_dir>` (a stale
  `work_dir` is removed first).

The HEAD SHA is resolved with `git rev-parse HEAD` and compared to the prior
SHA.

Every `git` invocation sets `GIT_TERMINAL_PROMPT=0` (never block on an
interactive credential prompt) and `GIT_HTTP_USER_AGENT=leancd`. Transport
selection is driven by `Config::repo_kind`:

- **HTTPS** (`https://`/`http://`): if both `GIT_USERNAME` and `GIT_PASSWORD`
  are non-empty, basic auth is percent-encoded and embedded into the URL
  (`https://user:pass@host/...`) before it is handed to `git`. The authed URL is
  never logged.
- **SSH** (`git@...`/`ssh://...`): an injected PEM key (`GIT_SSH_KEY`) is
  materialised to a PID-scoped temp file
  (`<work_dir.parent>/.leancd_ssh_key_{pid}`, mode `0600`, with a trailing
  newline appended so OpenSSH parses the PEM). A PID-scoped
  `.leancd_known_hosts_{pid}` isolates the host-key store; `GIT_SSH_COMMAND`
  points `ssh` at the key with `-i`, `StrictHostKeyChecking=accept-new`, and
  `UserKnownHostsFile=<pid file>`. Both files are unlinked when the sync handle
  is dropped.
- **File** (`file://`, `/abs`, `./rel`): passed through unchanged.
- **Other**: passed through unchanged.

## 6. Manifest parsing

`manifest::parse_dir` recursively collects `*.yaml` and `*.yml` files under
`work_dir/path` and parses each with `serde_saphyr::from_multiple`
(one document at a time, so the full set is never held in memory at once).
`serde_saphyr` (granit-parser-based, actively maintained, no `unsafe`) is
already linked transitively via `kube`, so depending on it directly adds no
new code to the binary; it replaces the archived, deprecated `serde_yaml`.
`manifest.rs` funnels every `serde_saphyr` call through `pub(crate)` helpers,
and its default `Options` (`strict_booleans = false`) reproduce `serde_yaml`'s
YAML 1.1 boolean semantics. An unparseable document now fails the whole file
(previously the bad document was skipped and the rest kept); the caller
(`parse_dir`) logs and skips the file.

Each document becomes a `RawManifest` if it has `apiVersion`, `kind`, and
`metadata.name`; non-mapping, null, or incomplete documents are skipped (not
fatal). A document with `kind: List` is **recursively expanded** into its
`items`, so `List` manifests behave the same as separate files.

`RawManifest` holds the identity extracted from the document
(`group`/`version` from `apiVersion`, `kind`, `metadata.name`,
`metadata.namespace`) plus the whole document as an untyped `serde_json::Value`
(`data`). This lets Lean CD apply any resource kind — including CRDs and
cluster-scoped resources — through `DynamicObject` without typed structs.

Before apply, `inject_managed_label` writes the configured label
(`app.kubernetes.io/managed-by=leancd` by default) into `metadata.labels`,
creating `metadata`/`labels` if absent.

## 7. Applying resources — DynamicObject + server-side apply

`kube_util` is the only module that talks to the Kubernetes API, and it never
builds an informer or cache — every call is a direct `List`/`Get`/`Patch`/
`Delete` on a `DynamicObject` that returns immediately. A cluster-wide cache
would dominate RSS on large clusters, so it is avoided entirely.

- **Discovery.** `resolve(group, version, kind)` calls
  `kube::discovery::pinned_kind(client, &gvk)`, which returns
  `(ApiResource, ApiCapabilities)`. This is a cheap metadata round-trip; in
  `apply_all`, results are cached per GVK for the duration of the pass (a
  local `HashMap`, not a cluster-wide cache).
- **Scope-aware `Api`.** `api_for` picks `Api::all_with` for `Scope::Cluster`
  resources, or `Api::namespaced_with(obj.namespace or cfg.namespace)` for
  namespaced ones.
- **Server-side apply.** `apply` builds a `DynamicObject` from the manifest
  value and patches it with `Patch::Apply(&obj)` under
  `PatchParams::apply(field_manager).force()`, which always claims ownership of
  conflicting fields.
- **List / Delete.** `list_all` supports an optional label selector (used by drift
  and prune); `delete` uses foreground cascade deletion
  (`DeleteParams::foreground()` → `propagationPolicy: Foreground`) so an owner
  resource is held behind a `foregroundDeletion` finalizer until its dependents
  are gone. The same policy is used for Helm-hook and full-teardown deletions.

`apply_all` iterates the manifest slice, resolving each GVK once (cached) and
applying each resource. Discovery and per-resource apply failures are logged
and counted, but do not abort the pass.

## 8. Drift detection — periodic List, Watch-triggered

Drift detection runs only on **steady-state passes** (prior state
present, HEAD unchanged) — every other pass is a full apply. Historically this
was periodic `List` only; Lean CD now **also watches** the `managed-by`
resources (`watch.rs`) so a cluster-side edit (`kubectl`, another controller)
wakes the reconcile loop within the debounce window instead of waiting up to
`poll_interval`. Three `--watch-mode`s, selected at runtime:

- `cache` (default): a `reflector` + `Store` per managed GVK; drift detection
  reads live state from the stores (`drift::detect_from_stores`) with **no
  per-pass `List`**. Holds a cache of all managed objects (scales with object
  count; measured to stay well under the budget).
- `trigger`: a `watcher` stream per managed GVK pokes a `Notify` on any
  `touched` event (an apply OR a delete); drift detection stays the List-based
  path below. Holds no object cache (the leanest mode).
- `off`: poll-only — today's original behavior.

The default is `cache`: it was measured (`bench/`, 15 ns × 18 resources) to
match `trigger` on RSS (≈16 MiB self, far under the 50 MiB budget) while
removing the per-pass `List` apiserver load. The watched-GVK set is rebuilt
(diffed) after every successful pass from the manifests just parsed, so streams
for kinds that leave Git are dropped and new kinds get streams without churning
stable streams. A watch-triggered reconcile goes through the identical
`run_once -> reconcile -> lock::acquire` path, so the Lease serialization is
unchanged.

`drift::detect`:

1. Collects the distinct GVKs in the manifest set.
2. For each GVK, resolves it and issues one `List` filtered by the managed-by
   label selector (`<key>=<value>`), so only Lean CD-managed resources are
   compared.
3. For each manifest, finds its live match by `(name, namespace)`:
   - No match ⇒ drift, reason `"missing in cluster"`.
   - Match ⇒ compare with `specs_differ`:
     - If the manifest has a `spec`, only `spec` is compared.
     - If the manifest has no `spec`, the **whole object** is compared so
       label/annotation drift on spec-less resources is still caught.
4. Comparison is a recursive **subset check** (`spec_subset`): the Git manifest
   is satisfied when every key it declares is present and recursively satisfied
   in the live object. Extra keys in live (server-injected defaults) are
   tolerated; a missing or diverging Git-declared key ⇒ drift.

If any drift is found, `reconcile` calls `apply_all` to re-apply the managed
set. Per-GVK drift counts are written to the `leancd_drift_detected` metric
(the gauge vec is reset each pass, so a drift that resolved clears on the next
pass).

## 9. Pruning — two signals

`prune::prune` deletes resources that Lean CD previously applied but that Git no
longer declares. The deletion set combines two signals:

1. **Primary** (`deletion_targets`): the persisted applied set (`prev`) minus
   the current Git set (`current`). This is a pure set difference — no API
   calls.
2. **Safety net**: for each GVK seen in `prev`, list live managed-by resources
   and add any that Git no longer declares. This recovers orphans even when a
   key was dropped from state.

GVKs never applied before are out of scope (state-light): a fully-empty `prev`
skips the safety net entirely — a deliberate trade-off favoring low RSS over
exhaustive API discovery.

Each candidate is resolved (discovery cached per GVK) and deleted with
foreground cascade (`DeleteParams::foreground()`): dependents are removed before
their owners. Delete failures are logged, not fatal.

`ResourceKey` (`group`, `version`, `kind`, `namespace`, `name`) is the stable
identity used for all set operations; it is derived both from manifests and
from live `DynamicObject`s (with the GVK supplied by the caller, since the API
server does not reliably populate `apiVersion`/`kind` on returned objects).

## 10. State model

State lives in a single ConfigMap, not a CRD or database — keeping both the
persistent and in-process footprint minimal. The ConfigMap is named
`<state-configmap>`
(default `leancd-state`) in `<namespace>` (default `default`).

`state::write` upserts the ConfigMap via SSA (under `field_manager`) and
deliberately does NOT stamp the managed-by label on it — the prune safety-net
lists live resources by that label, so an unlabeled state ConfigMap is
invisible to prune and Lean CD will not delete its own state every pass (BUG 2);
`state::read` returns `None` on a 404 (first run).
The `State` struct is serialized to JSON and stored under a single ConfigMap
data key, `state`:

| `State` field | Meaning |
|---|---|
| `last_sha` | Last applied commit SHA (empty ⇒ absent) |
| `last_sync_epoch` | Last sync, Unix seconds |
| `sync_count` | Number of passes (incremented even on error) |
| `last_error` | Last error message (cleared on success) |
| `drift_count` | Drifts detected on the last pass |
| `managed_count` | Resources managed on the last pass |
| `applied` | JSON array of `ResourceKey` applied on the last pass |

A corrupt `applied` JSON falls back to an empty array rather than failing, so a
single bad value cannot wedge reconciliation. `status` renders this ConfigMap.

## 11. Metrics

Lean CD exposes no HTTP endpoint. It instruments metrics with the OpenTelemetry
SDK and pushes them over OTLP/HTTP (protobuf, port 4318) to a collector at fixed
intervals (`PeriodicReader`, default 60s; `OTEL_METRIC_EXPORT_INTERVAL`). The
endpoint, protocol, headers, and timeout come from the standard
`OTEL_EXPORTER_OTLP_*` environment variables — Lean CD itself takes no metrics
flag. The meter provider is flushed (`shutdown()`) on controller exit.

Counters are incremented directly; the gauges are observable gauges backed by a
shared `Mutex`-guarded state, reported on each collection. RSS is read by an
observable-gauge callback via `procfs::process::Process::myself().statm()` —
`statm.resident * procfs::page_size()` — so it reflects the live process
footprint at collection time.

| Metric | Type | Labels | Updated |
|---|---|---|---|
| `leancd_sync_total` | counter | — | every `run_once` |
| `leancd_sync_errors_total` | counter | — | failed reconcile |
| `leancd_hooks_total` | counter | `phase`, `result` | per phase executed (presync/postsync/predelete/postdelete × succeeded/failed) |
| `leancd_sync_last_success_timestamp_seconds` | observable gauge | — | on success |
| `leancd_drift_detected` | observable gauge | `group`, `version`, `kind` | reset each pass, then set per GVK |
| `leancd_managed_resources` | observable gauge | — | each pass |
| `leancd_rss_bytes` | observable gauge | — | on each collection |

## 12. Runtime, concurrency, and failure modes

- **Single-threaded async runtime** (`current_thread`); `git` is a separate
  process; the OTel `PeriodicReader` runs metric export on its own thread.
- **Serialized reconciliation via a Lease.** `controller` and a concurrent
  `sync` (e.g. via `kubectl exec`, or in a separate Pod) both call the same
  `Reconciler::reconcile`, but only one pass at a time is allowed cluster-wide:
  `reconcile` first acquires a `coordination.k8s.io/v1` Lease
  (`{state-configmap}-reconcile-lock`, see `lock.rs`) and holds it for the whole
  pass (git fetch → apply → prune → state write). A pass that finds the lease
  held by another process skips with an INFO log (busy skip) rather than
  erroring, so `sync_errors` is not incremented and the controller does not
  back off. A crashed holder's lease is reclaimed after `lock_lease_duration`
  (default 60s) by the next passer. Passes also refresh the lease (`renewTime`)
  at the major await points and inside the hook-completion poll loop, so a long
  pass is never reclaimed as stale. This serialization is what makes the state
  ConfigMap safe without CAS: with passes serialized, the SSA `state::write`
  (under one `field_manager`) cannot lose updates. The PID-scoped `work_dir`
  (`Config::effective_work_dir`) is a second layer of defense: even if the lease
  were briefly lost, two processes in the same Pod never touch the same git
  checkout.
- **Error handling.** `reconcile` returns `Err` only for git/state/discovery-
  stopping errors; `run_once` records `last_error` and increments
  `sync_errors` on failure. Per-resource apply/prune/drift failures are `warn!`-
  logged and the pass continues. The controller loop never exits on a pass
  error — it logs and backs off: a failing pass waits an exponential
  `backoff_delay` (`backoff_base`·2ⁿ, capped at `backoff_max`), jittered to
  `[0.75x, 1.0x)`, before the next attempt, resetting to `poll_interval` on
  success. Shutdown is cooperative (see
  [§3](#3-the-single-binary-and-its-four-subcommands)).
- **`main`'s top-level errors** use `anyhow` (exit non-zero); library code uses
  `crate::error::{Error, Result}` (a `thiserror` enum).

## 13. Deployment shape

Lean CD runs as a Kubernetes `Deployment` (one replica, `strategy: Recreate`).
The shipped Helm chart ([`../charts/leancd/`](../charts/leancd/)) renders:

- a `Namespace`, `ServiceAccount`, a broad `ClusterRole`/`ClusterRoleBinding`
  (Lean CD applies arbitrary kinds including CRDs and cluster-scoped resources,
  so the default is broad — narrow it in production with `rbac.namespaced=true`),
  and
- the `Deployment` (image `leancd:latest`, `imagePullPolicy: IfNotPresent`,
  `args: ["controller"]`, `LEANCD_*` env, credentials via `envFrom` a Secret
  marked `optional`, resources request 32Mi/50m and limit 128Mi/200m).

Lean CD runs no HTTP listener: it pushes metrics over OTLP/HTTP, so the manifest
declares no `Service` or exposed port — point it at your own collector via
`OTEL_EXPORTER_OTLP_ENDPOINT`.

The runtime image is `debian:bookworm-slim` plus `git`, `ca-certificates`, and
`openssh-client` (the latter two for HTTPS and SSH transports; `git` because
Lean CD shells out to it). The multi-stage [`../Dockerfile`](../Dockerfile)
builds a release binary and copies it into the slim runtime.

See [`./tutorial.md`](./tutorial.md) for a worked deployment into a `kind`
cluster, and [`./user-manual.md`](./user-manual.md) §Deploy for the manifest
breakdown and RBAC guidance.

## 14. What this implementation does not do

Mirroring the project non-goals ([`../README.md`](../README.md)):

- No Kustomize, Helm, or Jsonnet — plain YAML only.
- No owner-reference traversal — pruning is by the persisted applied set plus
  the managed-by label, not by ownership graphs.
- No notifications, no web UI, no webhook receiver.
- One process = one repository + one path.

Implementation caveats (the memory strategy that keeps RSS minimal — all
subject to the correctness-first invariant above; the reconcile Lease in
`lock.rs` is the one deliberate structural cost accepted in its name):

- Watch is now used (`watch.rs`) as a cluster-side-drift trigger (default
  `--watch-mode=cache`). In `trigger` mode it holds no object cache; in `cache`
  mode it holds a per-GVK size-bounded `LightweightStore` (replacing the old
  reflector `Store`): objects ≤ `--cache-max-object-bytes` are cached in full
  (SmallTier) for a no-`List` drift check, while larger objects are tracked by
  key only (LargeTier) and drift-checked via a per-GVK `List` fallback. The
  cache's RSS therefore does not grow with per-object payload size, for any
  resource kind. Outside the watch drivers, every interaction is still a direct
  `List`/`Get`/`Patch`/`Delete` — there is no DB or global index.
- No persistent cache across passes — each reconcile is self-contained,
  re-discovering what it needs and discarding it.
- No global or background caches; intermediate data is scoped to a single
  reconcile and freed when it ends.
- Git is a depth-1 shallow clone, so no history objects are ever held; only
  the current working tree is parsed.
- Manifests are parsed one document at a time (streaming), never loaded as a
  whole.
- The async runtime is single-threaded (`current_thread`) to avoid per-thread
  stack memory.
- TLS uses rustls rather than OpenSSL for licensing/supply-chain reasons; the
  dependency set is gated by `cargo-deny` (Apache-2.0-compatible licenses; see
  `deny.toml`). The dependency count is not minimized for its own sake — runtime
  RSS is the metric that matters.
- Library vs hand-rolled is an empirical choice, not a doctrinal one: a new
  dependency does not automatically raise RSS, and a hand-rolled implementation
  is not automatically leaner. When both are viable, implement each and measure
  actual RSS with `make bench` before choosing.

## 15. Cross-references

- [`../README.md`](../README.md) — project overview and quick start.
- [`./user-manual.md`](./user-manual.md) — complete flag/env/metric reference,
  tuning, and troubleshooting.
- [`./tutorial.md`](./tutorial.md) — hands-on deployment into a `kind` cluster.
