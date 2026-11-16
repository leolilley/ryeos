#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:47:49Z:b74170038707ba6e8566faa40ff8e12d59289720b8c2f4e960add8919591e32b:NebZG2rtJANkExLxKnzI/stVhmjVnsaTOfSdAw804adP8QwiRPn701f6YqQUmg/tr9s9bbWCV4V5yMOPcYssAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/dev-host
#   version: "1.0.0"
#   description: Rebuild the pinned build-support-inputs tree with docker and the network, on the host
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   host_authority: required
#   config_schema:
#     type: object
#     properties:
#       worktree:
#         type: string
#         description: Absolute path of the worktree that holds the pins
#       output:
#         type: string
#         description: Absolute new directory for the rebuilt tree
#       scratch:
#         type: string
#         description: Absolute parent for the archive download directories
#       keep_scratch:
#         type: boolean
#       timeout:
#         type: integer
#         description: Seconds the node lets the run take (the runtime default is 300)
#     required: [worktree, output]
#     additionalProperties: false

"""Node entry for build-support-inputs `--execute`: a host-authority Tool.

It runs `docker cp` from the pinned image (root-equivalent docker access) and
fetches pinned archives over the network, so it runs on the host under an
operator grant. The dry run is the ordinary `build-support-inputs-plan` Tool beside it.
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
        argv = ["--execute",
                "--repository-root", entry.absolute(request.get("worktree"), "worktree"),
                "--output", entry.absolute(request.get("output"), "output")]
        if request.get("scratch") is not None:
            argv += ["--scratch", entry.absolute(request["scratch"], "scratch")]
        entry.flag(argv, request, "keep_scratch", "--keep-scratch")
    except (entry.EntryError, ValueError) as error:
        entry.refuse(error, "build-support-inputs")
    rebuild = _load(here / "lib/rebuild-build-support-inputs.py", "rebuild_build_support_inputs")
    entry.run(rebuild.main, argv)
