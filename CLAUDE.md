# CLAUDE.md

## IMPORTANT

- **TDD**: When adding a feature or fixing a bug, write a failing test first, confirm it fails, then implement the fix.
- **Pre-commit formatting**: Always run `make fmt` before `git commit`.
- **Update documentation**: When adding or modifying a feature, update README.md and the `--help` output (clap `#[command]`/`#[arg]` attributes in `main.rs`) accordingly.

## What this is

**leancd** is a minimal, low-memory Kubernetes continuous-delivery controller written in Rust. It syncs plain YAML manifests from a Git repository into the cluster it runs in, detects drift, and self-heals — like Argo CD / Flux CD, but with one overriding constraint: the process RSS must stay **≤ 100MiB** at all times (idle and sync peak).

This memory budget is **the headline goal** and is verified by an automated benchmark. Every design and implementation decision is justified against "does this increase RSS?". When in doubt, prefer fewer allocations, no caching, no background stores, and shelling out to `git` over embedding a Git library.

Rationale summary: RSS ≤ 100MiB is the headline goal, favoured over feature breadth, real-time responsiveness, and implementation convenience (Argo CD runs at hundreds of MiB–GiB, Flux at tens–100+MiB; leancd targets ≤100MiB). The trade-offs that enforce it — no cluster-wide cache, no background state, shallow clones, streaming YAML parses, a single-threaded runtime, minimal dependencies — are detailed in `doc/architecture.md` §14.

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
                               #   Forgejo + leancd Pods (leancd's intended
                               #   behaviour). #[ignore]d, so not in nix flake check.
                               #   Concurrency (controller vs sync running at once,
                               #   safe via one SSA fieldManager) is unit-test
                               #   territory, out of e2e scope.
make test                      # == nix flake check : full CI (fmt, clippy -D warnings,
                               #   nextest, cargo-deny, cargo-audit)
```

The project is Nix-flake based. `direnv` (`.envrc`) loads the flake, which provides the toolchain, plus `curl`, `kind`, `kubectl` in the dev shell. `make test` runs the complete CI gate (clippy denies warnings, `cargo-deny` allows any license compatible with the project's Apache-2.0 — permissive licenses plus MPL-2.0, strong copyleft excluded; see `deny.toml`).

**RSS benchmark** (`make bench` / `./bench/bench.sh`): spins up a `kind` cluster, generates N namespaces × Deployment/StatefulSet/ConfigMap/Service into a local Git repo, runs a release build of leancd against it, and samples two footprints in parallel: the **self** RSS (leancd's own process, read via `ps`) and the **tree** RSS (leancd + git/ssh subprocesses, summed via `ps` — shared pages double-counted, so deliberately conservative). It fails if any of the self/tree peak/idle RSS ≥ the budget. Tunables: `BENCH_NAMESPACE_COUNT` (default 15), `RSS_BUDGET_MIB` (default 100), `BENCH_SAMPLE_SECS` (default 30), `KIND_CLUSTER_NAME`.

## Architecture

### Single binary, three subcommands, one shared engine

`clap` (derive) defines `controller`, `sync`, and `status` (`cli.rs`). `controller` (long-lived, the `Deployment`) and `sync` (one pass) are dispatched to **the same `Reconciler`** (`reconcile.rs`) — `run_loop()` just calls `run_once()` on a poll interval, `sync` calls `run_once()` once and exits. This guarantees manual and automatic sync use identical apply logic. `status` is read-only (reads the state ConfigMap).

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
- **`git` via shell-out** (`tokio::process::Command`), chosen deliberately over an embedded Git library (`gix`): git runs as a separate process so its memory is **not** counted in leancd's RSS, and shelling out gives reliable repeated fetches without the low-level API risk of `gix`.
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
| `prune.rs` | set-diff deletion of resources removed from Git (`ResourceKey` identity); keeps `resource-policy: keep` and Helm-hook resources |
| `drift.rs` | per-GVK `List` + subset comparison |
| `state.rs` | single ConfigMap persistence (`State` ↔ `BTreeMap<String,String>`) |
| `metrics.rs` | OpenTelemetry OTLP/HTTP (push) metrics via `PeriodicReader`; exposes `leancd_rss_bytes`. No HTTP listener. |

## Implementation notes worth remembering

- **`serde_yaml` is intentional, despite deprecation.** It supports the streaming `Deserializer` that `manifest.rs` needs; `serde_yml` lacks an equivalent streaming-from-string API. `manifest.rs` carries `#![allow(deprecated)]` on purpose. kube-rs also depends on `serde_yaml`.
- **kube-rs v3 discovery API**: `kube::discovery::pinned_kind(client, &gvk)` returns `(ApiResource, ApiCapabilities)`; use `caps.scope` (`Scope::Cluster` vs namespaced) to pick `Api::all_with` vs `Api::namespaced_with`. SSA is `api.patch(name, &PatchParams::apply(fm).force(), &Patch::Apply(&obj))`.
- **TLS is rustls** (`features = ["rustls-tls"]`) — OpenSSL is intentionally avoided.
- **Managed-by label** (`app.kubernetes.io/managed-by=leancd` by default) is injected at apply time on every resource. Pruning uses the persisted applied-set as the primary signal and the label as the safety net.
- **Individual apply failures are logged, not fatal** (`apply_all` continues on error); a hard failure is only surfaced for git/state/discovery-stopping errors.

## Conventions when editing

- Match the existing style: module-level `//!` doc comments, `tracing` structured logs (`tracing::warn!`/`info!` with `error = %e`), `crate::error::{Error, Result}` (thiserror enum) rather than `anyhow` in library code (`anyhow` is only at `main`'s top level).
- Keep new dependencies minimal — remember `cargo-deny` only allows Apache-2.0-compatible licenses (see `deny.toml`). Evaluate any added dependency against the RSS budget first.
- When changing kube interaction code, confirm it issues direct API calls and builds no cache.
