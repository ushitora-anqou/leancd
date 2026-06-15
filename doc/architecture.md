# leancd Architecture

This document describes **how the current implementation works**. It is the
"how it works today" companion to [`design.md`](./design.md), which records the
*why* behind the design (goals, alternatives considered, memory-strategy
rationale). Where this document would stray into *why*, it links to
`design.md` instead of restating it.

For a quick overview see [`../README.md`](../README.md); for the complete
feature reference (every flag, env var, metric) see
[`./user-manual.md`](./user-manual.md); for a hands-on walkthrough see
[`./tutorial.md`](./tutorial.md).

## 1. Overview

leancd is a single static binary with three subcommands that all share one
reconciliation engine. One running process syncs exactly one Git repository
(one branch, one path) into the cluster it runs in.

```
        ┌─────────────────────────────────────────────┐
        │                  leancd                     │
        │                                             │
        │  controller / sync / status  ──► Reconciler │
        │                                      │      │
        │   ┌──────────┬──────────┬──────────┬───────┼───┐
        │   ▼          ▼          ▼          ▼       ▼   │
        │ git_sync  manifest  kube_util   drift/prune  state │
        │   │          │          │          │       │   │
        │   ▼          ▼          ▼          ▼       ▼   │
        │  git CLI   serde_yaml  kube API  kube API  CM  │
        └─────────────────────────────────────────────┘
                         │                  ▲
                         ▼                  │
                   Git repository      Kubernetes API
```

The overriding invariant is that process RSS stays **≤ 100MiB** at all times
(idle and at sync peak). Every mechanism below exists to preserve that
invariant; the reasoning is in [`design.md` §4](./design.md).

## 2. Module map

`src/` contains eleven modules. `reconcile` is the hub; `kube_util` is the only
boundary that touches the Kubernetes API; `main` wires the runtime.

| Module | Responsibility |
|---|---|
| `main.rs` | Entry point: `tokio` `current_thread` runtime, tracing, subcommand dispatch, graceful shutdown |
| `cli.rs` | `clap` subcommands (`controller`, `sync`, `status`) and shared `CommonArgs` → `Config` |
| `config.rs` | Validated `Config`; Git-transport classification; credential resolution; duration parser |
| `git_sync.rs` | Shallow `fetch`/`clone` and HEAD-SHA change detection via the `git` CLI |
| `manifest.rs` | Streaming multi-document YAML parse; GVK/ns/name extraction; `kind: List` expansion; managed-label injection |
| `kube_util.rs` | API discovery (`pinned_kind`), dynamic `Api` construction (cluster vs namespaced), SSA `apply`, `list`, `delete` |
| `reconcile.rs` | The `Reconciler` engine shared by `controller` and `sync` |
| `drift.rs` | Per-GVK `List` + subset comparison for drift detection |
| `prune.rs` | Two-signal deletion of resources removed from Git |
| `state.rs` | Single ConfigMap persistence (`State` ↔ `BTreeMap<String,String>`) |
| `metrics.rs` | Prometheus `/metrics` over a minimal `tokio::net` listener; exposes `leancd_rss_bytes` |
| `error.rs` | `thiserror` `Error` enum (`Git`, `Manifest`, `Kube`, `Config`, `Io`, `Other`) and `Result` alias |

## 3. The single binary and its three subcommands

`cli.rs` defines three subcommands. `controller` and `sync` are dispatched to
**the same `Reconciler`** — `controller` is just `sync` called repeatedly.

| Subcommand | Behaviour | Entry |
|---|---|---|
| `controller` | Long-lived: spawns the metrics server, then runs `run_loop()` until shutdown | `run_controller` |
| `sync [--force]` | One reconciliation pass (`run_once(force)`), then exits | `run_sync` |
| `status` | Reads the state ConfigMap and prints it, then exits (no reconciliation) | `run_status` |

Because manual and automatic sync share `run_once`, the apply logic is
identical in both paths.

`main.rs` runs under `#[tokio::main(flavor = "current_thread")]` — a
single-threaded async runtime — to minimise thread/stack memory (see
[`design.md` §4](./design.md)). `tracing_subscriber` is initialised from
`RUST_LOG` (default `info`). In `controller`, the reconciliation loop and the
metrics server are each spawned as tasks; on `SIGINT` or `SIGTERM`
(`shutdown_signal`) both task handles are `abort()`-ed and the process exits.

