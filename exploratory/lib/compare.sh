#!/usr/bin/env bash
# Comparison helpers: fetch a resource from each cluster as normalized JSON and
# diff them, ignoring manager-specific and server-injected fields.

# Normalize one k8s object read from stdin (kubectl -o json). Strips status,
# server-injected metadata, and manager-specific annotations/labels so a
# leancd-managed object and an argocd-managed object can be compared.
normalize() {
    jq '
      del(
        .status,
        .metadata.managedFields,
        .metadata.resourceVersion,
        .metadata.uid,
        .metadata.generation,
        .metadata.creationTimestamp,
        .metadata.selfLink,
        .metadata.ownerReferences,
        .metadata.clusterName,
        .metadata.finalizers,
        .metadata.generateName
      )
      | if (.metadata.annotations // null) == null then . else
          .metadata.annotations |= with_entries(select(
            (.key != "argocd.argoproj.io/tracking-id")
            and (.key != "argocd.argoproj.io/last-applied-configuration")
            and (.key != "kubectl.kubernetes.io/last-applied-configuration")
            and (.key != "deployment.kubernetes.io/revision")
          ))
        end
      | if (.metadata.labels // null) == null then . else
          .metadata.labels |= with_entries(select(
            (.key != "app.kubernetes.io/managed-by")
            and (.key != "leancd")
            and ((.key | startswith("argocd.argoproj.io/")) | not)
          ))
        end
      | if ((.metadata.labels // {}) | length) == 0 then del(.metadata.labels) else . end
      | if ((.metadata.annotations // {}) | length) == 0 then del(.metadata.annotations) else . end
      | del(.spec.clusterIP?, .spec.clusterIPs?, .spec.externalIPs?,
            .spec.externalTrafficPolicy?, .spec.ipFamilies?, .spec.ipFamilyPolicy?,
            .spec.internalTrafficPolicy?, .spec.sessionAffinity?,
            .spec.allocateLoadBalancerNodePorts?, .spec.healthCheckNodePort?)
      | (if (.spec.template.spec.containers // null) == null then . else
           .spec.template.spec.containers |= map(del(.resources?, .imagePullPolicy?,
             .terminationMessagePath?, .terminationMessagePolicy?, .volumeMounts?))
         end)
      | del(.spec.template?.spec?.restartPolicy?, .spec.template?.spec?.dnsPolicy?,
            .spec.template?.spec?.schedulerName?, .spec.template?.spec?.securityContext?,
            .spec.template?.spec?.terminationGracePeriodSeconds?,
            .spec.template?.spec?.enableServiceLinks?, .spec.strategy?,
            .spec.progressDeadlineSeconds?, .spec.revisionHistoryLimit?)
    ' 2>/dev/null
}

# get_norm <lean|argo> <kind> <name> [namespace]
get_norm() {
    local side="$1" kind="$2" name="$3" ns="${4:-}"
    if [[ "$side" == "lean" ]]; then
        if [[ -n "$ns" ]]; then
            kc_lean get "$kind" "$name" -n "$ns" -o json 2>/dev/null || true
        else
            kc_lean get "$kind" "$name" -o json 2>/dev/null || true
        fi
    else
        if [[ -n "$ns" ]]; then
            kc_argo get "$kind" "$name" -n "$ns" -o json 2>/dev/null || true
        else
            kc_argo get "$kind" "$name" -o json 2>/dev/null || true
        fi
    fi | normalize
}

# compare_resource <kind> <name> [namespace]
# Sets global COMPARE_RESULT (match|lean_missing|argo_missing|both_absent|diff)
# and returns 0 on match/both_absent, 1 on any divergence.
compare_resource() {
    local kind="$1" name="$2" ns="${3:-}"
    local label="${ns:+$ns/}$kind/$name"
    local lean argo
    lean="$(get_norm lean "$kind" "$name" "$ns")"
    argo="$(get_norm argo "$kind" "$name" "$ns")"
    if [[ -z "$lean" && -z "$argo" ]]; then
        echo "  [-] BOTH ABSENT: $label"; COMPARE_RESULT=both_absent; return 0
    elif [[ -z "$lean" ]]; then
        echo "  [!] MISSING in leancd: $label"; COMPARE_RESULT=lean_missing; return 1
    elif [[ -z "$argo" ]]; then
        echo "  [!] MISSING in argocd: $label"; COMPARE_RESULT=argo_missing; return 1
    elif diff <(printf '%s' "$lean") <(printf '%s' "$argo") >/tmp/leancd-cmp.diff 2>&1; then
        echo "  [=] MATCH: $label"; COMPARE_RESULT=match; return 0
    else
        echo "  [~] DIFF: $label"
        sed 's/^/      /' /tmp/leancd-cmp.diff | head -40
        COMPARE_RESULT=diff; return 1
    fi
}

# Check existence only (no content comparison). Sets COMPARE_RESULT.
exists_in() { # <lean|argo> <kind> <name> [namespace]
    local side="$1" kind="$2" name="$3" ns="${4:-}"
    if [[ "$side" == "lean" ]]; then
        kc_lean get "$kind" "$name" ${ns:+-n "$ns"} >/dev/null 2>&1
    else
        kc_argo get "$kind" "$name" ${ns:+-n "$ns"} >/dev/null 2>&1
    fi
}
