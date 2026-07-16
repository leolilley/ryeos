#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

violations="$(
    rg -n 'println!|eprintln!|writeln!|write!|print!' crates/bin/cli/src \
        --glob '*.rs' \
        --glob '!crates/bin/cli/src/tty/**' \
        --glob '!crates/bin/cli/src/help.rs' \
        || true
)"

if [[ -n "$violations" ]]; then
    printf 'CLI presentation writes must go through tty renderers:\n%s\n' "$violations" >&2
    exit 1
fi

# Help builds semantic documents for the shared renderer. It must never regain
# its former direct stdout/stderr side effects.
help_violations="$(
    rg -n 'println!|eprintln!|std::io::stdout|std::io::stderr' \
        crates/bin/cli/src/help.rs || true
)"
if [[ -n "$help_violations" ]]; then
    printf 'CLI help contains a direct terminal write:\n%s\n' "$help_violations" >&2
    exit 1
fi

printf 'CLI presentation boundaries: clean\n'
