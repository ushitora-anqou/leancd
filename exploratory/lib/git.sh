#!/usr/bin/env bash
# Git operations against the shared Forgejo repo, driven from the host.
: "${REPO_WORKDIR:=/tmp/compare-repo}"

init_repo() {
    if [[ -d "$REPO_WORKDIR/.git" ]]; then return 0; fi
    rm -rf "$REPO_WORKDIR"
    git clone -q "$(forgejo_push_url)" "$REPO_WORKDIR" 2>&1 | tail -3 || true
    git -C "$REPO_WORKDIR" config user.email "compare@leancd"
    git -C "$REPO_WORKDIR" config user.name "compare"
    git -C "$REPO_WORKDIR" config commit.gpgsign false
}

# Write stdin to <relpath> inside the repo.
write_file() {
    local rel="$1"
    local dest="$REPO_WORKDIR/$rel"
    mkdir -p "$(dirname "$dest")"
    cat > "$dest"
}

remove_file() { rm -f "$REPO_WORKDIR/$1"; }

# Strip all manifests from the repo (keep README from auto_init).
reset_repo() {
    init_repo
    find "$REPO_WORKDIR" \( -name '*.yaml' -o -name '*.yml' \) -delete 2>/dev/null || true
}

# Stage, commit (if needed) and force-push to main.
push_repo() {
    local msg="$1"
    init_repo
    git -C "$REPO_WORKDIR" add -A
    if git -C "$REPO_WORKDIR" diff --cached --quiet; then
        log "git: no changes to commit ($msg)"
        return 0
    fi
    git -C "$REPO_WORKDIR" commit -q -m "$msg"
    git -C "$REPO_WORKDIR" push -q -f origin HEAD:main
    log "git: pushed $(git -C "$REPO_WORKDIR" rev-parse --short HEAD) — $msg"
}

head_sha() { git -C "$REPO_WORKDIR" rev-parse HEAD; }

# Wait for both controllers to converge on the current repo HEAD.
# leancd: state ConfigMap data.last_sha == HEAD. argocd: sync.status==Synced and
# sync.revision == HEAD. Also nudges Argo CD to hard-refresh (it otherwise polls
# git every 3 min).
wait_sync() {
    local timeout="${1:-30}"
    local sha
    sha="$(head_sha)"
    kc_argo patch application compare -n argocd --type merge \
        -p '{"metadata":{"annotations":{"argocd.argoproj.io/refresh":"hard"}}}' 2>/dev/null || true
    log "waiting for both controllers to converge on HEAD ${sha:0:8} (timeout ${timeout}s)..."
    # Poll _both_synced until leancd's state.last_sha and Argo CD's sync.revision
    # both equal HEAD. An earlier fixed-sleep workaround existed for BUG 2 (state
    # CM prune churn) and BUG 3 (perpetual re-apply load); both are now fixed
    # (state.rs carries no managed-by label so the state CM is not pruned, and
    # drift.rs compares arrays element-wise by subset so steady state no longer
    # re-applies forever), so the state ConfigMap is stable and the poll loop is
    # reliable.
    wait_for "$timeout" "both controllers synced to ${sha:0:8}" _both_synced "$sha"
}

_sync_diag() {
    log "  leancd last_sha: $(kc_lean get configmap leancd-state -n app -o jsonpath='{.data.state}' 2>/dev/null | jq -r '.last_sha // empty' 2>/dev/null | cut -c1-12)"
    log "  argocd status:   $(kc_argo get application compare -n argocd -o jsonpath='{.status.sync.status}' 2>/dev/null) rev=$(kc_argo get application compare -n argocd -o jsonpath='{.status.sync.revision}' 2>/dev/null | cut -c1-12)"
}

_both_synced() {
    local sha="$1"
    local lean_sha lean_drift argo_status argo_rev state_json
    # State is the unified `.data.state` JSON blob (state.rs::to_data). The state
    # CM is SSA-patched once per pass, so a read can race with a patch in a
    # narrow window — retry a few times to get a non-empty read.
    local i
    for i in 1 2 3 4 5; do
        state_json="$(kc_lean get configmap leancd-state -n app -o jsonpath='{.data.state}' 2>/dev/null || true)"
        lean_sha="$(printf '%s' "$state_json" | jq -r '.last_sha // empty' 2>/dev/null || true)"
        [[ -n "$lean_sha" ]] && break
        sleep 1
    done
    lean_drift="$(printf '%s' "$state_json" | jq -r '.drift_count // empty' 2>/dev/null || true)"
    argo_status="$(kc_argo get application compare -n argocd -o jsonpath='{.status.sync.status}' 2>/dev/null || true)"
    argo_rev="$(kc_argo get application compare -n argocd -o jsonpath='{.status.sync.revision}' 2>/dev/null || true)"
    # leancd is converged only when it has BOTH seen HEAD and its drift-check
    # pass reports no drifts. A full-apply pass sets last_sha=HEAD before the
    # CRD-established CRs have all applied (they fail until the CRD is
    # established, then recover on the next drift→re-apply pass); waiting for
    # drift_count==0 ensures every Git resource is actually live before we
    # compare. Argo CD's `Synced` is NOT required: many resources leave Argo CD
    # OutOfSync on server-injected defaults it has no ignoreDifferences for
    # (StatefulSet volumeClaimTemplates, Service clusterIP, …) even after a
    # correct SSA apply; its revision matching HEAD means it has seen the commit
    # and its automated sync has applied. Final-state equality is then checked
    # directly by compare_resource.
    [[ "$lean_sha" == "$sha" && "$lean_drift" == "0" && "$argo_rev" == "$sha" ]]
}
