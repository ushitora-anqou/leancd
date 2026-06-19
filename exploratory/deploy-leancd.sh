#!/usr/bin/env bash
# Deploy leancd into the leancd-compare cluster, pointing it at the shared Forgejo repo.
set -euo pipefail
source "$(dirname "$0")/lib/common.sh"

log "deploying leancd to kind-${LEANCD_CLUSTER} (sync target namespace: ${SYNC_NS})..."
kc_lean create namespace "$SYNC_NS" 2>/dev/null || true
sed "s|__FORGEJO_GIT_URL__|$(forgejo_git_url)|g" \
    "$EXPLORATORY_DIR/manifests/leancd.yaml" | kc_lean apply -f - >/dev/null

log "restarting leancd Deployment to pick up the freshly-loaded image..."
kc_lean rollout restart deploy/leancd -n leancd
log "waiting for leancd Deployment to roll out..."
kc_lean rollout status deploy/leancd -n leancd --timeout=120s >/dev/null

# Give it a couple of poll cycles to do its first reconcile (clone + state write).
log "leancd Pod recent logs:"
sleep 8
kc_lean logs deploy/leancd -n leancd --tail=30 || true
