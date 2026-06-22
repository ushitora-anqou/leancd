#!/usr/bin/env bash
# Cut a Lean CD release in one command.
#
# Bumps the patch version (Cargo.toml + Chart.yaml version/appVersion), moves
# the [Unreleased] section in CHANGELOG.md under a new dated [X.Y.Z] heading,
# runs the full local gate, then commits, tags, and pushes — which
# triggers .github/workflows/release.yml to build the image, publish the chart,
# and create the GitHub Release.
#
# Write the changelog entries under [Unreleased] first; this script only does
# the mechanical rename, the version bump, and the git dance.
#
# Env:
#   RELEASE_DRYRUN=1  preview the bump + CHANGELOG rename without committing,
#                     tagging, or pushing (rolls the working tree back).
set -euo pipefail

# --- preflight ----------------------------------------------------------------
cd "$(git rev-parse --show-toplevel 2>/dev/null)" || {
  echo "release.sh: not inside a git repo" >&2
  exit 1
}

[ "$(git rev-parse --abbrev-ref HEAD)" = "master" ] || {
  echo "release.sh: must be on master" >&2
  exit 1
}
git diff --quiet || { echo "release.sh: unstaged changes present; commit or stash first" >&2; exit 1; }
git diff --cached --quiet || { echo "release.sh: staged changes present; commit or stash first" >&2; exit 1; }

git fetch origin master --quiet
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/master)" ] || {
  echo "release.sh: HEAD != origin/master; push/pull first" >&2
  exit 1
}

# --- compute the next (patch+1) version --------------------------------------
cur=$(grep -E '^version' Cargo.toml | head -1 | awk -F'"' '{print $2}')
case "$cur" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "release.sh: could not parse version '$cur' from Cargo.toml" >&2; exit 1 ;;
esac
IFS=. read -r major minor patch <<<"$cur"
newv="$major.$minor.$((patch + 1))"
date=$(date +%Y-%m-%d)
echo "release.sh: v$cur -> v$newv ($date)"

# --- move [Unreleased] under a dated [newv] heading; prepend an empty one -----
awk -v new="## [$newv] - $date" '
  !done && /^## \[Unreleased\][[:space:]]*$/ {
    print "## [Unreleased]"
    print ""
    print "_Nothing yet._"
    print ""
    print new
    done = 1
    next
  }
  { print }
' CHANGELOG.md > CHANGELOG.md.tmp && mv CHANGELOG.md.tmp CHANGELOG.md

# --- bump Cargo.toml + Chart.yaml (version/appVersion) -----------------------
sed -i -E 's/^version = "[0-9]+\.[0-9]+\.[0-9]+"$/version = "'"$newv"'"/' Cargo.toml
# Sync Cargo.lock to the new version (nix flake check builds with --locked).
cargo update -p leancd --precise "$newv" >/dev/null
sed -i -E 's/^version: .+$/version: '"$newv"'/' charts/leancd/Chart.yaml
sed -i -E 's/^appVersion: ".*"$/appVersion: "'"$newv"'"/' charts/leancd/Chart.yaml

# --- format, then re-run the full gate ---------------------------------------
# chart-version-consistency verifies the three versions now agree.
make fmt
make test

# --- dry run: show the diff and roll back ------------------------------------
if [ "${RELEASE_DRYRUN:-0}" = "1" ]; then
  echo
  echo "=== RELEASE_DRYRUN=1: not committing / tagging / pushing. Diff: ==="
  git --no-pager diff
  git checkout -- CHANGELOG.md Cargo.toml charts/leancd/Chart.yaml
  git checkout -- Cargo.lock 2>/dev/null || true
  echo "=== rolled back the working tree ==="
  exit 0
fi

# --- commit, sign the tag, push (triggers release.yml) -----------------------
git add -u
git commit -m "chore(release): v$newv"
git tag -a "v$newv" -m "Lean CD v$newv"
git push origin master
git push origin "v$newv"

echo "release.sh: released v$newv — watch: gh run watch"
