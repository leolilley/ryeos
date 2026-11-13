#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:47:49Z:28ab824744e4d1eaa8b82c589f710bd2d296ecc23265c394456a5bc445d370a0:QPV3hqYbQAYNse5hc9afQlIbH7qVZZXyOHuMzNC8apR/S9VGkOE8uIqVS3WvpdklFb9rp8KnkOeakf4w4JU3AQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/dev-host
#   version: "1.0.0"
#   description: Check a ryeosd binary was compiled with the features a bundle set needs, on the host
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   host_authority: required
#   config_resolve:
#     type: single
#     spec:
#       path: development/ryeos/dev-node.yaml
#       mode: first_match
#   config_schema:
#     type: object
#     properties:
#       worktree:
#         type: string
#         description: Absolute path of the worktree
#       daemon:
#         type: string
#         enum: [installed, built]
#         description: The dev node's installed daemon (default) or the worktree's built one
#       ryeosd:
#         type: string
#         description: Absolute path of a daemon binary to check instead
#       node_profile:
#         type: string
#       bundle_set:
#         type: string
#       bundles:
#         type: array
#         items:
#           type: string
#     required: [worktree]
#     additionalProperties: false

"""Node entry for featured-build-check: a host-authority Tool.

It runs installed or built host binaries (`ryeosd build-info`) and the
repository's bundle-features script, so it runs on the host under an operator
grant. `lib/featured_build_check.py` stays the one implementation.
"""

from pathlib import Path
import importlib.util
import sys


def _load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


if __name__ == "__main__":
    here = Path(__file__).resolve().parent
    entry = _load(here / "lib/node_entry.py", "node_entry")
    try:
        request = entry.read_request()
        argv = ["--json", "--config-file", str(entry.config_file(request)),
                "--worktree", entry.absolute(request.get("worktree"), "worktree")]
        if request.get("ryeosd") is not None:
            argv += ["--ryeosd", entry.absolute(request["ryeosd"], "ryeosd")]
        elif request.get("daemon") == "built":
            argv.append("--built")
        else:
            argv.append("--installed")
        entry.option(argv, request, "node_profile", "--node-profile")
        entry.option(argv, request, "bundle_set", "--bundle-set")
        if request.get("bundles"):
            argv += ["--bundles", ",".join(request["bundles"])]
    except (entry.EntryError, ValueError) as error:
        entry.refuse(error, "featured-build-check")
    sys.path.insert(0, str(here / "lib"))
    import featured_build_check
    entry.run(featured_build_check.main, argv)
