#!/usr/bin/env bash
# Health-heavy scenario: verify RSS stays under budget when Argo CD-style
# resource-health assessment is ON against a workload of Deployments that each
# fan out to many ReplicaSet/Pod children (the ownerReference chain). Each
# Deployment's .status is read every pass by the health assessment, so this
# stresses the same List/status path the steady-state health evaluation uses —
# at a larger fan-out and namespace count than the default bench.
#
# Mirrors bench/cache-bloat.sh: it reuses bench.sh with env overrides (bench.sh
# and gen-manifests.sh honor BENCH_NAMESPACE_COUNT / BENCH_DEP_REPLICAS /
# LEANCD_WATCH_MODE / LEANCD_HEALTH_MODE) and parses the `leancd-bench:`
# summary line. Runs in cache mode and is gated at RSS_BUDGET_MIB (default 50).
#
# Prereqs: kind, kubectl, git, cargo (same as bench.sh).
#
# Tunables:
#   RSS_BUDGET_MIB        RSS budget in MiB (default 50)
#   HEALTH_HEAVY_NS       namespace count (default 30)
#   HEALTH_HEAVY_REPLICAS replicas per Deployment (default 6)
#   KIND_CLUSTER_NAME     base kind cluster name (default leancd-health)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUDGET_MIB="${RSS_BUDGET_MIB:-50}"
BUDGET_BYTES=$(( BUDGET_MIB * 1024 * 1024 ))
HEAVY_NS="${HEALTH_HEAVY_NS:-30}"
HEAVY_REPLICAS="${HEALTH_HEAVY_REPLICAS:-6}"
CLUSTER="${KIND_CLUSTER_NAME:-leancd-health}"

mib() { # bytes -> MiB (integer), or "-" when empty
  [ -n "$1" ] && echo $(( ($1 + 0) / 1024 / 1024 )) || echo "-"
}

# Run bench.sh with env from the caller; parse its `leancd-bench:` summary line
# into PEAK/IDLE/TMAX/TMIN (bytes). If the summary is missing, force the values
# to the budget so the row FAILs.
PEAK=0; IDLE=0; TMAX=0; TMIN=0
run_bench_capture() {
  local out_file line
  out_file="$(mktemp)"
  "$ROOT/bench/bench.sh" >"$out_file" 2>&1 || true
  line="$(grep -m1 '^leancd-bench:' "$out_file" || true)"
  if [ -z "$line" ]; then
    cat "$out_file" >&2
    PEAK=$BUDGET_BYTES; IDLE=$BUDGET_BYTES; TMAX=$BUDGET_BYTES; TMIN=$BUDGET_BYTES
  else
    PEAK="$(printf '%s' "$line" | sed -n 's/.*peak=\([0-9]*\).*/\1/p')"
    IDLE="$(printf '%s' "$line" | sed -n 's/.*idle=\([0-9]*\).*/\1/p')"
    TMAX="$(printf '%s' "$line" | sed -n 's/.*treerss_max=\([0-9]*\).*/\1/p')"
    TMIN="$(printf '%s' "$line" | sed -n 's/.*treerss_min=\([0-9]*\).*/\1/p')"
  fi
  rm -f "$out_file"
}

overall=0
print_row() { # name peak idle tmax tmin
  local name="$1" status="PASS" v
  for v in "$2" "$3" "$4" "$5"; do
    if [ "${v:-0}" -ge "$BUDGET_BYTES" ]; then status="FAIL"; overall=1; fi
  done
  printf '%-12s %10s %10s %10s %10s %8s\n' \
    "$name" "$(mib "$2")" "$(mib "$3")" "$(mib "$4")" "$(mib "$5")" "$status"
}

printf '\nHealth-heavy scenario (cache mode, health ON, %sMiB budget)\n\n' "$BUDGET_MIB"
printf '%-12s %10s %10s %10s %10s %8s\n' \
  "scenario" "peak(MiB)" "idle(MiB)" "treepk(MiB)" "treeidl(MiB)" "status"

echo ">> scenario health-heavy: $HEAVY_NS namespaces x replicas=$HEAVY_REPLICAS, health assessment on" >&2
BENCH_NAMESPACE_COUNT="$HEAVY_NS" BENCH_DEP_REPLICAS="$HEAVY_REPLICAS" \
  LEANCD_WATCH_MODE=cache LEANCD_HEALTH_MODE=on \
  RSS_BUDGET_MIB="$BUDGET_MIB" KIND_CLUSTER_NAME="$CLUSTER" run_bench_capture
print_row "health-heavy" "$PEAK" "$IDLE" "$TMAX" "$TMIN"

echo
if [ "$overall" -eq 0 ]; then
  echo "PASS: health-heavy scenario under the ${BUDGET_MIB}MiB budget"
else
  echo "FAIL: health-heavy scenario breached the ${BUDGET_MIB}MiB budget" >&2
fi
exit "$overall"
