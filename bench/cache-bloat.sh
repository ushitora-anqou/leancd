#!/usr/bin/env bash
# Cache-bloat scenarios: verify the watch cache (LightweightStore, `--watch-mode=cache`)
# stays under the RSS budget when the managed object count, per-object size, or churn
# rate is pushed well beyond the default bench. Every scenario runs in cache mode and
# is gated at RSS_BUDGET_MIB (default 50).
#
# Scenarios:
#   scale     — many namespaces: object COUNT scales the LightweightStore
#   large-obj — large ConfigMap payloads: per-object SIZE scales the LightweightStore
#   churn     — repeated create/delete of managed objects: the LightweightStore must track
#               deletes and not accumulate (leak) over many cycles
#
# `scale` and `large-obj` reuse bench/bench.sh with env overrides (bench.sh and
# gen-manifests.sh already honor BENCH_NAMESPACE_COUNT / BENCH_PAYLOAD_BYTES /
# LEANCD_WATCH_MODE). `churn` is self-contained here (it drives HEAD changes during
# sampling, which bench.sh does not do).
#
# Prereqs: kind, kubectl, git, cargo (same as bench.sh).
#
# Tunables:
#   RSS_BUDGET_MIB       RSS budget in MiB (default 50)
#   CACHE_BLOAT_NS       namespace count for `scale` (default 40)
#   CACHE_BLOAT_PAYLOAD  ConfigMap payload bytes for `large-obj` (default 51200 = 50KiB)
#   CACHE_BLOAT_CHURNS   create/delete cycles for `churn` (default 20)
#   KIND_CLUSTER_NAME    base kind cluster name (default leancd-bloat)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUDGET_MIB="${RSS_BUDGET_MIB:-50}"
BUDGET_BYTES=$(( BUDGET_MIB * 1024 * 1024 ))
BLOAT_NS="${CACHE_BLOAT_NS:-40}"
BLOAT_PAYLOAD="${CACHE_BLOAT_PAYLOAD:-51200}"
BLOAT_CHURNS="${CACHE_BLOAT_CHURNS:-20}"
CLUSTER="${KIND_CLUSTER_NAME:-leancd-bloat}"

mib() { # bytes -> MiB (integer), or "-" when empty
  [ -n "$1" ] && echo $(( ($1 + 0) / 1024 / 1024 )) || echo "-"
}

# Run bench.sh with env from the caller; parse its `leancd-bench:` summary line
# into the globals PEAK/IDLE/TMAX/TMIN (bytes). If the summary is missing (bench.sh
# failed before printing one), force the values to the budget so the row FAILs.
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

# RSS (bytes) of one process via ps (KiB column * 1024). Mirrors bench.sh.
self_rss_bytes() {
  ps -o rss= -p "$1" 2>/dev/null | awk '{ printf "%d\n", ($1+0)*1024 }'
}

# RSS (bytes) of a process and all descendants (shared pages double-counted;
# conservative). Mirrors bench.sh's tree_rss_bytes.
tree_rss_bytes() {
  ps -eo pid=,ppid=,rss= | awk -v root="$1" '
    { rss[$1]=$3; kids[$2]=kids[$2] " " $1 }
    function walk(p,   k, n, i, sum) {
      sum = rss[p]+0
      n = split(kids[p], k, " ")
      for (i=1; i<=n; i++) if (k[i]!="") sum += walk(k[i])
      return sum
    }
    END { printf "%d\n", (walk(root)+0)*1024 }
  '
}

overall=0
print_row() { # name peak idle tmax tmin
  local name="$1" status="PASS" v
  for v in "$2" "$3" "$4" "$5"; do
    if [ "${v:-0}" -ge "$BUDGET_BYTES" ]; then status="FAIL"; overall=1; fi
  done
  printf '%-11s %10s %10s %10s %10s %8s\n' \
    "$name" "$(mib "$2")" "$(mib "$3")" "$(mib "$4")" "$(mib "$5")" "$status"
}

printf '\nCache-bloat scenarios (cache mode, %sMiB budget)\n\n' "$BUDGET_MIB"
printf '%-11s %10s %10s %10s %10s %8s\n' \
  "scenario" "peak(MiB)" "idle(MiB)" "treepk(MiB)" "treeidl(MiB)" "status"

# --- Scenario 1: scale (object count) -------------------------------------
echo ">> scenario scale: $BLOAT_NS namespaces" >&2
BENCH_NAMESPACE_COUNT="$BLOAT_NS" LEANCD_WATCH_MODE=cache \
  RSS_BUDGET_MIB="$BUDGET_MIB" KIND_CLUSTER_NAME="$CLUSTER" run_bench_capture
print_row "scale" "$PEAK" "$IDLE" "$TMAX" "$TMIN"

# --- Scenario 2: large-obj (per-object size) ------------------------------
echo ">> scenario large-obj: ${BLOAT_PAYLOAD}B ConfigMap payload" >&2
BENCH_PAYLOAD_BYTES="$BLOAT_PAYLOAD" LEANCD_WATCH_MODE=cache \
  RSS_BUDGET_MIB="$BUDGET_MIB" KIND_CLUSTER_NAME="$CLUSTER" run_bench_capture
