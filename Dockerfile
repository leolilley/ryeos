# syntax=docker/dockerfile:1.7
#
# Local full daemon + bundles composition image. Release publication uses the
# explicitly scoped Dockerfile.standard and Dockerfile.central-host images.

# ── Stage 1: Build all binaries + publish bundles ──
FROM rust:1.95-slim@sha256:e14e87345b4d5964ddcc3491d27ee046a0f23820f340c3c1e24da6880141f7c0 AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates curl xz-utils meson ninja-build libcap-dev && \
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
FROM node:22-trixie-slim@sha256:4228fca437e45714a3ebd1d4ecd1dcc583cf79f5a940aa025e286b472d93b67c AS node-runtime

FROM debian:trixie-slim@sha256:28de0877c2189802884ccd20f15ee41c203573bd87bb6b883f5f46362d24c5c2

# Node 22 for TS-authored project tools (e.g. backend-client.js), and Python
# for the bundled python/function and python/script runtimes. Include venv/pip
# so project images can install their own Python tool dependencies without
# rebuilding the RyeOS base from scratch.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates python3 python3-venv python3-pip && \
    rm -rf /var/lib/apt/lists/*

COPY --from=node-runtime /usr/local/ /usr/local/

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

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
  CMD ["ryeos", "node", "status"]

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
