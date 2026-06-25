# Lean CD RSS benchmark

This directory verifies the headline guarantee: Lean CD keeps its RSS minimal
while reconciling a realistic cluster — both at the sync peak
(fetch/parse/apply) and at idle.

## What it does

1. Spins up a `kind` cluster (the simulated Kubernetes cluster).
2. Generates a realistic manifest set (N namespaces ×
   Deployment/StatefulSet/ConfigMap/Service + cluster-scoped resources) into a
   local Git repository.
3. Builds Lean CD in **release** mode and runs it as a controller pointed at the
   kind cluster via kubeconfig.
4. Samples two footprints in parallel from startup through the settled state,
   capturing the **peak** (max) and **idle** (final) value of each:
   - **self** — Lean CD's own RSS, read directly from the process via `ps`.
   - **tree** — the whole process tree (Lean CD + git/ssh subprocesses), summed
     via `ps`. Shared pages are double-counted, so this deliberately
     overestimates (a conservative regression gate).
5. Fails if any of the self/tree peak/idle values >= the budget (default
   `RSS_BUDGET_MIB`); every sampled point must stay under it.

## Prerequisites

- [kind](https://kind.sigs.k8s.io/), `kubectl`, `git`
- Rust toolchain (`cargo`)

## Running

```sh
make bench        # or: ./bench/bench.sh
```

Tunable via environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `BENCH_NAMESPACE_COUNT` | 15 | namespaces generated (×18 resources each) |
| `RSS_BUDGET_MIB` | 50 | RSS budget in MiB |
| `BENCH_SAMPLE_SECS` | 30 | seconds to sample RSS for peak detection |
| `KIND_CLUSTER_NAME` | leancd-bench | kind cluster name |

### Watch modes

The bench inherits `LEANCD_WATCH_MODE` from the environment (the `controller`
binary default is `cache`). To compare the watch modes against the RSS budget:

```sh
LEANCD_WATCH_MODE=off     RSS_BUDGET_MIB=50 make bench   # poll-only baseline
LEANCD_WATCH_MODE=trigger RSS_BUDGET_MIB=50 make bench   # watch trigger, List drift-check
LEANCD_WATCH_MODE=cache   RSS_BUDGET_MIB=50 make bench   # watch + Store drift-check (default)
```

At 15 namespaces × 18 resources the three measure ≈13 / 16 / 16 MiB self RSS
respectively — all well under 50 MiB. The bench does not inject mid-run drift,
so it captures the **steady-state** watch cost (open watch streams, idle); the
*latency* improvement (sub-`poll_interval` self-heal) is validated in the e2e
suite (`tests/e2e.rs::watch_self_heal_fast`), not here.

## Scale sweep

To track how the footprint scales with the namespace count:

```sh
make scale        # or: ./bench/scale.sh
```

`scale.sh` runs `bench.sh` at each level (default `8 15 20` namespaces, via
`SCALE_NS_LEVELS`) and prints a table of peak/idle RSS. It exits non-zero if any
level breaches the budget.

## Cache-bloat scenarios

`make bench` / `make scale` measure cache mode only at the default scale (15 ns
× 18 resources, 200 B ConfigMap payload), where the watch `Store` is small. The
`Store` grows with both the **object count** and the **per-object size**, and a
correctness question is whether repeated create/delete (churn) makes it
**accumulate** (leak). `cache-bloat.sh` stresses each axis on purpose and gates
the result against `RSS_BUDGET_MIB` (default 50):

```sh
./bench/cache-bloat.sh
```

| Scenario | What it stresses | How |
|---|---|---|
| `scale` | object **count** | many namespaces (default `CACHE_BLOAT_NS=40`) × default per-ns resources |
| `large-obj` | per-object **size** | large ConfigMap payload (default `CACHE_BLOAT_PAYLOAD=51200` = 50 KiB) |
| `single-large-file` | **single-file** parse | same large payload but every manifest merged into one multi-doc YAML (`BENCH_MERGE_TO_SINGLE_FILE=1`), exercising one large parse stream vs many small files |
| `churn` | create/delete **leak** | drives leancd directly and adds/removes a ConfigMap each cycle (default `CACHE_BLOAT_CHURNS=20`); idle RSS must not climb across cycles |

All three run in `--watch-mode=cache`. `scale` and `large-obj` reuse `bench.sh`
with env overrides; `churn` is self-contained (it mutates HEAD during sampling,
which `bench.sh` does not). The script prints a table of self/tree peak/idle RSS
per scenario and exits non-zero if any value breaches the budget.

Tunables (all optional): `CACHE_BLOAT_NS`, `CACHE_BLOAT_PAYLOAD`,
`CACHE_BLOAT_CHURNS`, `RSS_BUDGET_MIB`. `gen-manifests.sh` honors
`BENCH_PAYLOAD_BYTES` and `BENCH_{DEP,STS,CM,SVC}_PER_NS` so the manifest set can
be shaped for either axis.

## CI integration

These benchmarks need `kind`/Docker, so they are **not** part of
`nix flake check` (which runs in a sandbox). `make test` covers the static gate
(fmt/clippy/nextest/deny/audit) only. Run `make bench` / `make scale` manually
or in an external CI job that has Docker — both scripts exit non-zero on a
budget breach, so wiring them into such a job catches RSS regressions.

## Measurement note

Lean CD runs as a host process against the kind cluster's kubeconfig rather than
in-cluster. This exercises the exact same reconciliation code paths and memory
profile; the in-cluster Deployment ships in the Helm chart ([`../charts/leancd/`](../charts/leancd/)).

The **tree** measurement sums RSS across Lean CD and every descendant process it
spawns (the `git` CLI for fetch/clone/reset, plus any `ssh` it shells out to).
Because RSS double-counts pages shared between processes, the tree total is an
overestimate — deliberately conservative. This verifies that git's memory is
accounted for too, even though git runs as a separate process and is excluded
from Lean CD's own RSS (read via `ps`).