`sync` and `status` are fire-and-forget: they construct a `Reconciler` (or just
a `Client` for `status`), do one pass, and return. They start no metrics server.

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

   | `force` | `has_prev` | `changed` | full-apply? |
   |:---:|:---:|:---:|:---:|
   | false | false | false | **yes** (first run) |
   | false | false | true  | **yes** (first run) |
   | false | true  | false | **no** — drift-check only |
   | false | true  | true  | **yes** (HEAD moved) |
   | true  | *      | *     | **yes** (`--force`) |

   i.e. `force || !has_prev || changed`. The only path that *skips* a full
   apply is steady state (no force, prior state present, HEAD unchanged), which
   takes the drift-check branch instead.

6. **Apply or drift-check.**
   - Full-apply path: `apply_all` applies every manifest via server-side apply.
   - Drift-check path: `drift::detect` lists live managed resources and compares
     them; if any drift is found, `apply_all` is called to re-apply.
7. **Prune.** `prune::prune` deletes resources present in the prior applied set
   (and, as a safety net, live managed-by resources) that are absent from the
   current Git set.
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

## 5. Git and the git CLI — process isolation

leancd shells out to `git` (`tokio::process::Command`) rather than embedding a
Git library. Because `git` runs as a separate process, its memory is **not**
counted in leancd's RSS — this is the core reason the shell-out approach was
chosen (see [`design.md` §6 and 付録B](./design.md)).

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
`work_dir/path` and parses each with a streaming `serde_yaml::Deserializer`
(one document at a time, so the full set is never held in memory at once).
`serde_yaml` is used deliberately — despite being in maintenance mode it is the
stable, streaming-capable parser the design requires (see
[`design.md` 付録B](./design.md)); `manifest.rs` carries
`#![allow(deprecated)]` on purpose.

Each document becomes a `RawManifest` if it has `apiVersion`, `kind`, and
`metadata.name`; non-mapping, null, or incomplete documents are skipped (not
fatal). A document with `kind: List` is **recursively expanded** into its
`items`, so `List` manifests behave the same as separate files.

`RawManifest` holds the identity extracted from the document
(`group`/`version` from `apiVersion`, `kind`, `metadata.name`,
`metadata.namespace`) plus the whole document as an untyped `serde_json::Value`
(`data`). This lets leancd apply any resource kind — including CRDs and
cluster-scoped resources — through `DynamicObject` without typed structs.

Before apply, `inject_managed_label` writes the configured label
(`app.kubernetes.io/managed-by=leancd` by default) into `metadata.labels`,
creating `metadata`/`labels` if absent.

## 7. Applying resources — DynamicObject + server-side apply

`kube_util` is the only module that talks to the Kubernetes API, and it never
builds an informer or cache — every call is a direct `List`/`Get`/`Patch`/
`Delete` on a `DynamicObject` that returns immediately (see
[`design.md` §4](./design.md)).

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
  `PatchParams::apply(field_manager)` (`.force()` when `--force`), which claims
  ownership of conflicting fields.
- **List / Delete.** `list` supports an optional label selector (used by drift
  and prune); `delete` uses `DeleteParams::default()`.

`apply_all` iterates the manifest slice, resolving each GVK once (cached) and
applying each resource. Discovery and per-resource apply failures are logged
and counted, but do not abort the pass.

## 8. Drift detection — periodic List, not Watch

Drift detection runs only on **steady-state passes** (no force, prior state
present, HEAD unchanged) — every other pass is a full apply. This is done with
periodic `List` calls, never `Watch` (see [`design.md` §4](./design.md)).

`drift::detect`:

1. Collects the distinct GVKs in the manifest set.
2. For each GVK, resolves it and issues one `List` filtered by the managed-by
   label selector (`<key>=<value>`), so only leancd-managed resources are
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

`prune::prune` deletes resources that leancd previously applied but that Git no
longer declares. The deletion set combines two signals (see
[`design.md` 付録B](./design.md)):

1. **Primary** (`deletion_targets`): the persisted applied set (`prev`) minus
   the current Git set (`current`). This is a pure set difference — no API
   calls.
2. **Safety net**: for each GVK seen in `prev`, list live managed-by resources
   and add any that Git no longer declares. This recovers orphans even when a
   key was dropped from state.

GVKs never applied before are out of scope (state-light): a fully-empty `prev`
skips the safety net entirely — a deliberate trade-off documented in
[`design.md` 付録B](./design.md).

Each candidate is resolved (discovery cached per GVK) and deleted with
`DeleteParams::default()`. Delete failures are logged, not fatal.

