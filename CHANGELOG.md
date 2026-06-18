# Changelog

All notable changes to leancd are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — production readiness

- **Backoff on failure**: the controller now backs off exponentially
  (`--backoff-base`/`--backoff-max`) on consecutive sync failures, capped,
  jittered (`[0.75x, 1.0x)` to avoid synchronisation), and reset to the poll
  interval on success.
- **Graceful shutdown**: `--shutdown-timeout` makes the controller finish the
  in-flight reconciliation pass on SIGTERM, falling back to abort after the
  grace period (no more mid-pass abort).
- **`leancd health`**: a new subcommand for liveness/readiness `exec` probes
  (`--health-stale-factor`); exit 0 = fresh, 1 = never synced, 2 = stale,
  3 = failing.
- **Build version metadata**: the short git SHA is embedded at build time;
  `leancd --version` and the startup log report `0.1.0 (sha …)`.
- **Runtime log reload**: `SIGHUP` re-reads `RUST_LOG` so the level can change
  without a redeploy.
- **Hardened Deployment**: Pod Security Standards `restricted` (non-root,
  read-only root FS, dropped capabilities, seccomp) with a `/tmp` `emptyDir`,
  plus `livenessProbe`/`readinessProbe` using `leancd health`.
- **`deploy/leancd-namespaced.yaml`**: namespace-scoped RBAC template and a
  `NetworkPolicy` (egress to kube-API/Git/OTLP/kube-dns, no ingress).
- **CI/CD**: GitHub Actions CI (`fmt`/`clippy -D`/test/`cargo-deny`/
  `cargo-audit`) and a multi-arch (`amd64`+`arm64`) GHCR release workflow on
  `v*` tags.
- **Metrics docs**: Prometheus ingestion guidance (Prometheus ≥ 3.0 native OTLP
  / OTel Collector) added to the user manual.

## [0.1.0] - 2026-06-13

Initial public baseline: Git-to-cluster sync, server-side apply, drift
detection, pruning, Helm hooks (Argo CD-equivalent phases), OTLP/HTTP metrics,
and the RSS ≤ 100MiB benchmark.
