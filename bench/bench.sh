#!/usr/bin/env bash
# Benchmark leancd's RSS against the headline budget (default 100MiB) using a
# simulated cluster on `kind`.
#
# The benchmark builds leancd in release mode and runs it as a process pointed
# at the kind cluster (kubeconfig), syncing a generated manifest set, then
# scrapes the `leancd_rss_bytes` metric. Running leancd as a process (rather
# than in-cluster) measures exactly the same code paths and memory profile.
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

echo ">> sampling RSS for ${SAMPLE_SECS}s (peak + idle)"
PEAK=0
IDLE=0
end=$(( $(date +%s) + SAMPLE_SECS ))
while [ "$(date +%s)" -lt "$end" ]; do
  if sample="$(curl -fsS "$METRICS_URL" 2>/dev/null \
               | awk '/^leancd_rss_bytes / {print $2}' | tail -1)"; then
    if [ -n "$sample" ]; then
      IDLE="$sample"
      [ "$sample" -gt "$PEAK" ] && PEAK="$sample"
    fi
  fi
  sleep 0.5
done

if [ "$PEAK" -eq 0 ]; then
  echo "FAIL: could not read leancd_rss_bytes metric during sampling" >&2
  exit 1
fi

echo ">> peak RSS = $PEAK bytes ($(( PEAK / 1024 / 1024 ))MiB)"
echo ">> idle RSS = $IDLE bytes ($(( IDLE / 1024 / 1024 ))MiB)"

status=0
if [ "$PEAK" -ge "$BUDGET_BYTES" ]; then
  echo "FAIL: peak RSS $PEAK >= budget $BUDGET_BYTES" >&2
  status=1
fi
if [ "$IDLE" -ge "$BUDGET_BYTES" ]; then
  echo "FAIL: idle RSS $IDLE >= budget $BUDGET_BYTES" >&2
  status=1
fi
[ "$status" -ne 0 ] && exit "$status"

echo "PASS: peak $PEAK / idle $IDLE bytes under the $(( BUDGET_BYTES / 1024 / 1024 ))MiB budget"
# Machine-parseable summary for bench/scale.sh.
echo "leancd-bench: peak=$PEAK idle=$IDLE budget=$BUDGET_BYTES"
