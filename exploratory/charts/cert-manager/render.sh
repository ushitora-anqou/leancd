#!/usr/bin/env bash
# Render the cert-manager Helm chart into the shared compare repo workdir as a
# second comparison workload — a real-world chart distinct from VictoriaMetrics
# (CRDs + validating/mutating webhook Deployments + pre-install/post-install Job
# hooks), which exercises hook-weight / hook-delete-policy handling against a
# different chart shape. The generic render flow lives in lib/chart.sh.
#
# The chart version is not pinned to a literal here: run.sh resolves the latest
# stable version via `helm search repo` so the comparison does not break when a
# new cert-manager minor is cut. Override with CM_CHART_VER to pin.
set -euo pipefail
source "$(dirname "$0")/../../lib/common.sh"
source "$(dirname "$0")/../../lib/git.sh"
source "$(dirname "$0")/../../lib/chart.sh"

CM_REPO="${CM_REPO:-https://charts.jetstack.io}"
CM_CHART="${CM_CHART:-cert-manager}"
CM_RELEASE="${CM_RELEASE:-cm}"

init_repo
log "ensuring helm repo 'jetstack' ..."
helm repo add jetstack "$CM_REPO" >/dev/null 2>&1 || true
helm repo update >/dev/null 2>&1 || true
if [[ -z "${CM_CHART_VER:-}" ]]; then
    CM_CHART_VER="$(helm search repo "jetstack/$CM_CHART" --versions -o json 2>/dev/null \
        | jq -r '.[0].version' 2>/dev/null || true)"
    [[ -z "$CM_CHART_VER" ]] && CM_CHART_VER="v1.16.0"
fi

VALUES_FILE="$(cd "$(dirname "$0")" && pwd)/values.yaml"
DEST="${CM_RENDER_DEST:-$REPO_WORKDIR/cert-manager.yaml}"

# jetstack repo was already added above; pass an empty repo_url to render_chart
# (it re-adds idempotently) — the chart ref is "jetstack/$CM_CHART".
render_chart jetstack "$CM_REPO" "$CM_CHART" "$CM_CHART_VER" "$CM_RELEASE" "$VALUES_FILE" "$DEST"
