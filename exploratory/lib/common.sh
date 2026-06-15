#!/usr/bin/env bash
# Shared constants and helpers for the leancd-vs-ArgoCD exploratory comparison.
# Sourced by every other script under exploratory/.

set -euo pipefail

# --- Fixed identity (must match across all scripts) ---
export LEANCD_CLUSTER="${LEANCD_CLUSTER:-leancd-compare}"
export ARGOCD_CLUSTER="${ARGOCD_CLUSTER:-argocd-compare}"
export FORGEJO_CONTAINER="${FORGEJO_CONTAINER:-forgejo-compare}"
export FORGEJO_DOCKER_NET="${FORGEJO_DOCKER_NET:-kind}" # the network kind nodes share
export FORGEJO_HOST_PORT="${FORGEJO_HOST_PORT:-3000}"   # host-side port for git push / API
export FORGEJO_USER="${FORGEJO_USER:-leancd}"
export FORGEJO_PASS="${FORGEJO_PASS:-leancd-compare-pass}"
export FORGEJO_EMAIL="${FORGEJO_EMAIL:-leancd@compare.local}"
export REPO_OWNER="${REPO_OWNER:-leancd}"
export REPO="${REPO:-compare}"

# Namespace that BOTH controllers sync application manifests into. Controllers
# themselves live in their own namespaces (leancd / argocd); synced resources
# land here so we can compare apples-to-apples.
export SYNC_NS="${SYNC_NS:-app}"

# Where leancd writes its state ConfigMap. Argo CD has no equivalent.
export LEANCD_NS="leancd"
export ARGOCD_NS="argocd"

# Resolve repo root (exploratory/..) regardless of where we are sourced from.
EXPLORATORY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export EXPLORATORY_DIR
export STATE_FILE="${STATE_FILE:-$EXPLORATORY_DIR/.state}"
export NOTES_DIR="${NOTES_DIR:-$EXPLORATORY_DIR/notes}"

mkdir -p "$NOTES_DIR"

# Load dynamic state (FORGEJO_IP, repo URLs, ...) if setup.sh already wrote it.
if [[ -f "$STATE_FILE" ]]; then
    # shellcheck disable=SC1090
    source "$STATE_FILE"
fi

log()  { echo "[$(date +%H:%M:%S)] $*" >&2; }
note() { echo "[$(date +%H:%M:%S)] $*"; }

# Forgejo URLs ---------------------------------------------------------------
# Host-side URL (git push + REST API from the test driver).
forgejo_host_url()   { echo "http://127.0.0.1:${FORGEJO_HOST_PORT}"; }
# In-cluster URL reachable from Pods (uses the Forgejo container's Docker IP).
forgejo_cluster_url() { echo "http://${FORGEJO_IP}:3000"; }
# Git URL a controller points at, reachable from inside the kind network.
forgejo_git_url()    { echo "$(forgejo_cluster_url)/${REPO_OWNER}/${REPO}.git"; }
# Git URL the test driver pushes to (host side, with embedded credentials).
forgejo_push_url()   { echo "http://${FORGEJO_USER}:${FORGEJO_PASS}@127.0.0.1:${FORGEJO_HOST_PORT}/${REPO_OWNER}/${REPO}.git"; }

# kubectl helpers -------------------------------------------------------------
kc_lean() { timeout 12 kubectl --request-timeout=8s --context "kind-${LEANCD_CLUSTER}" "$@"; }
kc_argo() { timeout 12 kubectl --request-timeout=8s --context "kind-${ARGOCD_CLUSTER}" "$@"; }

# Poll a predicate until it returns 0 or the timeout elapses.
# Usage: wait_for <seconds> <description> <cmd...>
wait_for() {
    local timeout="$1"; local desc="$2"; shift 2
    local deadline=$(( $(date +%s) + timeout ))
    while (( $(date +%s) < deadline )); do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 2
    done
    log "TIMEOUT waiting for: $desc"
    return 1
}
