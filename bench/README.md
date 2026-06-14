# leancd RSS benchmark

This directory verifies the headline guarantee: leancd keeps its RSS under
**100MiB** while reconciling a realistic cluster — both at the sync peak
(fetch/parse/apply) and at idle.

## What it does

1. Spins up a `kind` cluster (the simulated Kubernetes cluster).
2. Generates a configurable set of manifests (ConfigMaps + cluster-scoped
   resources) into a local Git repository.
3. Builds leancd in **release** mode and runs it as a controller pointed at the
   kind cluster via kubeconfig.
4. Samples the `leancd_rss_bytes` Prometheus metric from startup through the
   settled state, capturing the **peak** (max) and **idle** (final) RSS.
5. Fails if either value >= the budget (default 100MiB). Design §8.2 requires
   both points to stay under the budget.

## Prerequisites

- [kind](https://kind.sigs.k8s.io/), `kubectl`, `git`, `curl`
- Rust toolchain (`cargo`)

## Running

```sh
make bench        # or: ./bench/bench.sh
```

Tunable via environment variables:

| Variable | Default | Meaning |
|---|---|---|
| `BENCH_RESOURCE_COUNT` | 200 | number of manifests generated |
| `RSS_BUDGET_MIB` | 100 | RSS budget in MiB |
| `BENCH_SAMPLE_SECS` | 30 | seconds to sample RSS for peak detection |
| `KIND_CLUSTER_NAME` | leancd-bench | kind cluster name |

## Scale sweep

To track how the footprint scales with the managed resource count (design §8.3):

```sh
make scale        # or: ./bench/scale.sh
```

`scale.sh` runs `bench.sh` at each level (default `100 300 500`, via
`SCALE_LEVELS`) and prints a table of peak/idle RSS. It exits non-zero if any
level breaches the budget.

## CI integration

These benchmarks need `kind`/Docker, so they are **not** part of
`nix flake check` (which runs in a sandbox). `make test` covers the static gate
(fmt/clippy/nextest/deny/audit) only. Run `make bench` / `make scale` manually
or in an external CI job that has Docker — both scripts exit non-zero on a
budget breach, so wiring them into such a job catches RSS regressions (design
§8.4).

## Measurement note

leancd runs as a host process against the kind cluster's kubeconfig rather than
in-cluster. This exercises the exact same reconciliation code paths and memory
profile; the in-cluster Deployment variant lives in `deploy/`.
