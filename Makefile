.PHONY: all build test test-unit fmt bench scale e2e

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
