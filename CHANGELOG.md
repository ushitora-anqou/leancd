# Changelog

All notable changes to Lean CD are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Cluster-side drift detection via watch** (`--watch-mode`): Lean CD now
  watches its managed-by resources so a cluster-side edit (`kubectl`, another
  controller) wakes the reconcile loop within `--watch-debounce` instead of
  waiting up to `--poll-interval`. Three modes — `off` (poll only, the previous
  behavior), `trigger` (a `watcher` per managed GVK pokes the loop; drift is
  still checked via `List`), and `cache` (default; a `watcher` + reflector
  `Store` per GVK, drift read from the `Store` with no per-pass `List`). `cache`
  was chosen as default: measured (`bench/`) to match `trigger` on RSS (≈16 MiB
  self at 15 ns × 18 resources) while removing the per-pass apiserver `List`
  load. A watch-triggered reconcile goes through the identical
  `run_once → reconcile → lock::acquire` path, so the Lease serialization is
  unchanged.
- **Cache-bloat benchmark** (`bench/cache-bloat.sh`): stresses the watch `Store`
  along the two axes that grow it — object count (`scale`) and per-object size
  (`large-obj`) — plus a create/delete `churn` leak check (idle RSS must not
  climb across cycles). All gated against `RSS_BUDGET_MIB`;
  `bench/gen-manifests.sh` shapes the manifest set for either axis.
- **Resource health assessment** (Argo CD-style): Lean CD now evaluates the
  health of its managed resources and exposes it as an *independent* signal —
  sync completion is unchanged (a successful apply still completes a sync). It
  is a port of Argo CD's built-in per-GVK health checks (`Deployment`,
  `StatefulSet`, `ReplicaSet`, `DaemonSet`, `Pod`, `Job`, `Service`, `Ingress`,
  `PVC`, `HPA`, `APIService`, `Workflow`); like Argo CD it does **not** descend
  `ownerReferences` (a Deployment's health reads its own `.status`, which
  already aggregates its ReplicaSet/Pod state). The worst status across
  evaluated resources is persisted in the state ConfigMap, exported as the
  `leancd_health_status` gauge (by `status`), and shown by `leancd status` /
  `leancd health`. Live objects are reused from the drift `List`/watch cache —
  no new resident cache.
- **`--health-mode`** (`on`/`off`, default `on`; `LEANCD_HEALTH_MODE`): toggles
  the health assessment. `off` skips it and its metric (sync completion is
  unaffected either way).
- **Health-heavy benchmark** (`bench/health-heavy.sh`): stresses health
  assessment at a larger Deployment→ReplicaSet→Pod fan-out and namespace count
  than the default bench (`HEALTH_HEAVY_NS`, `HEALTH_HEAVY_REPLICAS`), health
  mode `on`, gated against `RSS_BUDGET_MIB`. `bench/gen-manifests.sh` gains
  `BENCH_DEP_REPLICAS` to scale the Pod fan-out per Deployment.

### Changed

- **Migrated to Rust edition 2024**: `Cargo.toml` now sets `edition = "2024"`
  with `rust-version = "1.85"`. Edition 2024 breaking changes are handled
  conservatively with no production-code behavior change: test-only
  `std::env::set_var`/`std::env::remove_var` calls are wrapped in `unsafe`
  blocks (these became `unsafe` functions in edition 2024), and a test
  helper's `gen` parameter was renamed to `generation` (`gen` is now a
  reserved keyword). The project uses no `unsafe`, `extern`, RPIT, or
  `static mut` references, so the other edition 2024 breaking changes do not
  apply.
- **RSS budget tightened to 50 MiB** (was 100). The headline gate enforced by
  `bench/bench.sh` is now `RSS_BUDGET_MIB=50` (default); `bench/scale.sh`
  continues to forward 100. Lean CD measures ≈16 MiB self RSS at the default
  scale, so 50 MiB keeps ample headroom while sharpening the regression gate.
- **Dependencies updated**: `kube` 3.1 → 4.0 with `k8s-openapi` 0.27 → 0.28
  (Kubernetes `v1_36`); the OpenTelemetry stack (`opentelemetry`,
  `opentelemetry-otlp`, `opentelemetry_sdk`) 0.28 → 0.32 (the Metrics SDK
  stabilized in 0.30); and `rand` 0.8 → 0.10. `serde_yaml` 0.9.34 is retained
  deliberately — the streaming `Deserializer` it provides is still required and
  `serde_yml` has no equivalent. The OTel metric
  unit tests moved off the now feature-gated `ManualReader`/`MetricReader`
  plumbing onto the SDK's supported `InMemoryMetricExporter` (`testing`
  feature, a dev-dependency so resolver 2 keeps it out of the release binary).
  All semver-compatible transitive deps were refreshed via `cargo update`.
  nix flake inputs are intentionally left untouched. RSS stays ≈19 MiB
  self/tree, well under the 50 MiB budget (`make bench`).

## [0.1.1] - 2026-06-22

### Added — production readiness

- **Backoff on failure**: the controller now backs off exponentially
  (`--backoff-base`/`--backoff-max`) on consecutive sync failures, capped,
  jittered (`[0.75x, 1.0x)` to avoid synchronization), and reset to the poll
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
  Lean CD's permissions to the namespace only (RoleBinding) and ships a
  `NetworkPolicy` (egress to kube-API/Git/OTLP/kube-dns, no ingress).
