#!/usr/bin/env bash
# Install the Argo CD controller into the argocd-compare cluster.
#
# This is the piece that was missing from the harness: `deploy-argocd.sh` only
# configures the Application (repo Secret + AppProject + Application) and assumes
# Argo CD is already running. Here we install the controller itself from the
# official manifest.
#
# We call kubectl directly instead of `kc_argo`: kc_argo's 12s/request-timeout
# is far too short for applying the multi-MB install manifest and for the
# argocd-server rollout wait.
set -euo pipefail
source "$(dirname "$0")/lib/common.sh"

# `stable` tracks the latest Argo CD v3; pin ARGOCD_VER for reproducibility.
ARGOCD_VER="${ARGOCD_VER:-stable}"
ARGOCD_MANIFEST="https://raw.githubusercontent.com/argoproj/argo-cd/${ARGOCD_VER}/manifests/install.yaml"
ARGO_CTX="kind-${ARGOCD_CLUSTER}"

log "installing Argo CD (${ARGOCD_VER}) into ${ARGO_CTX} ..."
kubectl --context "$ARGO_CTX" create namespace "$ARGOCD_NS" 2>/dev/null || true

# --server-side --force-conflicts is mandatory per the Argo CD install docs:
# the bundled CRDs exceed the client-side-apply annotation size limit.
log "applying install manifest (this can take a minute or two) ..."
kubectl --context "$ARGO_CTX" apply -n "$ARGOCD_NS" --server-side --force-conflicts \
    -f "$ARGOCD_MANIFEST" >/dev/null

log "waiting for argocd-server to roll out ..."
kubectl --context "$ARGO_CTX" rollout status deploy/argocd-server -n "$ARGOCD_NS" \
    --timeout=420s >/dev/null

argocd_image="$(kubectl --context "$ARGO_CTX" get deploy argocd-server -n "$ARGOCD_NS" \
    -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || echo unknown)"
log "Argo CD ready in ${ARGO_CTX} (server image=${argocd_image})"
