#!/usr/bin/env bash
# Benchmark leancd's RSS against the headline budget (default 100MiB) using a
# simulated cluster on `kind`.
#
# The benchmark builds leancd in release mode and runs it as a process pointed
# at the kind cluster (kubeconfig), syncing a generated manifest set. It samples
# two footprints in parallel:
#   - self:  leancd's own RSS, from its `leancd_rss_bytes` metric.
#   - tree:  the whole process tree (leancd + git/ssh subprocesses), summed via
#            `ps`. Shared pages are double-counted, so this overestimates on
#            purpose — a conservative regression gate.
# Both the self and tree peak/idle values must stay under the budget. Running
# leancd as a process (rather than in-cluster) measures exactly the same code
# paths and memory profile.
#
# Prereqs: kind, kubectl, git, curl, cargo.
#
# Tunables:
#   BENCH_NAMESPACE_COUNT  number of namespaces to generate (default 15)
#   RSS_BUDGET_MIB        RSS budget in MiB (default 100)
#   BENCH_SAMPLE_SECS     seconds to sample RSS for peak detection (default 30)
#   KIND_CLUSTER_NAME     kind cluster name (default leancd-bench)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NS_COUNT="${BENCH_NAMESPACE_COUNT:-15}"
BUDGET_BYTES=$(( ${RSS_BUDGET_MIB:-100} * 1024 * 1024 ))
CLUSTER="${KIND_CLUSTER_NAME:-leancd-bench}"
WORK="$(mktemp -d)"
LEANC_PID=""

cleanup() {
  [ -n "$LEANC_PID" ] && kill "$LEANC_PID" >/dev/null 2>&1 || true
  rm -rf "$WORK"
  kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Sum RSS (bytes) of a process and ALL its descendants (leancd + git/ssh
# subprocesses). A single `ps` snapshot keeps the whole tree at one instant. The
# ps RSS column is KiB, so multiply by 1024. Shared pages between processes are
# double-counted, so this overestimates — deliberately conservative (safe) for
# a regression gate.
tree_rss_bytes() {
  local root="$1"
  command -v ps >/dev/null 2>&1 || { echo 0; return; }
  ps -eo pid=,ppid=,rss= | awk -v root="$root" '
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

echo ">> creating kind cluster '$CLUSTER'"
kind create cluster --name "$CLUSTER" >/dev/null
# kind writes to ~/.kube/config; leancd's Client::try_default picks it up.

echo ">> generating $NS_COUNT namespaces (x18 resources each)"
"$ROOT/bench/gen-manifests.sh" "$NS_COUNT" "$WORK/repo"
git -C "$WORK/repo" init -q -b main
git -C "$WORK/repo" add -A
git -C "$WORK/repo" -c user.email=bench@leancd -c user.name=bench commit -qm "benchmark manifests"

echo ">> building leancd (release)"
( cd "$ROOT" && cargo build --release )

export LEANCD_NAMESPACE=default
export LEANCD_REPO_URL="file://$WORK/repo"
export LEANCD_BRANCH=main
export LEANCD_PATH=.
export LEANCD_POLL_INTERVAL=10s
export LEANCD_WORK_DIR="$WORK/leancd-work"
export LEANCD_METRICS_ADDR=127.0.0.1:19090

echo ">> starting leancd controller"
"$ROOT/target/release/leancd" controller >/dev/null 2>&1 &
LEANC_PID=$!

# Sample RSS from startup through the settled state. The maximum observed value
# covers the sync peak (fetch/parse/apply); the last sample is the idle RSS.
# Both must stay under the budget (design §8.2).
SAMPLE_SECS="${BENCH_SAMPLE_SECS:-30}"
METRICS_URL="http://127.0.0.1:19090/metrics"

echo ">> sampling RSS for ${SAMPLE_SECS}s (self peak/idle + process-tree peak/idle)"
PEAK=0
IDLE=0
PEAK_TREE=0
IDLE_TREE=0
end=$(( $(date +%s) + SAMPLE_SECS ))
while [ "$(date +%s)" -lt "$end" ]; do
  # leancd's own RSS, from its self-published metric.
  if sample="$(curl -fsS "$METRICS_URL" 2>/dev/null \
               | awk '/^leancd_rss_bytes / {print $2}' | tail -1)"; then
    if [ -n "$sample" ]; then
      IDLE="$sample"
      [ "$sample" -gt "$PEAK" ] && PEAK="$sample"
    fi
  fi
  # Whole process tree: leancd + git (and any ssh) subprocesses it spawns.
  if t="$(tree_rss_bytes "$LEANC_PID")"; then
    IDLE_TREE="$t"
    [ "$t" -gt "$PEAK_TREE" ] && PEAK_TREE="$t"
  fi
  sleep 0.5
done

if [ "$PEAK" -eq 0 ]; then
  echo "FAIL: could not read leancd_rss_bytes metric during sampling" >&2
  exit 1
fi

# Invariant: the tree includes leancd itself, so its RSS must be >= the self
# RSS. A violation means the tree walk is broken.
if [ "$PEAK_TREE" -lt "$PEAK" ]; then
  echo "FAIL (invariant): tree peak $PEAK_TREE < self peak $PEAK — tree must include self" >&2
  exit 1
fi

echo ">> self  peak RSS = $PEAK bytes ($(( PEAK / 1024 / 1024 ))MiB)"
echo ">> self  idle RSS = $IDLE bytes ($(( IDLE / 1024 / 1024 ))MiB)"
echo ">> tree peak RSS = $PEAK_TREE bytes ($(( PEAK_TREE / 1024 / 1024 ))MiB)"
echo ">> tree idle RSS = $IDLE_TREE bytes ($(( IDLE_TREE / 1024 / 1024 ))MiB)"

status=0
check() { # name value
  if [ "$2" -ge "$BUDGET_BYTES" ]; then
    echo "FAIL: $1 RSS $2 >= budget $BUDGET_BYTES" >&2
    status=1
  fi
}
check "self  peak" "$PEAK"
check "self  idle" "$IDLE"
check "tree peak" "$PEAK_TREE"
check "tree idle" "$IDLE_TREE"
[ "$status" -ne 0 ] && exit "$status"

echo "PASS: self peak $PEAK / idle $IDLE, tree peak $PEAK_TREE / idle $IDLE_TREE bytes under the $(( BUDGET_BYTES / 1024 / 1024 ))MiB budget"
# Machine-parseable summary for bench/scale.sh. The tree keys deliberately avoid
# the `peak=`/`idle=` substrings so scale.sh's greedy sed does not mis-match them.
echo "leancd-bench: peak=$PEAK idle=$IDLE budget=$BUDGET_BYTES treerss_max=$PEAK_TREE treerss_min=$IDLE_TREE"
