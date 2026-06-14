#!/usr/bin/env bash
# Track leancd RSS across increasing manifest scales (design §8.3).
#
# Runs bench/bench.sh once per resource count, collects the peak/idle RSS from
# its machine-parseable summary line, and prints a table. Exits non-zero if any
# level breaches the budget — so an external CI job can catch RSS regressions
# across scales (bench/README.md documents that this is a manual/external step,
# not part of `nix flake check`, because it needs kind/Docker).
#
# Prereqs: same as bench.sh (kind, kubectl, git, curl, cargo).
#
# Tunables:
#   SCALE_LEVELS      space-separated resource counts (default "100 300 500")
#   SCALE_BUDGET_MIB  RSS budget in MiB forwarded to each run (default 100)
#   BENCH_SAMPLE_SECS forwarded to bench.sh (default 30)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LEVELS="${SCALE_LEVELS:-100 300 500}"
BUDGET_MIB="${SCALE_BUDGET_MIB:-100}"

mib() { # bytes -> MiB (integer), or "-" when empty
  [ -n "$1" ] && echo $(( $1 / 1024 / 1024 )) || echo "-"
}

printf '%-8s %12s %12s %8s\n' "count" "peak(MiB)" "idle(MiB)" "status"
overall=0
for count in $LEVELS; do
  out_file="$(mktemp)"
  if BENCH_RESOURCE_COUNT="$count" RSS_BUDGET_MIB="$BUDGET_MIB" \
     "$ROOT/bench/bench.sh" >"$out_file" 2>&1; then
    status="PASS"
  else
    status="FAIL"
    overall=1
  fi

  line="$(grep -m1 '^leancd-bench:' "$out_file" || true)"
  peak="$(printf '%s' "$line" | sed -n 's/.*peak=\([0-9]*\).*/\1/p')"
  idle="$(printf '%s' "$line" | sed -n 's/.*idle=\([0-9]*\).*/\1/p')"

  printf '%-8s %12s %12s %8s\n' "$count" "$(mib "$peak")" "$(mib "$idle")" "$status"
  # Surface the failing run's output for debugging.
  [ "$status" = "FAIL" ] && cat "$out_file" >&2
  rm -f "$out_file"
done

exit "$overall"
