#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

violations="$(
    grep -R -nE \
        --include='*.rs' \
        --exclude='help.rs' \
        --exclude-dir='tty' \
        'println!|eprintln!|writeln!|write!|print!' crates/bin/cli/src \
        || true
)"

if [[ -n "$violations" ]]; then
    printf 'CLI presentation writes must go through tty renderers:\n%s\n' "$violations" >&2
    exit 1
fi

# Help builds semantic documents for the shared renderer. It must never regain
# its former direct stdout/stderr side effects.
help_violations="$(
    grep -nE 'println!|eprintln!|std::io::stdout|std::io::stderr' \
        crates/bin/cli/src/help.rs || true
)"
if [[ -n "$help_violations" ]]; then
    printf 'CLI help contains a direct terminal write:\n%s\n' "$help_violations" >&2
    exit 1
fi

# The terminal client is launched by `ryeos tui`, so its startup output is
# part of the CLI presentation contract and must reuse ryeos_cli::tty.
tui_violations="$(
    grep -nE 'println!|eprintln!|std::io::stdout|std::io::stderr' \
        crates/clients/terminal/src/main.rs || true
)"
if [[ -n "$tui_violations" ]]; then
    printf 'ryeos-tui startup contains a direct terminal write:\n%s\n' "$tui_violations" >&2
    exit 1
fi

# `web --print-url` intentionally owns one exact stdout serializer. Human
# launcher diagnostics on stderr must still use the shared console.
web_violations="$(
    grep -nE 'eprintln!|std::io::stderr' crates/clients/web/src/bin/web.rs || true
)"
if [[ -n "$web_violations" ]]; then
    printf 'web launcher contains a direct terminal diagnostic:\n%s\n' "$web_violations" >&2
    exit 1
fi

printf 'CLI presentation boundaries: clean\n'
