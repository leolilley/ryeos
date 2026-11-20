FROM rust:1.95-slim-bookworm@sha256:d7482085ff5b415f84dba5647ae71606650bdef00db7aeb69f4b3d170c3e4082
WORKDIR /src
COPY . /src
RUN CARGO_TARGET_DIR=/cargo-t cargo build --release -p lillux --example probe_durable_root
RUN --mount=type=cache,id=ryeos-release-verify,target=/verify-root,sharing=locked \
    set -x; mkdir -p /verify-root/probe-$$ /tmp/probe-overlay; \
    echo '--- mountinfo lines for /verify-root and /'; \
    grep -E ' /verify-root | / / ' /proc/self/mountinfo || true; \
    stat -f -c 'verify-root fs=%T' /verify-root; stat -f -c 'tmp fs=%T' /tmp; \
    (findmnt -T /verify-root || cat /proc/self/mountinfo | head -50) ; \
    /cargo-t/release/examples/probe_durable_root /verify-root/probe-$$ /tmp/probe-overlay; \
    rm -rf /verify-root/probe-$$
