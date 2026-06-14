# syntax=docker/dockerfile:1

# Builder: compile leancd in release mode.
FROM rust:1-bookworm AS builder
WORKDIR /app

# Cache dependencies separately: copy only the manifests and build a throwaway
# binary so the dependency layer is reused unless Cargo.toml/Cargo.lock change.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Build the real binary on top of the cached dependency layer.
COPY src/ ./src/
RUN cargo build --release

# Runtime: a minimal image with the tooling leancd shells out to. Per design
# 付録B the base image must include `git` (git_sync runs git as a separate
# process); ca-certificates and openssh-client are needed for HTTPS and SSH
# transports respectively.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates openssh-client \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/leancd /usr/local/bin/leancd
ENTRYPOINT ["leancd"]
