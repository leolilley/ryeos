#!/usr/bin/env bash
# RUSTC_WRAPPER for a job whose sccache server was up at the start. Cargo runs
# it as `rustc-wrapper.sh <rustc> <args...>`. Each compile goes through
# sccache; when sccache itself cannot reach or restart its server (the server
# stopped mid-job and the cache backend is down), that compile runs with plain
# rustc instead, and a marker sends every later compile of the job straight to
# rustc so none of them waits on a server that will not start.
set -uo pipefail

mark="${SCCACHE_FALLBACK_MARK:?SCCACHE_FALLBACK_MARK is set by sccache-or-plain}"
if [[ -e "$mark" ]]; then
    exec "$@"
fi

# Cargo reads the compiler's stderr to its end, so holding it until sccache
# exits changes no output, only when it arrives.
err="$(mktemp)"
"${SCCACHE_PATH:-sccache}" "$@" 2>"$err"
status=$?
# Only sccache's own client error about its server counts: it is the first
# line sccache writes, before any compiler runs. Compiler diagnostics (JSON
# objects, or rendered text with a gutter) never start the stream with it, so
# a compile error is passed through untouched.
if [[ "$status" -ne 0 ]] \
    && head -n 1 "$err" | grep -qE '^sccache: error: (Timed out waiting for server startup|Server startup failed)'; then
    rm -f "$err"
    if (set -o noclobber; : > "$mark") 2>/dev/null; then
        echo "::warning title=sccache lost mid-job: building without the cache::The sccache server stopped and could not restart; the rest of this job compiles with plain rustc." >&2
        {
            echo "### sccache lost mid-job: plain rustc"
            echo
            echo "The sccache server stopped and could not restart; the remaining compiles of this job ran uncached."
        } >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
    fi
    exec "$@"
fi
cat "$err" >&2
rm -f "$err"
exit "$status"
