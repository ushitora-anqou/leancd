#!/usr/bin/env bash
# Render the VictoriaMetrics K8s Stack Helm chart into the shared compare repo
# workdir as a single multi-doc YAML (CRDs first, then the rendered CRs/services),
# and report doc/CRD counts + largest doc size.
#
# Why fetch CRDs separately: Helm 3's `helm template` does NOT emit files under
# the chart's `crds/` directory (they are install-only), so the operator's ~24
# CustomResourceDefinitions would be missing and every VM* CR would fail to
# apply with "no matches for kind". We fetch them with `helm show crds` and
# prepend them. Within a single file leancd parses documents in order
# (manifest::parse_str preserves document order), so CRDs precede their CRs —
# though CR establishment in the API server still takes a pass or two
# (R2: recovered next pass via missing -> drift -> re-apply).
set -euo pipefail
source "$(dirname "$0")/../lib/common.sh"
source "$(dirname "$0")/../lib/git.sh"

VM_REPO="${VM_REPO:-https://victoriametrics.github.io/helm-charts/}"
VM_CHART="${VM_CHART:-victoria-metrics-k8s-stack}"
VM_RELEASE="${VM_RELEASE:-vmks}"
# Pinned for report reproducibility (latest stable at the time of writing).
VM_CHART_VER="${VM_CHART_VER:-0.84.0}"
VALUES_FILE="$(cd "$(dirname "$0")" && pwd)/values.yaml"
DEST="${VM_RENDER_DEST:-$REPO_WORKDIR/vm-stack.yaml}"

init_repo

log "ensuring helm repo 'vm' ..."
helm repo add vm "$VM_REPO" >/dev/null 2>&1 || true
helm repo update >/dev/null 2>&1 || true

CRDS_TMP="$(mktemp)"
MAIN_TMP="$(mktemp)"
trap 'rm -f "$CRDS_TMP" "$MAIN_TMP"' EXIT

log "fetching CRDs (helm show crds) for vm/$VM_CHART@$VM_CHART_VER ..."
helm show crds "vm/$VM_CHART" --version "$VM_CHART_VER" > "$CRDS_TMP"
crd_count="$(grep -c '^kind: CustomResourceDefinition' "$CRDS_TMP" || true)"

log "rendering templates (helm template, namespace=${SYNC_NS}) ..."
helm template "$VM_RELEASE" "vm/$VM_CHART" --version "$VM_CHART_VER" \
    -f "$VALUES_FILE" --namespace "$SYNC_NS" > "$MAIN_TMP"

# Combine: CRDs first (parse order within the file), then the rendered CRs.
{
    cat "$CRDS_TMP"
    printf '\n---\n'
    cat "$MAIN_TMP"
} > "$DEST"

doc_count="$(grep -c '^---' "$DEST" || true)"
biggest="$(awk 'BEGIN{RS="\n---\n"} {n=length($0); if(n>max){max=n}} END{print max+0}' "$DEST")"
log "rendered $DEST"
log "  docs~$doc_count  crds=$crd_count  largest_doc_bytes=$biggest (k8s annotation limit 262144)"
log "  chart=vm/$VM_CHART@$VM_CHART_VER  namespace=$SYNC_NS"
