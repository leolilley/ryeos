#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:47:49Z:d04d9a606aa1fab2eec33ea036e8a7970ee16679761b64766b540a5b7b3a4da3:GBuFNqbczscJ1Ve6PqVPgmZOmvZy24omISGbAWudIuJ62eJ3iRIttnFriumgQZoPLicYyFA9AClDp4ylMHG4Bw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/dev-host
#   version: "1.0.0"
#   description: Initialize a throwaway node from a bundle set and optionally start it, on the host
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
#       cli:
#         type: string
#         enum: [built, installed]
#         description: The worktree's built ryeos CLI (default) or the dev node's installed one
#       ryeos:
#         type: string
#         description: Absolute path of a ryeos CLI to init with instead
#       node_profile:
#         type: string
#       source:
#         type: string
#         description: Absolute bundle source dir (default <worktree>/bundles)
#       trust_file:
#         type: string
#       start:
#         type: boolean
#         description: Also start the node and wait for ready
#       keep:
#         type: boolean
#         description: Keep the temporary app root and logs
#       timeout:
#         type: integer
#         description: Seconds the node lets the run take (the runtime default is 300)
#     required: [worktree]
#     additionalProperties: false

"""Node entry for bundle-set-init-smoke: a host-authority Tool.

It runs `ryeos init` into a scratch app root and, with `start`, spawns a
throwaway `ryeosd`, so it runs on the host under an operator grant; a
process-scope grant makes a crashed smoke node die with the run.
`lib/bundle_set_init_smoke.py` stays the one implementation.
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
        if request.get("ryeos") is not None:
            argv += ["--ryeos", entry.absolute(request["ryeos"], "ryeos")]
        elif request.get("cli") == "installed":
            argv.append("--installed")
        entry.option(argv, request, "node_profile", "--node-profile")
        if request.get("source") is not None:
            argv += ["--source", entry.absolute(request["source"], "source")]
        if request.get("trust_file") is not None:
            argv += ["--trust-file", entry.absolute(request["trust_file"], "trust_file")]
        entry.flag(argv, request, "start", "--start")
        entry.flag(argv, request, "keep", "--keep")
    except (entry.EntryError, ValueError) as error:
        entry.refuse(error, "bundle-set-init-smoke")
    sys.path.insert(0, str(here / "lib"))
    import bundle_set_init_smoke
    entry.run(bundle_set_init_smoke.main, argv)
