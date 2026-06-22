# CLAUDE.md

## IMPORTANT

- **TDD (all three test layers)**: When adding a feature or fixing a bug, write a failing test first, confirm it fails, then implement the fix. Cover the change across **all three test layers** as appropriate — **unit tests** (`#[cfg(test)]` in each module), **integration tests** (cluster-free multi-module coverage under `tests/`), and **e2e tests** (`tests/e2e.rs`, a `kind` cluster with in-cluster Forgejo + Lean CD). Don't satisfy a change with a single layer when more apply.
- **Run `nix flake check` before finishing or committing**: `nix flake check` (== `make test`) is the full CI gate — fmt (cargo + taplo), clippy (`-D warnings`), unit + integration tests (nextest), cargo-deny, cargo-audit, helm lint, helm template. Never mark a task done or run `git commit` until it is green, **and** run `make e2e` for the e2e layer. Fix failures before moving on; never skip, `#[ignore]`, or work around a failing test.
- **Pre-commit formatting**: Always run `make fmt` before `git commit`.
- **Update documentation**: When adding or modifying a feature, update README.md and the `--help` output (clap `#[command]`/`#[arg]` attributes in `main.rs`) accordingly.

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

**RSS benchmark** (`make bench` / `./bench/bench.sh`): spins up a `kind` cluster, generates N namespaces × Deployment/StatefulSet/ConfigMap/Service into a local Git repo, runs a release build of Lean CD against it, and samples two footprints in parallel: the **self** RSS (Lean CD's own process, read via `ps`) and the **tree** RSS (Lean CD + git/ssh subprocesses, summed via `ps` — shared pages double-counted, so deliberately conservative). It fails if any of the self/tree peak/idle RSS ≥ the budget. Tunables: `BENCH_NAMESPACE_COUNT` (default 15), `RSS_BUDGET_MIB` (default 100), `BENCH_SAMPLE_SECS` (default 30), `KIND_CLUSTER_NAME`.

## Architecture

### Single binary, four subcommands, one shared engine

`clap` (derive) defines `controller`, `sync`, `status`, and `health` (`cli.rs`), plus `--version`. `controller` (long-lived, the `Deployment`) and `sync` (one pass) are dispatched to **the same `Reconciler`** (`reconcile.rs`) — `run_loop()` just calls `run_once()` on a poll interval, `sync` calls `run_once()` once and exits. This guarantees manual and automatic sync use identical apply logic. `status` and `health` are read-only (they read the state ConfigMap); `health` classifies freshness for an `exec` liveness/readiness probe.

### Reconciliation flow (`reconcile.rs::reconcile`)

1. Read prior state from the state ConfigMap (`state.rs`) — previous HEAD SHA + previously-applied resource keys.
2. `git_sync::sync` — shallow `fetch`/`clone` (`--depth 1`), compare HEAD SHA → `changed` bool. Short-circuits heavy work when nothing moved.
3. `manifest::parse_dir` — streaming, per-document YAML parse into untyped `RawManifest`s; inject the managed-by label into each.
4. **Full apply** when `first run || sha changed`; otherwise **drift-check** (`drift::detect`) and re-apply only if drift is found. `hooks::classify` splits manifests into phases; on a full apply the order is **PreSync hooks → `apply_all`(main) → PostSync hooks** (Job/Pod hooks are awaited to completion; a failed PreSync hook aborts the pass). A **full teardown** (main set empty, prior applied set non-empty) runs **pre-delete → prune all → post-delete**.
5. `prune::prune` — delete keys in the prior applied set that are absent from the current Git set. Live objects with `helm.sh/resource-policy: keep` or `helm.sh/hook` are kept (hooks are managed by `hooks.rs`, not the prune set-diff).
6. Write updated state (new SHA, applied **main** keys, counts) back to the ConfigMap; update OTel instruments.

### Memory strategy — do not violate these

- **No kube-rs informer/cache.** `kube_util.rs` never builds a `Controller` or `Store`. Every interaction is a direct `List`/`Get`/`Patch` on `DynamicObject` and returns immediately.
- **Drift via periodic `List`, never `Watch`.** One `List` per managed GVK (filtered by the managed-by label), then a recursive subset check (`spec_subset`) that tolerates server-injected defaults.
- **`git` via shell-out** (`tokio::process::Command`): the `git` CLI gives reliable repeated shallow fetches through a simple, well-trodden API. (The benchmark samples both Lean CD's own RSS and the whole process tree — Lean CD plus its git/ssh subprocesses — so git's footprint is accounted for either way.)
- **Streaming YAML parse** (`serde_yaml::Deserializer`) — one document at a time.
- **State is one ConfigMap + a label**, not a CRD or DB. Process memory holds only: last SHA, lightweight API-discovery metadata (GVK → `ApiResource`, no resource bodies), and transient pass state.
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
| `drift.rs` | per-GVK `List` + subset comparison |
| `state.rs` | single ConfigMap persistence (`State` ↔ `BTreeMap<String,String>`) |
| `metrics.rs` | OpenTelemetry OTLP/HTTP (push) metrics via `PeriodicReader`; exposes `leancd_rss_bytes`. No HTTP listener. |
| `error.rs` | `thiserror` `Error` enum (`Git`, `Manifest`, `Kube`, `Config`, `Hook`, `Io`, `Other`) and `Result` alias |
| `health.rs` | `health` subcommand: classifies the last sync state (fresh/never/stale/failing) for `exec` liveness/readiness probes |
| `version.rs` | build-time version info: embeds the git SHA (via `build.rs`) for `--version` and the startup log |

## Implementation notes worth remembering

- **`serde_yaml` is intentional, despite deprecation.** It supports the streaming `Deserializer` that `manifest.rs` needs; `serde_yml` lacks an equivalent streaming-from-string API. `manifest.rs` carries `#![allow(deprecated)]` on purpose. kube-rs also depends on `serde_yaml`.
- **kube-rs v3 discovery API**: `kube::discovery::pinned_kind(client, &gvk)` returns `(ApiResource, ApiCapabilities)`; use `caps.scope` (`Scope::Cluster` vs namespaced) to pick `Api::all_with` vs `Api::namespaced_with`. SSA is `api.patch(name, &PatchParams::apply(fm).force(), &Patch::Apply(&obj))`.
- **TLS is rustls** (`features = ["rustls-tls"]`) — OpenSSL is intentionally avoided.
- **Managed-by label** (`app.kubernetes.io/managed-by=leancd` by default) is injected at apply time on every resource. Pruning uses the persisted applied-set as the primary signal and the label as the safety net.
- **Individual apply failures are logged, not fatal** (`apply_all` continues on error); a hard failure is only surfaced for git/state/discovery-stopping errors.

## Conventions when editing

- Match the existing style: module-level `//!` doc comments, `tracing` structured logs (`tracing::warn!`/`info!` with `error = %e`), `crate::error::{Error, Result}` (thiserror enum) rather than `anyhow` in library code (`anyhow` is only at `main`'s top level).
- A new dependency does not automatically raise RSS, and a hand-rolled implementation is not automatically leaner — both are empirical questions. When a choice exists between reusing an existing library and hand-rolling, implement **both**, measure actual RSS (via `make bench`), and keep whichever is smaller (or simpler, when RSS is comparable). The dependency count is not a concern in itself; RSS and license compatibility are. Remember `cargo-deny` only allows Apache-2.0-compatible licenses (see `deny.toml`).
- When changing kube interaction code, confirm it issues direct API calls and builds no cache.

## Documentation conventions

- **Product name in prose is "Lean CD".** In documentation, READMEs, code comments, and commit messages, refer to the product as **Lean CD**. The lowercase identifier `leancd` is a *different* thing — the binary/CLI name (`leancd controller`, `leancd --version`), `LEANCD_*` env vars, the container image and Helm chart/repo name (`ghcr.io/.../leancd`, `charts/leancd`), Kubernetes resource names (`leancd-state`, `leancd-git-credentials`), metric names (`leancd_rss_bytes`), the `managed-by=leancd` label value, and Rust crate/module paths (`leancd::reconcile`) — and is **not** rewritten in prose. Keep CLI examples (`leancd --version`, `helm install leancd ...`) verbatim.
- **American English.** Documentation, commit messages, and code comments use American spellings: behavior (not behaviour), labeled (not labelled), honored (not honoured), initializes (not initialises), favored (not favoured), judgment (not judgement), and so on.
