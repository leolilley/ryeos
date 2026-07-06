# syntax=docker/dockerfile:1.7
#
# ryeosd-full — daemon + bundles composed image (Tier 1 distribution).
# Published to ghcr.io/leolilley/ryeosd-full:<version> by .github/workflows/publish-ryeosd.yml.

# ── Stage 1: Build all binaries + publish bundles ──
FROM rust:1.95-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# Build provenance: baked into the binary at compile time (ryeos-app/build.rs
# reads RYEOS_VCS_REF / RYEOS_BUILD_DATE) and onto the image as OCI labels in
# the runtime stage. Pass via `--build-arg VCS_REF=$(git rev-parse HEAD)` and
# `--build-arg BUILD_DATE=$(date -u +%Y-%m-%dT%H:%M:%SZ)`.
ARG VCS_REF=unknown
ARG BUILD_DATE=unknown
ENV RYEOS_VCS_REF=$VCS_REF
ENV RYEOS_BUILD_DATE=$BUILD_DATE

# Build all binaries, stage into bundle trees, publish bundles.
# The publisher key is injected via BuildKit secret mount — the build
# fails if the secret is missing or empty.
RUN --mount=type=secret,id=publisher-key \
    test -s /run/secrets/publisher-key && \
    ./scripts/populate-bundles.sh \
      --key /run/secrets/publisher-key \
      --owner ryeos-official \
      --all

# ── Stage 2: Runtime image ──
# Keep the runtime Debian generation compatible with the Rust builder image;
# rust:1.95-slim currently links binaries requiring GLIBC_2.39+.
FROM debian:trixie-slim

# Node 22 for TS-authored project tools (e.g. backend-client.js), and Python
# for the bundled python/function and python/script runtimes. Include venv/pip
# so project images can install their own Python tool dependencies without
# rebuilding the RyeOS base from scratch.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg python3 python3-venv python3-pip && \
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y --no-install-recommends nodejs && \
    apt-get purge -y curl gnupg && apt-get autoremove -y && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Daemon + ops CLI + core tools (authorize-client, sign, identity, etc.).
COPY --from=builder /build/target/release/ryeosd             /usr/local/bin/ryeosd
COPY --from=builder /build/target/release/ryeos              /usr/local/bin/ryeos
COPY --from=builder /build/target/release/ryeos-core-tools   /usr/local/bin/ryeos-core-tools

# Bundles with rebuilt CAS, baked into /opt (read-only template).
COPY --from=builder /build/bundles/.ai       /opt/ryeos/.ai
COPY --from=builder /build/bundles/core      /opt/ryeos/core
COPY --from=builder /build/bundles/standard  /opt/ryeos/standard
COPY --from=builder /build/bundles/web       /opt/ryeos/web
COPY --from=builder /build/bundles/ryeos-ui   /opt/ryeos/ryeos-ui
COPY --from=builder /build/bundles/hosted-node /opt/ryeos/hosted-node

# Entrypoint runs ryeos init every boot (idempotent) then starts daemon.
# /data/app persists across redeploys.
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

ENV HOME=/data/app
ENV RYEOS_APP_ROOT=/data/app
EXPOSE 8000

# Re-declared here: build-stage ARGs do not carry across FROM boundaries.
ARG VCS_REF=unknown
ARG BUILD_DATE=unknown
LABEL org.opencontainers.image.source="https://github.com/leolilley/ryeos"
LABEL org.opencontainers.image.revision="$VCS_REF"
LABEL org.opencontainers.image.created="$BUILD_DATE"
LABEL io.ryeos.host-triple="x86_64-unknown-linux-gnu"
LABEL io.ryeos.bundle-protocol="1.0"

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
