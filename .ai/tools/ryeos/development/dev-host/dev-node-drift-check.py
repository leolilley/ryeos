#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:47:49Z:8b3b4e0d5ecace4169213f5b53e09eeace91852e20c32dfd6d0e361b0a5efa8f:XyDz0AsifAsiyGQx0imykUmVUGdpbY2DfGIHaaa6KzNdWkW1EhnEHWgCA3EQmUNe2v+2zPKFM0+KDqEa4RTICg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/dev-host
#   version: "1.0.0"
#   description: Compare the dev node's installed binaries and bundles with a worktree, on the host
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
#       app_root:
#         type: string
#         description: Absolute app root to compare (default from the dev-node Config)
#       bin_dir:
#         type: string
#         description: Absolute directory of the installed binaries
#       bundles:
#         type: array
#         items:
#           type: string
#     required: [worktree]
#     additionalProperties: false

"""Node entry for dev-node-drift-check: a host-authority Tool.

It reads installed binaries, a node's own app root and git in the worktree, so
it runs on the host under an operator grant. `lib/dev_node_drift_check.py`
stays the one implementation.
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
        if request.get("app_root") is not None:
            argv += ["--app-root", entry.absolute(request["app_root"], "app_root")]
        if request.get("bin_dir") is not None:
            argv += ["--bin-dir", entry.absolute(request["bin_dir"], "bin_dir")]
        if request.get("bundles"):
            argv += ["--bundles", ",".join(request["bundles"])]
    except (entry.EntryError, ValueError) as error:
        entry.refuse(error, "dev-node-drift-check")
    sys.path.insert(0, str(here / "lib"))
    import dev_node_drift_check
    entry.run(dev_node_drift_check.main, argv)
