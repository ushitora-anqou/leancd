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
- **Graceful shutdown**: `--shutdown-timeout-secs` makes the controller finish the
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
- **Namespaced RBAC posture**: the chart's `rbac.namespaced=true` mode binds
  leancd's permissions to the namespace only (RoleBinding) and ships a
  `NetworkPolicy` (egress to kube-API/Git/OTLP/kube-dns, no ingress).
- **CI/CD**: GitHub Actions CI (`fmt`/`clippy -D`/test/`cargo-deny`/
  `cargo-audit`) and a multi-arch (`amd64`+`arm64`) GHCR release workflow on
  `v*` tags.
- **Metrics docs**: Prometheus ingestion guidance (Prometheus ≥ 3.0 native OTLP
  / OTel Collector) added to the user manual.

### Changed — packaging

- **Helm chart**: leancd now ships as a Helm chart at `charts/leancd/`, replacing
  the static `deploy/leancd.yaml` and `deploy/leancd-namespaced.yaml` manifests.
  The chart reproduces the cluster-scoped Deployment, RBAC, probes, and
  PSS-restricted securityContext, adds a `rbac.namespaced` toggle (the former
  namespaced mode + NetworkPolicy) and an optional Grafana dashboard ConfigMap
  (`dashboards.enabled`, on by default, labelled `grafana_dashboard: "1"` for
  kiwigrid sidecar autodiscovery). Migrate with `helm install leancd charts/leancd`.
- **CI**: `nix flake check` now also runs `helm lint` and `helm template` structure
  tests across the value variations; `make e2e` gained a
  `helm_install_deploys_controller` scenario; the dev shell now provides `helm`.

### Notes

- **BUG 8 (VictoriaMetrics dashboard ConfigMap annotations) reclassified as not
  a bug**: the annotation delta vs Argo CD is Argo CD's injected
  `argocd.argoproj.io/tracking-id`, never present in the source manifest. A
  regression test (`apply_round_trip_preserves_metadata_annotations`) pins that
  `metadata.annotations` and `data` survive leancd's SSA patch-body round-trip.

### Fixed

- **BUG 9 (drift false-positive on k8s zero-value fields)**: `drift::spec_subset`
  now treats k8s zero-value fields (boolean `false`, `null`, empty `[]`, and
  number `0`) as equivalent to the field being absent in live, and
  `specs_differ` strips Secret `stringData` (k8s converts it to base64 `data` on
  apply). Previously the VictoriaMetrics K8s Stack re-applied 3 resources every
  pass (KSM Deployment `hostNetwork: false`; node-exporter DaemonSet
  `livenessProbe.httpGet.httpHeaders: null` / `initialDelaySeconds: 0`;
  vmalertmanager Secret `stringData`) — `drift_count` now stays 0.

## [0.1.0] - 2026-06-13

Initial public baseline: Git-to-cluster sync, server-side apply, drift
detection, pruning, Helm hooks (Argo CD-equivalent phases), OTLP/HTTP metrics,
and the RSS ≤ 100MiB benchmark.
