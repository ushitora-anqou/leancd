#!/usr/bin/env bash
# Safety assertions for the leancd-vs-ArgoCD exploratory comparison.
#
# The "safety" guarantee axis goes beyond final-state equality (compare.sh):
# it checks that leancd does not over/under-prune, does not loop on drift,
# can reclaim SSA field-manager conflicts, and runs hooks in ascending
# hook-weight order (Argo CD parity). Each helper prints a [PASS]/[FAIL] line
# and returns 0/1 so a scenario can record the outcome in its report section.
#
# Sourced after common.sh and compare.sh (uses kc_lean, exists_in, jq).

# Read a field from leancd's state ConfigMap. State is persisted as the unified
# `.data.state` JSON blob (state.rs::to_data): {last_sha, sync_count,
# managed_count, drift_count, last_error, ...}. Returns "" on any failure.
# Usage: _state_field <ns> <field>
_state_field() {
    local ns="$1" field="$2"
    kc_lean get configmap leancd-state -n "$ns" -o jsonpath='{.data.state}' 2>/dev/null \
        | jq -r --arg f "$field" '.[$f] // empty' 2>/dev/null
}

# Assert leancd's drift_count settled to 0 across two reads a few seconds apart.
# A perpetual re-apply loop (a drift false-positive, BUG 3/6/9 class) keeps
# drift_count > 0 on every pass.
# Usage: assert_drift_settled [ns]
assert_drift_settled() {
    local ns="${1:-app}"
    local d1 d2
    d1="$(_state_field "$ns" drift_count)"
    sleep 5
    d2="$(_state_field "$ns" drift_count)"
    if [[ "$d1" == "0" && "$d2" == "0" ]]; then
        echo "  [PASS] drift settled (drift_count=0 across two reads)"
        return 0
    fi
    echo "  [FAIL] drift NOT settled: read1=${d1:-?} read2=${d2:-?} (possible re-apply loop)"
    return 1
}

# Assert a resource leancd should have pruned is now absent (prune
# under-delete guard). Usage: assert_pruned <kind> <name> [ns]
assert_pruned() {
    local kind="$1" name="$2" ns="${3:-}"
    if exists_in lean "$kind" "$name" "$ns"; then
        echo "  [FAIL] ${ns:+$ns/}$kind/$name still exists in leancd (prune under-delete)"
        return 1
    fi
    echo "  [PASS] ${ns:+$ns/}$kind/$name pruned in leancd"
    return 0
}

# Assert a resource leancd must keep (resource-policy:keep, a helm hook, or
# simply still Git-declared) was NOT pruned (prune over-delete guard).
# Usage: assert_kept <kind> <name> [ns]
assert_kept() {
    local kind="$1" name="$2" ns="${3:-}"
    if ! exists_in lean "$kind" "$name" "$ns"; then
        echo "  [FAIL] ${ns:+$ns/}$kind/$name missing in leancd (prune over-delete)"
        return 1
    fi
    echo "  [PASS] ${ns:+$ns/}$kind/$name kept in leancd"
    return 0
}

# Assert a field on a leancd resource was reclaimed to the Git-expected value
# (SSA field-manager conflict reclaim check, BUG 4 class). leancd always applies
# with force-conflict SSA, so a field taken by another manager must return.
# Usage: assert_field_reclaimed <kind> <name> <ns> <jsonpath> <expected>
assert_field_reclaimed() {
    local kind="$1" name="$2" ns="$3" path="$4" expected="$5"
    local actual
    actual="$(kc_lean get "$kind" "$name" -n "$ns" -o jsonpath="$path" 2>/dev/null)"
    if [[ "$actual" == "$expected" ]]; then
        echo "  [PASS] $ns/$kind/$name $path reclaimed to '$expected'"
        return 0
    fi
    echo "  [FAIL] $ns/$kind/$name $path='$actual' (expected '$expected'; SSA conflict not reclaimed)"
    return 1
}

# Assert leancd ran hooks in non-decreasing hook-weight order for the recent
# passes, by scanning its logs for the "running helm hook" lines (which carry
# weight=N). Argo CD runs Helm hooks in ascending weight (hookByWeight); leancd
# matches via hooks.rs::sort_by_weight. Returns 0 if the observed weight
# sequence is non-decreasing. Usage: assert_hook_order [phase]
assert_hook_order() {
    local phase="${1:-}"
    local lines weights
    lines="$(kc_lean logs deploy/leancd -n leancd --tail=2000 2>/dev/null \
        | sed 's/\x1b\[[0-9;]*m//g' \
        | grep 'running helm hook')"
    if [[ -n "$phase" ]]; then
        lines="$(echo "$lines" | grep "phase=$phase")"
    fi
    weights="$(echo "$lines" | grep -oE 'weight=-?[0-9]+' | grep -oE '\-?[0-9]+')"
    if [[ -z "$weights" ]]; then
        echo "  [WARN] no hook-run log lines found${phase:+ for phase=$phase}; cannot verify order"
        return 0
    fi
    local prev="-999999" ok=1 w
    while read -r w; do
        if (( w < prev )); then ok=0; break; fi
        prev="$w"
    done <<< "$weights"
    if (( ok )); then
        echo "  [PASS] hook weights non-decreasing: $(echo "$weights" | tr '\n' ' ')"
        return 0
    fi
    echo "  [FAIL] hook weights NOT ascending: $(echo "$weights" | tr '\n' ' ')"
    return 1
}
