#!/usr/bin/env bash
# Tear down the exploratory comparison environment.
set -euo pipefail
source "$(dirname "$0")/lib/common.sh"

log "tearing down compare environment..."
kind delete cluster --name "$LEANCD_CLUSTER" >/dev/null 2>&1 || true
kind delete cluster --name "$ARGOCD_CLUSTER" >/dev/null 2>&1 || true
docker rm -f "$FORGEJO_CONTAINER" >/dev/null 2>&1 || true
rm -f "$STATE_FILE"
log "done"
