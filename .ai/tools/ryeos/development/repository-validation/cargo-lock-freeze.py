#!/usr/bin/env python3
# ryeos:signed:2026-11-27T23:05:43Z:887c379643f580fd39f966be94b81e1615d95c6cc1893c6d3ac8c80af4d20646:+/+B+a7DOPDySSp73ZeppwVbLBzIWn8gC9aDpoW7sJ4pQ7p1a8MybcnquGMzlEnaT1lAiiDQUnfL8esX9W3nAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/repository-validation
#   version: "1.0.0"
#   description: While Cargo.lock.freeze exists, fail unless the root Cargo.lock still hashes to the value it records
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   host_authority: required
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false

"""Hold Cargo.lock still while release products pin it.

The release products pin the exact root Cargo.lock in signed items, so while
those pins stand a changed lock invalidates them. The freeze is a file at the
repository root, `Cargo.lock.freeze`, with two lines:

    sha256 <64 lowercase hex: the sha256 of Cargo.lock>
    reason <one line: why it is frozen, and what thaws it>

While the file exists the check requires the root `Cargo.lock` to hash to that
value. A change to the lock therefore has to change the freeze file in the same
commit, which is visible in review and refused by every gate that runs this
check (a deliberate re-pin edits both; thawing deletes the file). With no
freeze file the check passes and says it is not frozen. A malformed freeze file
is a failure, never a pass.

--check       check --project-path; exit 1 on failure
--self-test   alter copies and require each alteration to fail with its own
              message and the unaltered pair to pass

Usage: cargo-lock-freeze.py (--check | --self-test) [--project-path DIR]
"""

import argparse
import hashlib
import re
import sys
import tempfile
from pathlib import Path

FREEZE = "Cargo.lock.freeze"
LOCK = "Cargo.lock"
HEX = re.compile(r"^[0-9a-f]{64}$")


def check(root: Path):
    """Return (ok, message)."""
    freeze = root / FREEZE
    if not freeze.exists():
        return True, f"{LOCK} is not frozen (no {FREEZE})"
    lines = [l for l in freeze.read_text(encoding="utf-8").splitlines() if l.strip()]
    if len(lines) != 2 or not lines[0].startswith("sha256 ") or not lines[1].startswith("reason "):
        return False, f"{FREEZE} must be exactly two lines: 'sha256 <hex>' and 'reason <text>'"
    recorded = lines[0][len("sha256 "):].strip()
    if not HEX.match(recorded):
        return False, f"{FREEZE}: the sha256 is not 64 lowercase hex digits"
    if not lines[1][len("reason "):].strip():
        return False, f"{FREEZE}: the reason is empty"
    lock = root / LOCK
    if not lock.is_file():
        return False, f"{LOCK} is missing while {FREEZE} exists"
    actual = hashlib.sha256(lock.read_bytes()).hexdigest()
    if actual != recorded:
        return False, (
            f"{LOCK} changed while frozen: it hashes to {actual}, {FREEZE} records {recorded}. "
            "The release products pin the recorded lock; a re-pin edits both files in one change, "
            f"a thaw deletes {FREEZE}"
        )
    return True, f"{LOCK} matches {FREEZE} ({recorded[:12]})"


def self_test():
    failures = []
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        lock = b"# lock\n[[package]]\nname = \"a\"\n"
        (root / LOCK).write_bytes(lock)
        digest = hashlib.sha256(lock).hexdigest()

        def case(name, freeze_text, lock_bytes, want_ok, want_text):
            f = root / FREEZE
            if freeze_text is None:
                f.unlink(missing_ok=True)
            else:
                f.write_text(freeze_text, encoding="utf-8")
            (root / LOCK).write_bytes(lock_bytes)
            ok, message = check(root)
            if ok != want_ok or want_text not in message:
                failures.append(f"{name}: got ({ok}, {message!r}), want ({want_ok}, ...{want_text}...)")

        good = f"sha256 {digest}\nreason release pins until R-5 is qualified\n"
        case("not frozen", None, lock, True, "not frozen")
        case("frozen and unchanged", good, lock, True, "matches")
        case("frozen and changed", good, lock + b"x", False, "changed while frozen")
        case("bad hash text", "sha256 xyz\nreason r\n", lock, False, "not 64 lowercase hex")
        case("uppercase hash", f"sha256 {digest.upper()}\nreason r\n", lock, False, "not 64 lowercase hex")
        case("empty reason", f"sha256 {digest}\nreason \n", lock, False, "reason is empty")
        case("extra line", good + "note\n", lock, False, "exactly two lines")
        case("missing reason line", f"sha256 {digest}\n", lock, False, "exactly two lines")
        (root / LOCK).unlink()
        (root / FREEZE).write_text(good, encoding="utf-8")
        ok, message = check(root)
        if ok or "missing while" not in message:
            failures.append(f"no lock: got ({ok}, {message!r})")
    return failures


def main():
    parser = argparse.ArgumentParser()
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--self-test", action="store_true")
    parser.add_argument("--project-path", default=".")
    args = parser.parse_args()
    if args.self_test:
        failures = self_test()
        for f in failures:
            print("FAIL", f)
        print("cargo-lock-freeze self-test:", "FAILED" if failures else "ok")
        return 1 if failures else 0
    ok, message = check(Path(args.project_path))
    print(("ok: " if ok else "FAIL: ") + message)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
