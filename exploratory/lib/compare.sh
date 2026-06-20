#!/usr/bin/env bash
# Comparison helpers: fetch a resource from each cluster as normalized JSON and
# diff them, ignoring manager-specific and server-injected fields.

# Normalize one k8s object read from stdin (kubectl -o json). Strips status,
# server-injected metadata, manager-specific annotations/labels, and the
# server-default fields k8s injects into pod specs, so a leancd-managed object
# and an argocd-managed object can be compared.
#
# NORMALIZE_PROFILE (env) tunes how aggressively chart/operator-specific noise
# is stripped. The semantics intentionally mirror leancd's own drift checker
# (src/drift.rs::spec_subset / is_k8s_zero_value) so the harness and leancd
# agree on what counts as a server default:
#   vm      (default) additionally drop VictoriaMetrics-operator annotations and
#           content hashes — the operator injects these at runtime, not from
#           Git, so they legitimately differ across the two clusters. Used by
#           the vm-stack scenarios (S1-S10).
#   minimal strip only the universally server/manager-injected fields below.
#           Used by the minimal-manifest and non-VM chart scenarios (S11+).
normalize() {
    jq '
      def stripContainers: map(del(.resources?, .imagePullPolicy?,
        .terminationMessagePath?, .terminationMessagePolicy?, .volumeMounts?));
      del(.status, .metadata.managedFields, .metadata.resourceVersion, .metadata.uid,
          .metadata.generation, .metadata.creationTimestamp, .metadata.selfLink,
          .metadata.ownerReferences, .metadata.clusterName, .metadata.finalizers,
          .metadata.generateName)
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
      # Strip server-default fields from every pod-spec container list the object
      # may carry: workloads under .spec.template.spec (Deployment/StatefulSet/
      # DaemonSet/Job), bare Pods under .spec, and CronJob jobTemplates. k8s
      # injects resources/imagePullPolicy/... into every container; the Git
      # manifest need not declare them, so they are noise (BUG 3 class).
      | if (.spec.template.spec.containers // null) != null then
          .spec.template.spec.containers |= stripContainers else . end
      | if (.spec.template.spec.initContainers // null) != null then
          .spec.template.spec.initContainers |= stripContainers else . end
      | if (.spec.containers // null) != null then
          .spec.containers |= stripContainers else . end
      | if (.spec.initContainers // null) != null then
          .spec.initContainers |= stripContainers else . end
      | if (.spec.jobTemplate.spec.template.spec.containers // null) != null then
          .spec.jobTemplate.spec.template.spec.containers |= stripContainers else . end
      | del(.spec.template?.spec?.restartPolicy?, .spec.template?.spec?.dnsPolicy?,
            .spec.template?.spec?.schedulerName?, .spec.template?.spec?.securityContext?,
            .spec.template?.spec?.terminationGracePeriodSeconds?,
            .spec.template?.spec?.enableServiceLinks?, .spec.strategy?,
            .spec.progressDeadlineSeconds?, .spec.revisionHistoryLimit?)
      | if $ENV.NORMALIZE_PROFILE != "minimal" then
          (if (.metadata.annotations // null) == null then . else
             .metadata.annotations |= with_entries(select(
               ((.key | startswith("checksum/")) | not)
               and ((.key | startswith("operator.victoriametrics.com/")) | not)
               and ((.key | startswith("victoriametrics.com/")) | not)
             ))
           end)
        else . end
      # Re-drop now-empty annotations/labels: the profile step above may have
      # emptied them (e.g. only tracking-id + checksum were present), and an
      # empty object on one side vs an absent field on the other reads as noise.
      | if ((.metadata.annotations // {}) | length) == 0 then del(.metadata.annotations) else . end
      | if ((.metadata.labels // {}) | length) == 0 then del(.metadata.labels) else . end
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

# Compare a Secret across the two clusters by the SET of data keys only (not the
# base64 values). Operator-generated Secrets (webhook certs) and any randomly
# seeded values legitimately differ byte-for-byte across clusters, so comparing
# values would report perpetual noise. A key-set mismatch (e.g. one cluster has
# admin-password and the other does not) is a real divergence.
# Usage: compare_secret <name> [namespace]
compare_secret() {
    local name="$1" ns="${2:-}"
    local label="${ns:+$ns/}secret/$name"
    local lean_keys argo_keys
    lean_keys="$(kc_lean get secret "$name" ${ns:+-n "$ns"} -o json 2>/dev/null \
        | jq -r '.data // {} | keys | sort | join(",")' 2>/dev/null || echo MISSING)"
    argo_keys="$(kc_argo get secret "$name" ${ns:+-n "$ns"} -o json 2>/dev/null \
        | jq -r '.data // {} | keys | sort | join(",")' 2>/dev/null || echo MISSING)"
    if [[ "$lean_keys" == "MISSING" && "$argo_keys" == "MISSING" ]]; then
        echo "  [-] BOTH ABSENT: $label"; COMPARE_RESULT=both_absent; return 0
    elif [[ "$lean_keys" == "MISSING" ]]; then
        echo "  [!] MISSING in leancd: $label"; COMPARE_RESULT=lean_missing; return 1
    elif [[ "$argo_keys" == "MISSING" ]]; then
        echo "  [!] MISSING in argocd: $label"; COMPARE_RESULT=argo_missing; return 1
    elif [[ "$lean_keys" == "$argo_keys" ]]; then
        echo "  [=] MATCH (data key-set): $label  keys=[$lean_keys]"; COMPARE_RESULT=match; return 0
    else
        echo "  [~] DIFF (data key-set): $label"
        echo "        lean=[$lean_keys]"
        echo "        argo=[$argo_keys]"
        COMPARE_RESULT=diff; return 1
    fi
}

# Summarize the managed resource COUNT for a kind across both clusters
# (namespaced: counts in SYNC_NS; cluster-scoped: counts cluster-wide). Useful
# for the "full stack deployed" scenario where enumerating every object is noisy.
# Usage: compare_count <kind> [namespace]
compare_count() {
    local kind="$1" ns="${2:-}"
    local lean_n argo_n
    lean_n="$(kc_lean get "$kind" ${ns:+-n "$ns"} -o name 2>/dev/null | wc -l | tr -d ' ')"
    argo_n="$(kc_argo get "$kind" ${ns:+-n "$ns"} -o name 2>/dev/null | wc -l | tr -d ' ')"
    local label="${ns:+$ns/}$kind"
    if [[ "$lean_n" == "$argo_n" ]]; then
        echo "  [=] MATCH count: $label  lean=$lean_n argo=$argo_n"
    else
        echo "  [~] DIFF count: $label  lean=$lean_n argo=$argo_n"
    fi
}
