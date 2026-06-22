.PHONY: all build test test-unit fmt bench scale e2e release

all: fmt build test-unit

build:
	cargo build

test-unit:
	cargo test

# Full CI checks (fmt, clippy, nextest, deny, audit) via nix.
test:
	nix flake check

# RSS benchmark against a kind cluster (requires kind, kubectl, git, curl).
bench:
	./bench/bench.sh

# RSS across increasing scales (requires kind; see bench/scale.sh).
scale:
	./bench/scale.sh

# End-to-end tests against a kind cluster with an in-cluster Forgejo and
# leancd (requires docker, kind, kubectl, git, curl). Not part of nix flake
# check (no Docker in the sandbox); run it manually or in an external CI job,
# like make bench.
e2e:
	cargo test --test e2e -- --ignored --test-threads=1 --nocapture

fmt:
	cargo fmt
	taplo fmt Cargo.toml taplo.toml deny.toml

# Cut a release: bumps the patch version (Cargo.toml + Chart.yaml
# version/appVersion), moves the CHANGELOG [Unreleased] section under a dated
# [X.Y.Z] heading, runs the full local gate, then commits, signs a tag, and
# pushes (triggers .github/workflows/release.yml end to end). Write the
# changelog entries under [Unreleased] first. Env RELEASE_DRYRUN=1 previews
# the bump without committing/tagging/pushing.
release:
	./scripts/release.sh
