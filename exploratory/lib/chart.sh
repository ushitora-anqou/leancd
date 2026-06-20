#!/usr/bin/env bash
# Generic Helm-chart renderer for the leancd-vs-ArgoCD comparison. Renders a
# chart into the shared compare repo workdir as a single multi-doc YAML (CRDs
# first, then the rendered resources), so leancd parses CRDs before their CRs.
#
# Why fetch CRDs separately: Helm 3's `helm template` does NOT emit files under
# a chart's `crds/` directory (they are install-only), so CRDs would be missing
# and every CR would fail to apply with "no matches for kind". We fetch them
# with `helm show crds` and prepend them; within the single output file leancd
# parses documents in order (manifest::parse_str preserves document order).
#
# Usage: render_chart <repo_name> <repo_url> <chart> <version> <release> \
#                     <values_file> <dest> [extra helm args...]

_CHART_CRDS_TMP=""
_CHART_MAIN_TMP=""
_chart_cleanup() { rm -f "$_CHART_CRDS_TMP" "$_CHART_MAIN_TMP"; }

render_chart() {
    local repo_name="$1" repo_url="$2" chart="$3" version="$4"
    local release="$5" values_file="$6" dest="$7"
    shift 7
    local extra=("$@")

    init_repo

    log "ensuring helm repo '$repo_name' ..."
    helm repo add "$repo_name" "$repo_url" >/dev/null 2>&1 || true
    helm repo update >/dev/null 2>&1 || true

    _CHART_CRDS_TMP="$(mktemp)"
    _CHART_MAIN_TMP="$(mktemp)"
    trap _chart_cleanup EXIT

    log "fetching CRDs (helm show crds) for $repo_name/$chart@$version ..."
    # Some charts ship no CRDs; treat an empty/failed result as "no CRDs".
    helm show crds "$repo_name/$chart" --version "$version" > "$_CHART_CRDS_TMP" 2>/dev/null || true
    local crd_count
    crd_count="$(grep -c '^kind: CustomResourceDefinition' "$_CHART_CRDS_TMP" || true)"

    log "rendering templates (helm template, namespace=${SYNC_NS}) ..."
    if [[ "${#extra[@]}" -gt 0 ]]; then
        helm template "$release" "$repo_name/$chart" --version "$version" \
            -f "$values_file" --namespace "$SYNC_NS" "${extra[@]}" > "$_CHART_MAIN_TMP"
    else
        helm template "$release" "$repo_name/$chart" --version "$version" \
            -f "$values_file" --namespace "$SYNC_NS" > "$_CHART_MAIN_TMP"
    fi

    # Combine: CRDs first (parse order within the file), then the rendered CRs.
    {
        cat "$_CHART_CRDS_TMP"
        printf '\n---\n'
        cat "$_CHART_MAIN_TMP"
    } > "$dest"

    local doc_count biggest
    doc_count="$(grep -c '^---' "$dest" || true)"
    biggest="$(awk 'BEGIN{RS="\n---\n"} {n=length($0); if(n>max){max=n}} END{print max+0}' "$dest")"
    log "rendered $dest"
    log "  docs~$doc_count  crds=$crd_count  largest_doc_bytes=$biggest (k8s annotation limit 262144)"
    log "  chart=$repo_name/$chart@$version  namespace=$SYNC_NS"
}
