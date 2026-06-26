# CLAUDE.md

## IMPORTANT

- **TDD (all three test layers)**: When adding a feature or fixing a bug, write a failing test first, confirm it fails, then implement the fix. Cover the change across **all three test layers** as appropriate — **unit tests** (`#[cfg(test)]` in each module), **integration tests** (cluster-free multi-module coverage under `tests/`), and **e2e tests** (`tests/e2e.rs`, a `kind` cluster with in-cluster Forgejo + Lean CD). Don't satisfy a change with a single layer when more apply.
- **Run `nix flake check` before finishing or committing**: `nix flake check` (== `make test`) is the full CI gate — fmt (cargo + taplo), clippy (`-D warnings`), unit + integration tests (nextest), cargo-deny, cargo-audit, helm lint, helm template. Never mark a task done or run `git commit` until it is green, **and** run `make e2e` for the e2e layer. Fix failures before moving on; never skip, `#[ignore]`, or work around a failing test.
- **Pre-commit formatting**: Always run `make fmt` before `git commit`.
- **Update documentation**: When adding or modifying a feature, update README.md and the `--help` output (clap `#[command]`/`#[arg]` attributes in `cli.rs`) accordingly.
- **Update CHANGELOG.md**: Every code change — feature, fix, refactor, docs-only edit, or test addition — must add an entry to `CHANGELOG.md` under the `[Unreleased]` heading in the matching Keep a Changelog category (`Added` / `Changed` / `Deprecated` / `Removed` / `Fixed` / `Security`). This is on par with the rules above; do not mark a task done with an unchanged changelog. See "Documentation conventions" below for the format.

## What this is

**Lean CD** is a minimal, low-memory Kubernetes continuous-delivery controller written in Rust. It syncs plain YAML manifests from a Git repository into the cluster it runs in, detects drift, and self-heals — like Argo CD / Flux CD.

**Correctness is the higher-order invariant; the memory budget is subordinate to it.** `sync` must never leave the cluster in an incorrect state — in particular, concurrent `controller` and `sync` passes (same Pod via `kubectl exec`, or a separate Pod) must not race on the git checkout or clobber sync state. This is enforced by serializing each reconcile pass through a Kubernetes Lease (`lock.rs`, one pass at a time cluster-wide, with stale-lease reclaim). The cost is constant-order and adds no crate dependencies. If correctness and the RSS budget ever conflict, **correctness wins**.

Subject to that invariant, the overriding constraint is that process RSS stays **strictly small** at all times (idle and sync peak) — minimizing memory consumption is **the headline goal**, enforced against a tunable budget by an automated benchmark (see `bench/`). Every design and implementation decision is justified against "does this increase RSS? (without breaking correctness)". When in doubt, prefer fewer allocations, no caching, and no background stores.

Rationale summary: a minimal process-RSS footprint is the headline goal, favored over feature breadth, real-time responsiveness, and implementation convenience (Argo CD runs at hundreds of MiB–GiB, Flux at tens–100+MiB; Lean CD targets a far smaller footprint). The trade-offs that enforce it — no cluster-wide cache, no background state, shallow clones, streaming YAML parses, a single-threaded runtime — are detailed in `doc/architecture.md` §14.

## Commands

```sh
cargo build                    # debug build
cargo build --release          # release build (what the benchmark runs)
cargo test                     # unit tests (each module has #[cfg(test)])
cargo test <name>              # run a single test by name substring
make all                       # fmt + build + test-unit
make fmt                       # cargo fmt + taplo fmt (Cargo.toml, taplo.toml, deny.toml)
make bench                     # RSS benchmark against a kind cluster (see below)
make e2e                       # end-to-end tests: kind cluster with in-cluster
                               #   Forgejo + Lean CD Pods (Lean CD's intended
                               #   behavior). #[ignore]d, so not in nix flake check.
                               #   Concurrency (controller vs sync running at once)
                               #   is serialized by a reconcile Lease (lock.rs);
                               #   the lease/stale logic is unit-tested and an e2e
                               #   scenario asserts final consistency under a
                               #   concurrent controller + sync.
make test                      # == nix flake check : full CI (fmt, clippy -D warnings,
                               #   nextest, cargo-deny, cargo-audit, helm lint, helm template)
```