`ResourceKey` (`group`, `version`, `kind`, `namespace`, `name`) is the stable
identity used for all set operations; it is derived both from manifests and
from live `DynamicObject`s (with the GVK supplied by the caller, since the API
server does not reliably populate `apiVersion`/`kind` on returned objects).

## 10. State model

State lives in a single ConfigMap, not a CRD or database (see
[`design.md` §4](./design.md)). The ConfigMap is named `<state-configmap>`
(default `leancd-state`) in `<namespace>` (default `default`).

`state::write` upserts the ConfigMap via SSA (under `field_manager`) and stamps
the managed-by label on it; `state::read` returns `None` on a 404 (first run).
The `State` struct round-trips to/from a `BTreeMap<String,String>` of plain
string data:

| Key | Meaning |
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

`metrics::serve` binds a `tokio::net::TcpListener` to `metrics_addr` and, per
connection, refreshes `leancd_rss_bytes`, gathers the registry, and writes a
single HTTP/1.1 response with
`Content-Type: text/plain; version=0.0.4; charset=utf-8`. Only `/metrics` is
served; pull-based scraping only (no push queue).

RSS is read on every scrape via `procfs::process::Process::myself().statm()`
— `statm.resident * procfs::page_size()` — so the gauge is always fresh and
reflects the live process footprint, not a periodic sample.

| Metric | Type | Labels | Updated |
|---|---|---|---|
| `leancd_sync_total` | counter | — | every `run_once` |
| `leancd_sync_errors_total` | counter | — | failed reconcile |
| `leancd_sync_last_success_timestamp_seconds` | gauge | — | on success |
| `leancd_drift_detected` | gauge vec | `group`, `version`, `kind` | reset each pass, then set per GVK |
| `leancd_managed_resources` | gauge | — | each pass |
| `leancd_rss_bytes` | gauge | — | on each scrape |

## 12. Runtime, concurrency, and failure modes

- **Single-threaded async runtime** (`current_thread`); `git` is a separate
  process; the metrics server spawns one short-lived task per scrape.
- **Idempotent applies.** `controller` and a concurrent `sync` (e.g. via
  `kubectl exec`) both use SSA under one `field_manager`, so concurrent
  reconciles are safe; the worst case is redundant work.
- **Error handling.** `reconcile` returns `Err` only for git/state/discovery-
  stopping errors; `run_once` records `last_error` and increments
  `sync_errors` on failure. Per-resource apply/prune/drift failures are `warn!`-
  logged and the pass continues. The controller loop never exits on a pass
  error — it logs and sleeps until the next tick.
- **`main`'s top-level errors** use `anyhow` (exit non-zero); library code uses
  `crate::error::{Error, Result}` (a `thiserror` enum).

## 13. Deployment shape

leancd runs as a Kubernetes `Deployment` (one replica, `strategy: Recreate`).
The shipped manifest ([`../deploy/leancd.yaml`](../deploy/leancd.yaml)) creates:

- a `Namespace`, `ServiceAccount`, a broad `ClusterRole`/`ClusterRoleBinding`
  (leancd applies arbitrary kinds including CRDs and cluster-scoped resources,
  so the default is broad — narrow it in production),
- the `Deployment` (image `leancd:latest`, `imagePullPolicy: IfNotPresent`,
  `args: ["controller"]`, `LEANCD_*` env, credentials via `envFrom` a Secret
  marked `optional`, resources request 32Mi/50m and limit 128Mi/200m), and
- a `Service` exposing the metrics port.

The runtime image is `debian:bookworm-slim` plus `git`, `ca-certificates`, and
`openssh-client` (the latter two for HTTPS and SSH transports; `git` because
leancd shells out to it). The multi-stage [`../Dockerfile`](../Dockerfile)
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

Implementation caveats (memory strategy, see [`design.md` §4](./design.md)):

- No `kube-rs` informer/cache; no `Watch`; no DB or global index.
- No persistent cache across passes — each reconcile is self-contained,
  re-discovering what it needs and discarding it.

## 15. Cross-references

- [`design.md`](./design.md) — design rationale (Japanese): goals, memory
  strategy, technology selection, and 付録B implementation notes.
- [`../README.md`](../README.md) — project overview and quick start.
- [`./user-manual.md`](./user-manual.md) — complete flag/env/metric reference,
  tuning, and troubleshooting.
- [`./tutorial.md`](./tutorial.md) — hands-on deployment into a `kind` cluster.
