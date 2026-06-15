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
    log "nudging Argo CD + fixed wait ${timeout}s for convergence (HEAD ${sha:0:8})..."
    # Fixed wait rather than polling _both_synced: leancd prunes+rewrites its
    # state ConfigMap every pass (BUG 2) and kube API calls occasionally stall
    # under leancd's perpetual re-apply load (BUG 3), which made the poll loop
    # unreliable. leancd polls every 15s and Argo CD self-heals on refresh, so a
    # 30s wait is enough for normal cases (60s for CRD establishment).
    sleep "$timeout"
    return 0
}

_sync_diag() {
    log "  leancd last_sha: $(kc_lean get configmap leancd-state -n app -o jsonpath='{.data.last_sha}' 2>/dev/null | cut -c1-12)"
    log "  argocd status:   $(kc_argo get application compare -n argocd -o jsonpath='{.status.sync.status}' 2>/dev/null) rev=$(kc_argo get application compare -n argocd -o jsonpath='{.status.sync.revision}' 2>/dev/null | cut -c1-12)"
}

_both_synced() {
    local sha="$1"
    local lean_sha argo_status argo_rev
    # leancd prunes+rewrites its own state ConfigMap every pass (BUG 2), so the
    # read can 404 in a narrow window. Retry a few times before giving up.
    local i
    for i in 1 2 3 4 5; do
        lean_sha="$(kc_lean get configmap leancd-state -n app -o jsonpath='{.data.last_sha}' 2>/dev/null || true)"
        [[ -n "$lean_sha" ]] && break
        sleep 1
    done
    argo_status="$(kc_argo get application compare -n argocd -o jsonpath='{.status.sync.status}' 2>/dev/null || true)"
    argo_rev="$(kc_argo get application compare -n argocd -o jsonpath='{.status.sync.revision}' 2>/dev/null || true)"
    [[ "$lean_sha" == "$sha" && "$argo_status" == "Synced" && "$argo_rev" == "$sha" ]]
}
