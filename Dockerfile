# syntax=docker/dockerfile:1.7
#
# ryeosd-full — daemon + bundles composed image (Tier 1 distribution).
# Published to ghcr.io/leolilley/ryeosd-full:<version> by .github/workflows/publish-ryeosd.yml.

# ── Stage 1: Build all binaries + rebuild bundle CAS ──
FROM rust:1.86-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN cargo build --release \
      -p ryeosd \
      -p ryeos-directive-runtime \
      -p ryeos-graph-runtime \
      -p ryeos-knowledge-runtime \
      -p ryeos-handler-bins \
      -p ryeos-cli && \
    cargo build --release -p ryeos-tools \
      --bin rye-bundle-tool --bin rye-inspect --bin rye-sign

# Place freshly built runtimes/CLI into the standard bundle's bin/<triple>/.
RUN install -m 0755 \
      target/release/ryeos-directive-runtime \
      target/release/ryeos-graph-runtime \
      target/release/ryeos-knowledge-runtime \
      target/release/rye \
      ryeos-bundles/standard/.ai/bin/x86_64-unknown-linux-gnu/

# Place freshly built handlers/CLI tools into the core bundle's bin/<triple>/.
RUN install -m 0755 \
      target/release/rye-parser-yaml-document \
      target/release/rye-parser-yaml-header-document \
      target/release/rye-parser-regex-kv \
      target/release/rye-composer-extends-chain \
      target/release/rye-composer-graph-permissions \
      target/release/rye-composer-identity \
      target/release/rye-inspect \
      target/release/rye-sign \
      target/release/rye-tool-streaming-demo \
      ryeos-bundles/core/.ai/bin/x86_64-unknown-linux-gnu/

# Rebuild CAS for both bundles. The seed default is for local docker-build runs;
# CI replaces this with a real key (see workflow Phase 2 hardening).
ARG RYE_SIGNING_SEED=1
RUN target/release/rye-bundle-tool rebuild-manifest \
        --source ryeos-bundles/standard --seed ${RYE_SIGNING_SEED} && \
    target/release/rye-bundle-tool rebuild-manifest \
        --source ryeos-bundles/core --seed ${RYE_SIGNING_SEED}

# ── Stage 2: Runtime image ──
FROM debian:bookworm-slim

# Node 22 for TS-authored project tools (e.g. backend-client.js).
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg && \
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y --no-install-recommends nodejs && \
    apt-get purge -y curl gnupg && apt-get autoremove -y && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Daemon + ops CLIs.
COPY --from=builder /build/target/release/ryeosd       /usr/local/bin/ryeosd
COPY --from=builder /build/target/release/rye          /usr/local/bin/rye
COPY --from=builder /build/target/release/rye-inspect  /usr/local/bin/rye-inspect
COPY --from=builder /build/target/release/rye-sign     /usr/local/bin/rye-sign

# Bundles with rebuilt CAS, baked into /opt (read-only template).
COPY --from=builder /build/ryeos-bundles/core      /opt/ryeos/core
COPY --from=builder /build/ryeos-bundles/standard  /opt/ryeos/standard

# Entrypoint seeds /data on first boot, then starts daemon.
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

ENV RYE_SYSTEM_SPACE=/data/core
EXPOSE 8000

LABEL org.opencontainers.image.source="https://github.com/leolilley/ryeos"
LABEL io.rye.host-triple="x86_64-unknown-linux-gnu"
LABEL io.rye.bundle-protocol="1.0"

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
