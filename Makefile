.PHONY: all build test fmt release release-nix

all: fmt build test

build:
	cargo build

test:
	nix flake check

fmt:
	cargo fmt
	taplo fmt Cargo.toml taplo.toml deny.toml
