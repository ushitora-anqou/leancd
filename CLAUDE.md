# CLAUDE.md

## IMPORTANT

- **TDD**: When adding a feature or fixing a bug, write a failing test first, confirm it fails, then implement the fix.
- **Pre-commit formatting**: Always run `make fmt` before `git commit`.
- **Update documentation**: When adding or modifying a feature, update README.md and the `--help` output (clap `#[command]`/`#[arg]` attributes in `main.rs`) accordingly.

## What this is

**leancd** is a minimal, low-memory Kubernetes continuous-delivery controller written in Rust. It syncs plain YAML manifests from a Git repository into the cluster it runs in, detects drift, and self-heals — like Argo CD / Flux CD, but with one overriding constraint: the process RSS must stay **≤ 100MiB** at all times (idle and sync peak).

This memory budget is **the headline goal** and is verified by an automated benchmark. Every design and implementation decision is justified against "does this increase RSS?". When in doubt, prefer fewer allocations, no caching, no background stores, and shelling out to `git` over embedding a Git library.

Full rationale: [`doc/design.md`](doc/design.md) (Japanese). Read at least §1 (goals), §4 (memory strategy), and 付録 B (implementation notes — several design-doc statements were changed during implementation).

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
                               #   Forgejo + leancd Pods (design.md behaviour).
                               #   #[ignore]d, so not in nix flake check.
                               #   Concurrency (design §3.4) is unit-test
                               #   territory, out of e2e scope.
make test                      # == nix flake check : full CI (fmt, clippy -D warnings,
                               #   nextest, cargo-deny, cargo-audit)
```

The project is Nix-flake based. `direnv` (`.envrc`) loads the flake, which provides the toolchain, plus `curl`, `kind`, `kubectl` in the dev shell. `make test` runs the complete CI gate (clippy denies warnings, `cargo-deny` allows permissive licenses only — MIT/Apache-2.0/BSD-3-Clause/BSL-1.0/ISC/Unicode-3.0; see `deny.toml`).

**RSS benchmark** (`make bench` / `./bench/bench.sh`): spins up a `kind` cluster, generates N namespaces × Deployment/StatefulSet/ConfigMap/Service into a local Git repo, runs a release build of leancd against it, scrapes the `leancd_rss_bytes` Prometheus metric, and fails if RSS ≥ the budget. Tunables: `BENCH_NAMESPACE_COUNT` (default 15), `RSS_BUDGET_MIB` (default 100), `KIND_CLUSTER_NAME`.

## Architecture

### Single binary, three subcommands, one shared engine

`clap` (derive) defines `controller`, `sync`, and `status` (`cli.rs`). `controller` (long-lived, the `Deployment`) and `sync` (one pass, optional `--force`) are dispatched to **the same `Reconciler`** (`reconcile.rs`) — `run_loop()` just calls `run_once(false)` on a poll interval, `sync` calls `run_once(force)` once and exits. This guarantees manual and automatic sync use identical apply logic. `status` is read-only (reads the state ConfigMap).

### Reconciliation flow (`reconcile.rs::reconcile`)

1. Read prior state from the state ConfigMap (`state.rs`) — previous HEAD SHA + previously-applied resource keys.
2. `git_sync::sync` — shallow `fetch`/`clone` (`--depth 1`), compare HEAD SHA → `changed` bool. Short-circuits heavy work when nothing moved.
3. `manifest::parse_dir` — streaming, per-document YAML parse into untyped `RawManifest`s; inject the managed-by label into each.
4. **Full apply** when `force || first run || sha changed`; otherwise **drift-check** (`drift::detect`) and re-apply only if drift is found.
5. `prune::prune` — delete keys in the prior applied set that are absent from the current Git set.
6. Write updated state (new SHA, applied keys, counts) back to the ConfigMap; update Prometheus gauges.

### Memory strategy — do not violate these (see design §4)

- **No kube-rs informer/cache.** `kube_util.rs` never builds a `Controller` or `Store`. Every interaction is a direct `List`/`Get`/`Patch` on `DynamicObject` and returns immediately.
- **Drift via periodic `List`, never `Watch`.** One `List` per managed GVK (filtered by the managed-by label), then a recursive subset check (`spec_subset`) that tolerates server-injected defaults.
- **`git` via shell-out** (`tokio::process::Command`), chosen deliberately over a Git library: git runs as a separate process so its memory is **not** counted in leancd's RSS. The design doc's references to `gix`/`spawn_blocking` are historical; the implementation uses `tokio::process`.
- **Streaming YAML parse** (`serde_yaml::Deserializer`) — one document at a time.
- **State is one ConfigMap + a label**, not a CRD or DB. Process memory holds only: last SHA, lightweight API-discovery metadata (GVK → `ApiResource`, no resource bodies), and transient pass state.
- **`tokio` `current_thread` runtime** (`#[tokio::main(flavor = "current_thread")]`) to minimize thread/stack memory.

### Module map

| Module | Responsibility |
|---|---|
| `cli.rs` | clap subcommands + shared `CommonArgs` (flags map to `LEANCD_*` env vars) → `Config` |
| `config.rs` | validated `Config`; git-credential resolution from env; URL encoding |
| `git_sync.rs` | shallow fetch/clone + HEAD-SHA change detection via the `git` CLI |
| `manifest.rs` | streaming multi-doc YAML parse; GVK/ns/name extraction; managed-label injection |
| `kube_util.rs` | API discovery (`pinned_kind`), dynamic `Api` construction (cluster vs namespaced), SSA `apply`, `list`, `delete` |
| `reconcile.rs` | the `Reconciler` engine shared by `controller`/`sync` |
| `prune.rs` | set-diff deletion of resources removed from Git (`ResourceKey` identity) |
| `drift.rs` | per-GVK `List` + subset comparison |
| `state.rs` | single ConfigMap persistence (`State` ↔ `BTreeMap<String,String>`) |
| `metrics.rs` | Prometheus `/metrics` over a minimal `tokio::net` listener; exposes `leancd_rss_bytes` |

## Implementation notes worth remembering

- **`serde_yaml` is intentional, despite deprecation.** It supports the streaming `Deserializer` that `manifest.rs` needs; `serde_yml` did not. `manifest.rs` carries `#![allow(deprecated)]` on purpose. kube-rs also depends on `serde_yaml`. See design 付録 B.
- **kube-rs v3 discovery API**: `kube::discovery::pinned_kind(client, &gvk)` returns `(ApiResource, ApiCapabilities)`; use `caps.scope` (`Scope::Cluster` vs namespaced) to pick `Api::all_with` vs `Api::namespaced_with`. SSA is `api.patch(name, &PatchParams::apply(fm).force(), &Patch::Apply(&obj))`.
- **TLS is rustls** (`features = ["rustls-tls"]`) — OpenSSL is intentionally avoided.
- **Managed-by label** (`app.kubernetes.io/managed-by=leancd` by default) is injected at apply time on every resource. Pruning uses the persisted applied-set as the primary signal and the label as the safety net.
- **Individual apply failures are logged, not fatal** (`apply_all` continues on error); a hard failure is only surfaced for git/state/discovery-stopping errors.

## Conventions when editing

- Match the existing style: module-level `//!` doc comments, `tracing` structured logs (`tracing::warn!`/`info!` with `error = %e`), `crate::error::{Error, Result}` (thiserror enum) rather than `anyhow` in library code (`anyhow` is only at `main`'s top level).
- Keep new dependencies minimal — remember `cargo-deny` allows MIT only. Evaluate any added dependency against the RSS budget first.
- When changing kube interaction code, confirm it issues direct API calls and builds no cache.
