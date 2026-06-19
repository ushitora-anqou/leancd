#!/usr/bin/env bash
# Configure the Argo CD Application (repo Secret + AppProject + Application) on
# the argocd-compare cluster, pointing at the shared Forgejo repo.
set -euo pipefail
source "$(dirname "$0")/lib/common.sh"

_argocd_app_synced() {
    local phase
    phase="$(kc_argo get application compare -n argocd \
        -o jsonpath='{.status.sync.status}' 2>/dev/null || true)"
    [[ "$phase" == "Synced" ]]
}

log "configuring Argo CD Application on kind-${ARGOCD_CLUSTER}..."
kc_argo create namespace "$SYNC_NS" 2>/dev/null || true
sed "s|__FORGEJO_GIT_URL__|$(forgejo_git_url)|g" \
    "$EXPLORATORY_DIR/manifests/argocd-app.yaml" | kc_argo apply -f - >/dev/null

log "waiting for Argo CD to reach Synced (this also validates Pod->Forgejo reachability)..."
if wait_for 240 "argocd app 'compare' Synced" _argocd_app_synced; then
    log "Argo CD application is Synced."
else
    log "WARN: Argo CD app not Synced within timeout. Diagnostics:"
    kc_argo get application compare -n argocd 2>&1 | head -30 || true
    kc_argo -n argocd logs deploy/argocd-repo-server --tail=30 2>&1 | tail -30 || true
    exit 1
fi

log "Argo CD application summary:"
kc_argo get application compare -n argocd 2>&1 | head -20 || true