print_row "large-obj" "$PEAK" "$IDLE" "$TMAX" "$TMIN"

# --- Scenario 3: single-large-file (one big multi-doc YAML) ---------------
# Same payload as large-obj, but every manifest is merged into one file, so the
# parse path reads one large document stream rather than many small files.
echo ">> scenario single-large-file: ${BLOAT_PAYLOAD}B payload, one file" >&2
BENCH_MERGE_TO_SINGLE_FILE=1 BENCH_PAYLOAD_BYTES="$BLOAT_PAYLOAD" \
  LEANCD_WATCH_MODE=cache RSS_BUDGET_MIB="$BUDGET_MIB" KIND_CLUSTER_NAME="$CLUSTER" \
  run_bench_capture
print_row "single-file" "$PEAK" "$IDLE" "$TMAX" "$TMIN"

# --- Scenario 4: churn (create/delete leak check) -------------------------
echo ">> scenario churn: $BLOAT_CHURNS create/delete cycles" >&2
CHURN_CLUSTER="$CLUSTER-churn"
WORK="$(mktemp -d)"
LEANC_PID=""
churn_cleanup() {
  [ -n "$LEANC_PID" ] && kill "$LEANC_PID" >/dev/null 2>&1 || true
  rm -rf "$WORK"
  kind delete cluster --name "$CHURN_CLUSTER" >/dev/null 2>&1 || true
}
trap churn_cleanup EXIT

echo ">> creating kind cluster '$CHURN_CLUSTER'" >&2
kind create cluster --name "$CHURN_CLUSTER" >/dev/null

# A tiny initial manifest set: one ConfigMap.
mkdir -p "$WORK/repo"
cat > "$WORK/repo/cm-seed.yaml" <<'EOF'
apiVersion: v1
kind: ConfigMap
metadata:
  name: cm-seed
  namespace: default
data:
  k: "v"
EOF
git -C "$WORK/repo" init -q -b main
git -C "$WORK/repo" add -A
git -C "$WORK/repo" -c user.email=bloat@leancd -c user.name=bloat commit -qm "seed"

# Release build (cached after scale/large-obj ran bench.sh).
( cd "$ROOT" && cargo build --release >/dev/null 2>&1 || true )

export LEANCD_NAMESPACE=default
export LEANCD_REPO_URL="file://$WORK/repo"
export LEANCD_BRANCH=main
export LEANCD_PATH=.
export LEANCD_POLL_INTERVAL=3s
export LEANCD_WATCH_MODE=cache
export LEANCD_WORK_DIR="$WORK/leancd-work"

"$ROOT/target/release/leancd" controller >/dev/null 2>&1 &
LEANC_PID=$!

CPEAK=0; CIDLE=0; CTMAX=0; CTMIN=0
sample() {
  local s t
  s="$(self_rss_bytes "$LEANC_PID" || echo 0)"
  t="$(tree_rss_bytes "$LEANC_PID" || echo 0)"
  [ "$s" -gt "$CPEAK" ] && CPEAK="$s"; CIDLE="$s"
  [ "$t" -gt "$CTMAX" ] && CTMAX="$t"; CTMIN="$t"
}
# Let leancd do its first reconcile + populate the LightweightStore.
for _ in $(seq 1 8); do sample; sleep 1; done
# Churn: add then remove a ConfigMap each cycle. leancd polls HEAD and applies /
# prunes; the watch updates the LightweightStore (Apply on add, Delete on prune). A leak
# would show idle RSS climbing across cycles.
for i in $(seq 1 "$BLOAT_CHURNS"); do
  cat > "$WORK/repo/cm-churn-$i.yaml" <<EOF
apiVersion: v1
kind: ConfigMap
metadata:
  name: cm-churn-$i
  namespace: default
data:
  k: "$i"
EOF
  git -C "$WORK/repo" add -A
  git -C "$WORK/repo" -c user.email=bloat@leancd -c user.name=bloat commit -qm "add cm-churn-$i"
  sleep 4
  sample
  git -C "$WORK/repo" rm -q "cm-churn-$i.yaml"
  git -C "$WORK/repo" -c user.email=bloat@leancd -c user.name=bloat commit -qm "remove cm-churn-$i"
  sleep 4
  sample
done
# Settle: let the last prune propagate, then take the idle reading.
for _ in $(seq 1 6); do sample; sleep 1; done
print_row "churn" "$CPEAK" "$CIDLE" "$CTMAX" "$CTMIN"

echo
if [ "$overall" -eq 0 ]; then
  echo "PASS: all cache-bloat scenarios under the ${BUDGET_MIB}MiB budget"
else
  echo "FAIL: one or more cache-bloat scenarios breached the ${BUDGET_MIB}MiB budget" >&2
fi
exit "$overall"