- **CI/CD**: GitHub Actions CI (`fmt`/`clippy -D`/test/`cargo-deny`/
  `cargo-audit`) and a multi-arch (`amd64`+`arm64`) GHCR release workflow on
  `v*` tags.
- **Metrics docs**: Prometheus ingestion guidance (Prometheus ≥ 3.0 native OTLP
  / OTel Collector) added to the user manual.

### Changed — packaging

- **Helm chart**: Lean CD now ships as a Helm chart at `charts/leancd/`, replacing
  the static `deploy/leancd.yaml` and `deploy/leancd-namespaced.yaml` manifests.
  The chart reproduces the cluster-scoped Deployment, RBAC, probes, and
  PSS-restricted securityContext, adds a `rbac.namespaced` toggle (the former
  namespaced mode + NetworkPolicy) and an optional Grafana dashboard ConfigMap
  (`dashboards.enabled`, on by default, labeled `grafana_dashboard: "1"` for
  kiwigrid sidecar autodiscovery). Migrate with `helm install leancd charts/leancd`.
- **CI**: `nix flake check` now also runs `helm lint` and `helm template` structure
  tests across the value variations; `make e2e` gained a
  `helm_install_deploys_controller` scenario; the dev shell now provides `helm`.

### Added — release & distribution

- **OCI chart publishing**: the Helm chart is now published to GHCR as an OCI
  artifact (`oci://ghcr.io/ushitora-anqou/charts/leancd`) on every `v*` tag, so
  it installs with `helm install leancd oci://... --version X.Y.Z` — no
  `helm repo add`. The `chart` workflow job packages and pushes it (with a
  `helm pull` guard so re-runs of a failed job are idempotent).
- **Automated GitHub Release**: the `v*` tag now creates the GitHub Release
  automatically — notes extracted from the `CHANGELOG.md` section for the
  version (falling back to auto-generated notes), with the chart `.tgz` attached.
- **Chart-version-consistency check**: `nix flake check` now asserts
  `Chart.yaml`'s `version`/`appVersion` match `Cargo.toml`'s version, catching a
  half-bumped release before tag push.

### Changed — release & distribution

- **Chart default image**: `image.repository` now defaults to
  `ghcr.io/ushitora-anqou/leancd` (was the local `leancd`), and the Deployment's
  image tag resolves to `Chart.appVersion`
  (`{{ .Values.image.tag | default .Chart.AppVersion }}`), so the published
  build installs without an `--set image.*` override. Setting `image.tag` still
  pins a specific version.
- **Release workflow**: `release.yml` gained `chart` and `release` jobs (image
  and chart publish in parallel, then the GitHub Release) and was elevated to
  `contents: write` so it can create the release.

### Notes

- **BUG 8 (VictoriaMetrics dashboard ConfigMap annotations) reclassified as not
  a bug**: the annotation delta vs Argo CD is Argo CD's injected
  `argocd.argoproj.io/tracking-id`, never present in the source manifest. A
  regression test (`apply_round_trip_preserves_metadata_annotations`) pins that
  `metadata.annotations` and `data` survive Lean CD's SSA patch-body round-trip.

### Fixed

- **BUG 9 (drift false-positive on k8s zero-value fields)**: `drift::spec_subset`
  now treats k8s zero-value fields (boolean `false`, `null`, empty `[]`, and
  number `0`) as equivalent to the field being absent in live, and
  `specs_differ` strips Secret `stringData` (k8s converts it to base64 `data` on
  apply). Previously the VictoriaMetrics K8s Stack re-applied 3 resources every
  pass (KSM Deployment `hostNetwork: false`; node-exporter DaemonSet
  `livenessProbe.httpGet.httpHeaders: null` / `initialDelaySeconds: 0`;
  vmalertmanager Secret `stringData`) — `drift_count` now stays 0.

### Changed — release tooling

- **amd64-only image**: the release workflow no longer builds `linux/arm64`
  (QEMU emulation added ~50 min per release); images are now `linux/amd64`
  only, cutting the build-and-push job from ~52 min to a few minutes.
- **GitHub Actions on Node 24**: bumped `actions/checkout`, `upload-artifact`,
  `download-artifact`, `docker/setup-buildx-action`, `docker/login-action`,
  `docker/build-push-action`, and `azure/setup-helm` to their current majors,
  clearing the Node.js 20 deprecation warnings.
- **One-command release (`make release`)**: bumps the patch version across
  `Cargo.toml` + `Chart.yaml`, moves the CHANGELOG `[Unreleased]` section under
  a dated `[X.Y.Z]` heading, runs the full local gate, then commits, tags, and
  pushes — triggering `release.yml` end to end. `RELEASE_DRYRUN=1 make
  release` previews the bump without pushing.
- **Version-agnostic chart template check**: the `nix flake check`
  `helm-template` assertion now reads `Chart.appVersion` dynamically instead of
  hard-coding `0.1.0`, so it survives the version bump `make release` performs.

### Fixed — release

- **Chart artifact upload**: the `chart` job uploaded the Helm tarball under the
  literal path `leancd-$V.tgz` (`$V` is not expanded inside `with:`), so no
  artifact was published and the `release` job failed with "Artifact not found"
  on the v0.1.0 tag. The path now uses `leancd-${{ env.V }}.tgz`.

## [0.1.0] - 2026-06-13

Initial public baseline: Git-to-cluster sync, server-side apply, drift
detection, pruning, Helm hooks (Argo CD-equivalent phases), OTLP/HTTP metrics,
and the RSS ≤ 100MiB benchmark.