The project is Nix-flake based. `direnv` (`.envrc`) loads the flake, which provides the toolchain, plus `curl`, `kind`, `kubectl`, `helm` in the dev shell. `make test` runs the complete CI gate (clippy denies warnings, `cargo-deny` allows any license compatible with the project's Apache-2.0 — permissive licenses plus MPL-2.0, strong copyleft excluded; see `deny.toml`).

**RSS benchmark** (`make bench` / `./bench/bench.sh`): spins up a `kind` cluster, generates N namespaces × Deployment/StatefulSet/ConfigMap/Service into a local Git repo, runs a release build of Lean CD against it, and samples two footprints in parallel: the **self** RSS (Lean CD's own process, read via `ps`) and the **tree** RSS (Lean CD + git/ssh subprocesses, summed via `ps` — shared pages double-counted, so deliberately conservative). It fails if any of the self/tree peak/idle RSS ≥ the budget. Tunables: `BENCH_NAMESPACE_COUNT` (default 15), `RSS_BUDGET_MIB` (default 50), `BENCH_SAMPLE_SECS` (default 30), `KIND_CLUSTER_NAME`.

## Architecture

### Single binary, four subcommands, one shared engine

`clap` (derive) defines `controller`, `sync`, `status`, and `health` (`cli.rs`), plus `--version`. `controller` (long-lived, the `Deployment`) and `sync` (one pass) are dispatched to **the same `Reconciler`** (`reconcile.rs`) — `run_loop()` just calls `run_once()` on a poll interval (in `cache`/`trigger` `--watch-mode`, `watch.rs` also wakes it on a cluster-side change within `--watch-debounce`, ahead of the poll interval), and `sync` calls `run_once()` once and exits. This guarantees manual and automatic sync use identical apply logic. `status` and `health` are read-only (they read the state ConfigMap); `health` classifies freshness for an `exec` liveness/readiness probe.

### Reconciliation flow (`reconcile.rs::reconcile`)

1. Read prior state from the state ConfigMap (`state.rs`) — previous HEAD SHA + previously-applied resource keys.
2. `git_sync::sync` — shallow `fetch`/`clone` (`--depth 1`), compare HEAD SHA → `changed` bool. Short-circuits heavy work when nothing moved.
3. `manifest::parse_dir` — streaming, per-document YAML parse into untyped `RawManifest`s; inject the managed-by label into each.
4. **Full apply** when `first run || sha changed`; otherwise **drift-check** and re-apply only if drift is found. The drift-check is `drift::detect` (per-GVK `List`, filtered by the managed-by label) in `off`/`trigger` `--watch-mode`, or `drift::detect_from_lw` (reads the per-GVK size-bounded `LightweightStore`) in `cache` (default). `hooks::classify` splits manifests into phases; on a full apply the order is **PreSync hooks → `apply_all`(main) → PostSync hooks** (Job/Pod hooks are awaited to completion; a failed PreSync hook aborts the pass). A **full teardown** (main set empty, prior applied set non-empty) runs **pre-delete → prune all → post-delete**.
5. `prune::prune` — delete keys in the prior applied set that are absent from the current Git set. Live objects with `helm.sh/resource-policy: keep` or `helm.sh/hook` are kept (hooks are managed by `hooks.rs`, not the prune set-diff).
6. Write updated state (new SHA, applied **main** keys, counts) back to the ConfigMap; update OTel instruments.

### Memory strategy — do not violate these

- **No kube-rs informer/cache outside the watch drivers.** `kube_util.rs` never builds a `Controller` or `Store` — every apply/get/delete/list is a direct call on `DynamicObject` and returns immediately. The one deliberate exception is `watch.rs` in `cache` mode (the default `--watch-mode`), which holds a per-GVK size-bounded `LightweightStore` (small objects cached in full, large ones by key only); this is a measured, budget-bounded structural cost — the same category as the reconcile `Lease` (`lock.rs`) — not a cluster-wide cache or background store.
- **Drift detection by managed-by subset check.** In `off`/`trigger` `--watch-mode`: one `List` per managed GVK (filtered by the managed-by label), then a recursive subset check (`spec_subset`) that tolerates server-injected defaults. In `cache` (default): live state is read from the per-GVK `LightweightStore` (`drift::detect_from_lw`); small objects are subset-checked from the cache (no per-pass `List`), while large objects (over `--cache-max-object-bytes`) fall back to a per-GVK `List`. A cluster-side change wakes `run_loop` via `watch.rs` within `--watch-debounce` instead of waiting up to `poll_interval`.
- **`git` via shell-out** (`tokio::process::Command`): the `git` CLI gives reliable repeated shallow fetches through a simple, well-trodden API. (The benchmark samples both Lean CD's own RSS and the whole process tree — Lean CD plus its git/ssh subprocesses — so git's footprint is accounted for either way.)
- **Multi-document YAML parse** (`serde_saphyr::from_multiple`) — one document at a time. All `serde_saphyr` calls are funneled through `pub(crate)` helpers in `manifest.rs`.
- **State is one ConfigMap + a label**, not a CRD or DB. Process memory holds: last SHA, lightweight API-discovery metadata (GVK → `ApiResource`; no resource bodies on the steady-state path), transient pass state, and — in `cache` mode only — the per-GVK managed-by `LightweightStore` (size-bounded, measured under budget).
- **`tokio` `current_thread` runtime** (`#[tokio::main(flavor = "current_thread")]`) to minimize thread/stack memory.

### Module map

| Module | Responsibility |
|---|---|
| `cli.rs` | clap subcommands + shared `CommonArgs` (flags map to `LEANCD_*` env vars) → `Config` |
| `config.rs` | validated `Config`; git-credential resolution from env; URL encoding |
| `git_sync.rs` | shallow fetch/clone + HEAD-SHA change detection via the `git` CLI |
| `manifest.rs` | streaming multi-doc YAML parse; GVK/ns/name extraction; managed-label injection; annotation read helper |
| `kube_util.rs` | API discovery (`pinned_kind`), dynamic `Api` construction (cluster vs namespaced), SSA `apply`, `list`, `get`, `delete` |
| `hooks.rs` | Helm-hook classification + execution: Argo CD-equivalent phase mapping (pre/post-install-upgrade, pre/post-delete), `hook-weight` ordering, `hook-delete-policy`, Job/Pod completion wait |
| `reconcile.rs` | the `Reconciler` engine shared by `controller`/`sync` |
| `lock.rs` | reconcile-pass mutual exclusion via a `coordination.k8s.io/v1` Lease (one pass at a time, cluster-wide); stale-lease reclaim |
| `prune.rs` | set-diff deletion of resources removed from Git (`ResourceKey` identity); keeps `resource-policy: keep` and Helm-hook resources |
| `watch.rs` | optional cluster-side-drift trigger (`--watch-mode` off/trigger/cache): per-GVK `watcher` drivers (consumed directly, no reflector) that wake `run_loop`; `cache` mode holds a per-GVK size-bounded `LightweightStore` |
| `drift.rs` | per-GVK `List` (or `LightweightStore` read in `cache` mode) + subset comparison |
| `state.rs` | single ConfigMap persistence (`State` ↔ `BTreeMap<String,String>`) |
| `metrics.rs` | OpenTelemetry OTLP/HTTP (push) metrics via `PeriodicReader` (`leancd_sync_total`, `leancd_sync_errors_total`, `leancd_hooks_total`, `leancd_sync_last_success_timestamp_seconds`, `leancd_drift_detected`, `leancd_managed_resources`, `leancd_rss_bytes`). No HTTP listener. |
| `error.rs` | `thiserror` `Error` enum (`Git`, `Manifest`, `Kube`, `Config`, `Hook`, `Io`, `Other`) and `Result` alias |
| `health.rs` | `health` subcommand: classifies the last sync state (fresh/never/stale/failing) for `exec` liveness/readiness probes |
| `version.rs` | build-time version info: embeds the git SHA (via `build.rs`) for `--version` and the startup log |

## Implementation notes worth remembering

- **`serde_saphyr` is the YAML library** (granit-parser-based, actively maintained, no `unsafe`). Already linked transitively via `kube`, so a direct dependency adds no new code to the binary; it replaces the archived `serde_yaml` (and the once-considered `serde_yml`, now also deprecated — RUSTSEC-2025-0068). `manifest.rs` funnels every `serde_saphyr` call through `pub(crate)` helpers so a future (pre-1.0) major bump touches only that module, and its default `Options` (`strict_booleans = false`) reproduces `serde_yaml`'s YAML 1.1 boolean semantics.
- **kube-rs v4 discovery API**: `kube::discovery::pinned_kind(client, &gvk)` returns `(ApiResource, ApiCapabilities)`; use `caps.scope` (`Scope::Cluster` vs namespaced) to pick `Api::all_with` vs `Api::namespaced_with`. SSA is `api.patch(name, &PatchParams::apply(fm).force(), &Patch::Apply(&obj))`.
- **TLS is rustls** (`features = ["rustls-tls"]`) — OpenSSL is intentionally avoided.
- **Managed-by label** (`app.kubernetes.io/managed-by=leancd` by default) is injected at apply time on every resource. Pruning uses the persisted applied-set as the primary signal and the label as the safety net.
- **Individual apply failures are logged, not fatal** (`apply_all` continues on error); a hard failure is only surfaced for git/state/discovery-stopping errors.
- **Watch is opt-in granularity, `cache` is the default.** `--watch-mode=cache` (default) holds a per-GVK size-bounded `LightweightStore` (replacing the old reflector `Store`); `trigger` holds none; `off` is poll-only. `cache` was chosen as default because it matches `trigger` on RSS (≈16 MiB self at 15 ns × 18 resources, far under the 50 MiB budget) while removing the per-pass `List` apiserver load for objects under `--cache-max-object-bytes`. The `kube` crate's `runtime` feature is enabled for `watcher` (the stream is consumed directly; the reflector is no longer used); `futures` (already a `kube` transitive dep) is promoted to consume the streams, and `jiff` (a `k8s-openapi` transitive dep) is promoted to build Lease renew/acquire times — neither adds new code to the binary.

## Conventions when editing

- Match the existing style: module-level `//!` doc comments, `tracing` structured logs (`tracing::warn!`/`info!` with `error = %e`), `crate::error::{Error, Result}` (thiserror enum) rather than `anyhow` in library code (`anyhow` is only at `main`'s top level).
- A new dependency does not automatically raise RSS, and a hand-rolled implementation is not automatically leaner — both are empirical questions. When a choice exists between reusing an existing library and hand-rolling, implement **both**, measure actual RSS (via `make bench`), and keep whichever is smaller (or simpler, when RSS is comparable). The dependency count is not a concern in itself; RSS and license compatibility are. Remember `cargo-deny` only allows Apache-2.0-compatible licenses (see `deny.toml`).
- When changing kube interaction code, confirm it issues direct API calls. The only cache is `watch.rs`'s per-GVK `LightweightStore` in `cache` mode; any new caching is a deliberate structural cost that must be measured (`make bench`) and kept under the RSS budget, like the reconcile `Lease`.

## Documentation conventions

- **Product name in prose is "Lean CD".** In documentation, READMEs, code comments, and commit messages, refer to the product as **Lean CD**. The lowercase identifier `leancd` is a *different* thing — the binary/CLI name (`leancd controller`, `leancd --version`), `LEANCD_*` env vars, the container image and Helm chart/repo name (`ghcr.io/.../leancd`, `charts/leancd`), Kubernetes resource names (`leancd-state`, `leancd-git-credentials`), metric names (`leancd_rss_bytes`), the `managed-by=leancd` label value, and Rust crate/module paths (`leancd::reconcile`) — and is **not** rewritten in prose. Keep CLI examples (`leancd --version`, `helm install leancd ...`) verbatim.
- **American English.** Documentation, commit messages, and code comments use American spellings: behavior (not behaviour), labeled (not labelled), honored (not honoured), initializes (not initialises), favored (not favoured), judgment (not judgement), and so on.
- **CHANGELOG.md follows [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/).** Every code change (feature, fix, refactor, docs-only edit, or test addition) adds an entry under the `[Unreleased]` heading in the matching category — `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, or `Security`. `make release` later moves `[Unreleased]` under a dated `[X.Y.Z]` heading; the file also adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (already declared in its header).
