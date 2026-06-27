# Lean CD Helm Chart

A minimal, low-memory Kubernetes continuous-delivery controller that syncs plain
YAML manifests from a Git repository into the cluster — like Argo CD / Flux CD,
but far smaller. See the project [README](../../README.md) for what Lean CD does.

## Install

The published chart is on GHCR as an OCI artifact — install it directly (OCI
needs no `helm repo add`):

```sh
# 1. Namespace + Git credentials (omit the Secret for a public repo).
kubectl create namespace leancd
kubectl -n leancd create secret generic leancd-git-credentials \
  --from-literal=GIT_USERNAME=<user> --from-literal=GIT_PASSWORD=<token>
# (For SSH: --from-file=GIT_SSH_KEY=$HOME/.ssh/id_ed25519)

# 2. Install the chart. The default image resolves to Chart.appVersion, so no
#    image.* override is needed for the published build.
helm install leancd oci://ghcr.io/ushitora-anqou/charts/leancd \
  --version X.Y.Z \
  --namespace leancd --create-namespace \
  --set config.repoUrl=https://github.com/example/manifests.git

kubectl -n leancd wait --for=condition=Available deploy/leancd --timeout=240s
```

To install from a local checkout instead (e.g. development), point `helm` at the
chart directory:

```sh
helm install leancd ./charts/leancd \
  --namespace leancd --create-namespace \
  --set config.repoUrl=https://github.com/example/manifests.git
```

## RBAC posture

- **default** (`rbac.namespaced=false`): a `ClusterRole` bound cluster-wide via
  a `ClusterRoleBinding`. Lean CD can apply arbitrary kinds (including CRDs and
  cluster-scoped resources) wherever they land.
- **namespaced** (`--set rbac.namespaced=true`): the same `ClusterRole` bound to
  the Lean CD namespace only (a `RoleBinding`), plus a default-deny `NetworkPolicy`
  allowing egress solely to kube-dns, the API server, the Git host, and the OTLP
  collector. Tighten `networkPolicy.kubeApiCidr` / `networkPolicy.egressCidr` to
  your environment.

## Grafana dashboard

`dashboards.enabled=true` (the default) ships the overview dashboard as a
ConfigMap labeled `grafana_dashboard: "1"`. A Grafana running the kiwigrid
dashboard sidecar (e.g. the VictoriaMetrics or `grafana` helm charts with sidecar
dashboards enabled) imports it automatically. Lean CD itself runs no HTTP
listener — metrics reach Grafana via the OTLP collector set with
`metrics.otlpEndpoint`. Disable with `--set dashboards.enabled=false`.

## Key values

| Key | Default | Description |
|---|---|---|
| `image.repository` / `image.tag` | `ghcr.io/ushitora-anqou/leancd` / `Chart.appVersion` | Container image; `tag` defaults to `Chart.appVersion`, override to pin a version |
| `config.repoUrl` | `https://github.com/example/manifests.git` | Git repository to sync (override me) |
| `config.branch` / `config.path` | `main` / `.` | Branch and path globs to sync |
| `config.pollInterval` | `60s` | Reconcile poll interval |
| `config.cacheMaxObjectBytes` | `12288` | In `cache` watch mode, max serialized bytes of an object cached in full; larger → key-only + per-GVK List drift fallback. Size-based, any kind |
| `metrics.otlpEndpoint` | `http://otel-collector:4318` | OTLP/HTTP collector endpoint (bring your own collector) |
| `rbac.namespaced` | `false` | Bind permissions to the namespace only (+ NetworkPolicy) |
| `dashboards.enabled` | `true` | Ship the Grafana dashboard ConfigMap |
| `git.credentialsSecretName` | `leancd-git-credentials` | Secret with GIT_USERNAME/GIT_PASSWORD or GIT_SSH_KEY |
| `extraEnv` | `[]` | Extra env appended after the built-ins (last wins, so any `LEANCD_*`/`OTEL_*` can be overridden) |
| `resources` | limits 128Mi/200m, requests 32Mi/50m | RSS stays minimal; the limit leaves headroom (see `bench/`) |
| `podDisruptionBudget.enabled` / `.minAvailable` | `true` / `1` | PDB protecting the single replica from voluntary eviction (node drains) |
| `priorityClass.enabled` / `.value` | `false` / `1000000` | Opt-in high PriorityClass — the controller is among the last evicted under node pressure |
| `networkPolicy.enabled` | `true` | Generate the egress-restricting NetworkPolicy in **both** RBAC modes |
| `startupProbe` | (see values) | `leancd health` startup probe; a slow first clone/discovery is not killed by liveness |
| `image.pullSecrets` | `[]` | Pull secrets for a private / air-gapped registry |

Resource names are fixed (`leancd`, `leancd-grafana-dashboard`, …) and do not
track the release name — Lean CD is a cluster-scoped singleton. To run more than
one release, install into separate namespaces.
