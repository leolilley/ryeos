#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:47:49Z:4808ad336f891f905761e7346aea0599e68b1945d3477bf6e630f81025f846c4:9us3/QQG+6vCLOiOnvhpvbjY6OVHlvOM3yBvEpLyhFYjgoEZyEsCirblDsB7C0U03loJ7ePLBQzBgsIIS27bBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Check that a built ryeosd carries the Cargo features a bundle set needs.

External development host tool; it grants no RyeOS authority. A plain
`cargo build -p ryeosd` silently drops the features a bundle set's services
need (scheduling, model-provider-setup, ...), and the node then stops at
startup on the first service with no compiled handler. This reads the
daemon's own `ryeosd build-info --json` (its `compiled_features`) and compares
it with the selection the signed payload-ownership item derives for the set
(`scripts/pkg/bundle-features.py`, the source populate and install use).

  featured_build_check.py [--ryeosd PATH | --installed | --built]
                          [--node-profile P | --bundle-set S | --bundles a,b]
                          [--worktree DIR] [--config-file F] [--json]

Without a daemon option it checks the installed daemon from
`config:development/ryeos/dev-node`; without a selection, that config's
node profile. Exit 0 when every needed feature is compiled, 1 naming the
missing ones, 2 when it cannot tell.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
from pathlib import Path
import sys

_spec = importlib.util.spec_from_file_location(
    "dev_host", Path(__file__).resolve().parent / "dev_host.py")
dev_host = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(dev_host)
ToolError = dev_host.ToolError


def check(worktree: Path, ryeosd: Path, *, node_profile: str | None = None,
          bundle_set: str | None = None, bundles: list[str] | None = None) -> dict:
    if node_profile:
        bundle_set = dev_host.profile_bundle_set(worktree, node_profile)
    if bundle_set:
        needed = dev_host.daemon_features_for(worktree, set_name=bundle_set)
        selection = {"bundle_set": bundle_set}
    else:
        needed = dev_host.daemon_features_for(worktree, bundles=bundles)
        selection = {"bundles": bundles}
    if node_profile:
        selection["node_profile"] = node_profile
    info = dev_host.build_info(ryeosd)
    compiled = set(info["compiled_features"])
    return {
        "ok": needed <= compiled,
        "ryeosd": str(ryeosd),
        "revision": info.get("revision"),
        "profile": info.get("profile"),
        **selection,
        "needed": sorted(needed),
        "compiled": sorted(compiled),
        "missing": sorted(needed - compiled),
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    daemon = parser.add_mutually_exclusive_group()
    daemon.add_argument("--ryeosd", type=Path, help="the daemon binary to check")
    daemon.add_argument("--installed", action="store_true", help="the dev node's installed daemon (default)")
    daemon.add_argument("--built", action="store_true", help="the worktree's built daemon")
    selection = parser.add_mutually_exclusive_group()
    selection.add_argument("--node-profile")
    selection.add_argument("--bundle-set")
    selection.add_argument("--bundles", help="comma-separated bundle names")
    parser.add_argument("--worktree")
    parser.add_argument("--config-file", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    try:
        worktree = dev_host.worktree_argument(args.worktree)
        node = None
        if args.ryeosd is None or not (args.node_profile or args.bundle_set or args.bundles):
            node = dev_host.load_dev_node(args.config_file, worktree)
        ryeosd = args.ryeosd or (node.built("ryeosd") if args.built else node.installed("ryeosd"))
        bundles = [b for b in (args.bundles or "").split(",") if b] or None
        profile = args.node_profile
        if not (profile or args.bundle_set or bundles):
            profile = node.node_profile
        result = check(worktree, ryeosd, node_profile=profile, bundle_set=args.bundle_set, bundles=bundles)
    except ToolError as error:
        print(f"featured-build-check: {error}", file=sys.stderr)
        return 2
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        what = result.get("bundle_set") or ",".join(result.get("bundles") or [])
        print(f"{result['ryeosd']} ({result['profile']}, {result['revision']}) for {what}: "
              f"needs {result['needed'] or 'no features'}, compiled {result['compiled'] or 'none'}")
        if result["missing"]:
            print(f"FAIL missing ryeosd feature(s) {result['missing']}; rebuild with "
                  f"--features {','.join('ryeosd/' + f for f in result['missing'])} "
                  f"(or the set's full selection)")
        else:
            print("ok")
    return 0 if result["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
