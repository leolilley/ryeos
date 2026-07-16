#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
helper="$root/scripts/lib/ryeos-terminal.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

before="$(set +o)"
# shellcheck source=scripts/lib/ryeos-terminal.sh
source "$helper"
after="$(set +o)"
[[ "$before" == "$after" ]] || {
    printf 'terminal helper changed caller shell options\n' >&2
    exit 1
}

RYEOS_TTY=never ryeos_term_init
RYEOS_TTY=never ryeos_term_begin VERIFY "plain phase" 2>"$tmp/plain"
RYEOS_TTY=never ryeos_term_update "plain update" "detail" 2>>"$tmp/plain"
RYEOS_TTY=never ryeos_term_end success VERIFY "done" 2>>"$tmp/plain"
if grep -q $'\033' "$tmp/plain"; then
    printf 'plain output contained ANSI bytes\n' >&2
    exit 1
fi
grep -q 'RYE/OS VERIFY plain phase' "$tmp/plain"
grep -q 'plain update' "$tmp/plain"

status=0
RYEOS_TTY=never ryeos_term_run RUN child -- bash -c 'exit 23' \
    >/dev/null 2>"$tmp/failure" || status=$?
[[ "$status" == 23 ]]
grep -q 'RUN FAILED' "$tmp/failure"
if grep -q 'RUN COMPLETE' "$tmp/failure"; then
    printf 'failure path printed success\n' >&2
    exit 1
fi

NO_COLOR=1 TERM=xterm RYEOS_TTY=always bash -c \
    'source "$1"; ryeos_term_init; ryeos_term_begin RUN colorless; ryeos_term_cleanup' \
    _ "$helper" 2>"$tmp/no-color"
if grep -q $'\033\[[0-9;]*m' "$tmp/no-color"; then
    printf 'NO_COLOR output contained color sequences\n' >&2
    exit 1
fi

NO_COLOR= TERM=xterm RYEOS_TTY=always bash -c \
    'source "$1"; ryeos_term_init; ryeos_term_begin RUN empty-no-color; ryeos_term_cleanup' \
    _ "$helper" 2>"$tmp/empty-no-color"
if grep -q $'\033\[[0-9;]*m' "$tmp/empty-no-color"; then
    printf 'empty NO_COLOR output contained color sequences\n' >&2
    exit 1
fi

TERM=dumb RYEOS_TTY=always bash -c \
    'source "$1"; ryeos_term_init; ryeos_term_begin RUN dumb; ryeos_term_cleanup' \
    _ "$helper" 2>"$tmp/dumb"
if grep -q $'\033' "$tmp/dumb"; then
    printf 'TERM=dumb output contained ANSI bytes\n' >&2
    exit 1
fi
grep -q '^RYE/OS RUN dumb$' "$tmp/dumb"

status=0
TERM=xterm RYEOS_TTY=always bash -c \
    'source "$1"; ryeos_term_init; ryeos_term_begin INSTALL phase; exit 7' \
    _ "$helper" 2>"$tmp/exit-cleanup" || status=$?
[[ "$status" == 7 ]]
grep -q 'INSTALL FAILED' "$tmp/exit-cleanup"

status=0
TERM=xterm RYEOS_TTY=always bash -c \
    'source "$1"; ryeos_term_init; ryeos_term_begin INSTALL phase; ryeos_term_note detail; exit 9' \
    _ "$helper" 2>"$tmp/note-failure" || status=$?
[[ "$status" == 9 ]]
grep -q 'INSTALL FAILED' "$tmp/note-failure"

TERM=xterm bash -c \
    'source "$1"; ryeos_term_init; ryeos_term_begin RUN redirected; ryeos_term_cleanup' \
    _ "$helper" 2>"$tmp/auto-redirected"
if grep -q $'\033' "$tmp/auto-redirected"; then
    printf 'auto redirected output contained ANSI bytes\n' >&2
    exit 1
fi

TERM=xterm RYEOS_TTY=invalid bash -c \
    'source "$1"; ryeos_term_init; ryeos_term_begin RUN invalid-override; ryeos_term_cleanup' \
    _ "$helper" 2>"$tmp/invalid-override"
if grep -q $'\033' "$tmp/invalid-override"; then
    printf 'invalid RYEOS_TTY did not fall back to auto detection\n' >&2
    exit 1
fi

TERM=xterm RYEOS_TTY=always bash -c \
    'source "$1"; ryeos_term_init; ryeos_term_begin INSTALL parent; ryeos_term_run RUN child -- true; ryeos_term_end success "INSTALL COMPLETE" done' \
    _ "$helper" 2>"$tmp/nested"
grep -q 'RUN COMPLETE' "$tmp/nested"
grep -q 'resuming' "$tmp/nested"
grep -q 'INSTALL COMPLETE' "$tmp/nested"

TERM=xterm RYEOS_TTY=always bash -c \
    'source "$1"; ryeos_term_init; ryeos_term_run RUN first -- true; ryeos_term_run RUN second -- true' \
    _ "$helper" 2>"$tmp/sequential"
if grep -q 'resuming' "$tmp/sequential"; then
    printf 'sequential phases were incorrectly treated as nested\n' >&2
    exit 1
fi
grep -q $'\033\[1;38;2;142;192;124m' "$tmp/sequential"

status=0
TERM=xterm RYEOS_TTY=always bash -c \
    'trap '\''printf prior-trap\\n'\'' EXIT; source "$1"; ryeos_term_init; ryeos_term_begin RUN signal; kill -TERM "$$"' \
    _ "$helper" >"$tmp/signal-stdout" 2>"$tmp/signal-stderr" || status=$?
[[ "$status" == 143 ]]
grep -q 'prior-trap' "$tmp/signal-stdout"
if grep -q 'RUN COMPLETE' "$tmp/signal-stderr"; then
    printf 'signal path printed success\n' >&2
    exit 1
fi

NO_COLOR=1 TERM=xterm COLUMNS=20 RYEOS_TTY=always bash -c \
    'source "$1"; ryeos_term_init; ryeos_term_begin RUN "a deliberately long narrow-terminal label"; ryeos_term_cleanup' \
    _ "$helper" 2>"$tmp/narrow"
awk 'length($0) > 20 { exit 1 }' "$tmp/narrow" || {
    printf 'narrow terminal output exceeded COLUMNS\n' >&2
    exit 1
}

NO_COLOR=1 TERM=xterm COLUMNS=40 RYEOS_TTY=always bash -c \
    'source "$1"; ryeos_term_init; ryeos_term_begin VERIFY "界界界界界界界界界界界界界界界界"; ryeos_term_update "界界界界界界界界界界界界界界界界" detail; ryeos_term_cleanup' \
    _ "$helper" 2>"$tmp/wide-cells"
if (( $(wc -L < "$tmp/wide-cells") > 40 )); then
    printf 'wide terminal output exceeded COLUMNS\n' >&2
    exit 1
fi

if command -v script >/dev/null 2>&1; then
    TERM=xterm RYEOS_TTY=auto script -qec \
        "bash -c \"source '$helper'; ryeos_term_init; ryeos_term_begin RUN pty; ryeos_term_cleanup\"" \
        "$tmp/pty" >/dev/null
    grep -q $'\033\[2K' "$tmp/pty"
fi

printf 'ryeos terminal helper cases passed\n'
