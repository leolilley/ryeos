#!/usr/bin/env python3
# ryeos:signed:2026-11-20T00:51:46Z:7afd25e0e1a4b3298272485b0cd603f672a18541d57da924393afb3aaabd1fb9:yn+/HtenJexOhnAJS/+QGKqemUlafGCav19KrilzR2j/hoEI1/llKjMA/oTv8N/N9QR1UNerk6fif5hV52QlBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority. Run it from a checkout of this repository.
# Print a per-view summary of how the checked-in view behavior snapshots
# (crates/clients/base/tests/snapshots/views) differ between two git refs, one
# `path: old -> new` line per changed field, for reviewing a golden refresh.
#
# Usage: .ai/tools/ryeos/development/maintainer/lib/snapshot-diff.py <base-ref> [<head-ref>]
# head-ref defaults to HEAD. Exit status is 0 whether or not snapshots changed.
import json
import subprocess
import sys

DIR = "crates/clients/base/tests/snapshots/views"


def git(*args):
    return subprocess.run(
        ["git", *args], check=True, capture_output=True, text=True
    ).stdout


def show(ref, path):
    try:
        return json.loads(git("show", f"{ref}:{path}"))
    except subprocess.CalledProcessError:
        return None


def flatten(prefix, value, out):
    if isinstance(value, dict) and value:
        for key, child in value.items():
            flatten(f"{prefix}.{key}" if prefix else key, child, out)
    elif isinstance(value, list) and value:
        for index, child in enumerate(value):
            flatten(f"{prefix}[{index}]", child, out)
    else:
        text = json.dumps(value, sort_keys=True)
        out[prefix] = text if len(text) <= 160 else text[:160] + "..."


def main():
    if len(sys.argv) not in (2, 3):
        sys.exit("usage: snapshot-diff.py <base-ref> [<head-ref>]")
    base = sys.argv[1]
    head = sys.argv[2] if len(sys.argv) == 3 else "HEAD"
    names = git("diff", "--name-only", f"{base}...{head}", "--", DIR).split()
    if not names:
        print("no view snapshot changes")
        return
    for path in names:
        view = "view:" + path[len(DIR) + 1 : -len(".json")]
        old, new = show(base, path), show(head, path)
        if old is None:
            print(f"{view}: added")
            continue
        if new is None:
            print(f"{view}: removed")
            continue
        before, after = {}, {}
        flatten("", old, before)
        flatten("", new, after)
        print(f"{view}: changed")
        for key in sorted(set(before) | set(after)):
            if before.get(key) != after.get(key):
                print(f"  {key}: {before.get(key, '(absent)')} -> {after.get(key, '(removed)')}")
    print(f"{len(names)} view snapshot(s) changed")


main()
