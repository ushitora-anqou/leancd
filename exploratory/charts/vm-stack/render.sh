#!/usr/bin/env bash
# Render the VictoriaMetrics K8s Stack Helm chart into the shared compare repo
# workdir as a single multi-doc YAML (CRDs first, then the rendered CRs/services),
# and report doc/CRD counts + largest doc size.
#
# The generic helm-show-crds + helm-template flow lives in lib/chart.sh; this
# file holds only the VictoriaMetrics-specific repo/chart/version pins.
set -euo pipefail
source "$(dirname "$0")/../../lib/common.sh"
source "$(dirname "$0")/../../lib/git.sh"
source "$(dirname "$0")/../../lib/chart.sh"

VM_REPO="${VM_REPO:-https://victoriametrics.github.io/helm-charts/}"
VM_CHART="${VM_CHART:-victoria-metrics-k8s-stack}"
VM_RELEASE="${VM_RELEASE:-vmks}"
# Pinned for report reproducibility (latest stable at the time of writing).
VM_CHART_VER="${VM_CHART_VER:-0.84.0}"
VALUES_FILE="$(cd "$(dirname "$0")" && pwd)/values.yaml"
DEST="${VM_RENDER_DEST:-$REPO_WORKDIR/vm-stack.yaml}"

render_chart vm "$VM_REPO" "$VM_CHART" "$VM_CHART_VER" "$VM_RELEASE" "$VALUES_FILE" "$DEST"
