# leancd RSS benchmark

This directory verifies the headline guarantee: leancd keeps its RSS minimal
while reconciling a realistic cluster — both at the sync peak
(fetch/parse/apply) and at idle.

## What it does

1. Spins up a `kind` cluster (the simulated Kubernetes cluster).
2. Generates a realistic manifest set (N namespaces ×
   Deployment/StatefulSet/ConfigMap/Service + cluster-scoped resources) into a
   local Git repository.
3. Builds leancd in **release** mode and runs it as a controller pointed at the
   kind cluster via kubeconfig.
4. Samples two footprints in parallel from startup through the settled state,
   capturing the **peak** (max) and **idle** (final) value of each:
   - **self** — leancd's own RSS, read directly from the process via `ps`.
   - **tree** — the whole process tree (leancd + git/ssh subprocesses), summed
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
| `RSS_BUDGET_MIB` | 100 | RSS budget in MiB |
| `BENCH_SAMPLE_SECS` | 30 | seconds to sample RSS for peak detection |
| `KIND_CLUSTER_NAME` | leancd-bench | kind cluster name |

## Scale sweep

To track how the footprint scales with the namespace count:

```sh
make scale        # or: ./bench/scale.sh
```

`scale.sh` runs `bench.sh` at each level (default `8 15 20` namespaces, via
`SCALE_NS_LEVELS`) and prints a table of peak/idle RSS. It exits non-zero if any
level breaches the budget.

## CI integration

These benchmarks need `kind`/Docker, so they are **not** part of
`nix flake check` (which runs in a sandbox). `make test` covers the static gate
(fmt/clippy/nextest/deny/audit) only. Run `make bench` / `make scale` manually
or in an external CI job that has Docker — both scripts exit non-zero on a
budget breach, so wiring them into such a job catches RSS regressions.

## Measurement note

leancd runs as a host process against the kind cluster's kubeconfig rather than
in-cluster. This exercises the exact same reconciliation code paths and memory
profile; the in-cluster Deployment variant lives in `deploy/`.

The **tree** measurement sums RSS across leancd and every descendant process it
spawns (the `git` CLI for fetch/clone/reset, plus any `ssh` it shells out to).
Because RSS double-counts pages shared between processes, the tree total is an
overestimate — deliberately conservative. This verifies that git's memory is
accounted for too, even though git runs as a separate process and is excluded
from leancd's own RSS (read via `ps`).
