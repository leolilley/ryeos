#!/usr/bin/env bash
# Starts the sccache server for this job, or switches every later step of the
# job to plain rustc. Never exits non-zero: a cache outage slows the job, it
# does not fail it.
set -uo pipefail

ATTEMPTS=3
START_TIMEOUT=120

fall_back() {
    local why="$1"
    # An empty RUSTC_WRAPPER is how cargo is told to call rustc directly.
    echo "RUSTC_WRAPPER=" >> "$GITHUB_ENV"
    echo "SCCACHE_GHA_ENABLED=false" >> "$GITHUB_ENV"
    echo "::warning title=sccache unavailable: building without the cache::${why}. This job builds and tests with plain rustc; it runs the same work, uncached and slower."
    {
        echo "### sccache unavailable: plain rustc"
        echo
        echo "${why}. RUSTC_WRAPPER is cleared for the rest of this job; builds and tests run uncached."
    } >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
    exit 0
}

if [[ "${SCCACHE_OFF:-false}" == true ]]; then
    fall_back "sccache is switched off for this run"
fi

sccache="${SCCACHE_PATH:-}"
if [[ -z "$sccache" || ! -x "$sccache" ]]; then
    fall_back "sccache was not installed (the setup action failed)"
fi

# A healthy server must not stop on its own mid-job: a restart needs the cache
# backend, which is what fails during an outage.
export SCCACHE_IDLE_TIMEOUT=0

for attempt in $(seq 1 "$ATTEMPTS"); do
    if timeout "$START_TIMEOUT" "$sccache" --start-server \
        && timeout 60 "$sccache" --show-stats; then
        echo "sccache server is up (attempt ${attempt} of ${ATTEMPTS})."
        # Compiles go through the wrapper, which falls back to plain rustc if
        # the server is lost later in the job. It is copied out of the
        # checkout so a later checkout step cannot remove it.
        wrapper_dir="${RUNNER_TEMP:?}/sccache-or-plain"
        mkdir -p "$wrapper_dir"
        install -m 0755 "$GITHUB_ACTION_PATH/rustc-wrapper.sh" "$wrapper_dir/rustc-wrapper.sh"
        {
            echo "SCCACHE_IDLE_TIMEOUT=0"
            echo "SCCACHE_FALLBACK_MARK=$wrapper_dir/fell-back"
            echo "RUSTC_WRAPPER=$wrapper_dir/rustc-wrapper.sh"
        } >> "$GITHUB_ENV"
        exit 0
    fi
    echo "sccache server did not answer (attempt ${attempt} of ${ATTEMPTS})."
    timeout 30 "$sccache" --stop-server >/dev/null 2>&1 || true
    [[ "$attempt" -lt "$ATTEMPTS" ]] && sleep 15
done

fall_back "the sccache server did not start after ${ATTEMPTS} attempts"
