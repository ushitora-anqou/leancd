.PHONY: all build test test-unit fmt bench scale

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

fmt:
	cargo fmt
	taplo fmt Cargo.toml taplo.toml deny.toml
