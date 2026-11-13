#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:25:04Z:be3bbf4aeca2c33b620ea835d5b3908fcb345f6d5b06a6678aec3aeaa006d5d8:gjbMmM3W186vHVkPiy+pVSEsSCmY0M4H2gieqivA/jWL0FvC56nKnFhzBKowv6uw9DOrC2TSyzHSq7Ec7k/6Aw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/guarded-cargo
#   version: "1.0.0"
#   description: Run one cargo command for a worktree under the machine's shared build guard, on the host
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   host_authority: required
#   config_resolve:
#     type: single
#     spec:
#       path: development/ryeos/guarded-cargo.yaml
#       mode: first_match
#   config_schema:
#     type: object
#     properties:
#       worktree:
#         type: string
#         description: Absolute path of the worktree cargo runs in
#       mode:
#         type: string
#         enum: [run, status, prune_only]
#         description: run (default) runs cargo_args; status prints the guard state; prune_only prunes the target dir
#       cargo_args:
#         type: array
#         items:
#           type: string
#         description: The cargo arguments, for example [test, -p, ryeos-engine]
#       log:
#         type: string
#         description: Absolute file the cargo output and guard messages are appended to
#       timeout:
#         type: integer
#         description: Seconds the node lets the run take (the runtime default is 300)
#     required: [worktree]
#     additionalProperties: false

"""Node entry for guarded-cargo: a host-authority Tool.

It takes the machine's build lock, scans host processes and disks, and runs
the host cargo toolchain, so it runs on the host under an operator grant.
`lib/guarded_cargo.py` stays the one implementation; external use is
unchanged.
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
    entry = _load(here.parent / "dev-host/lib/node_entry.py", "node_entry")
    try:
        request = entry.read_request()
        argv = ["--config-file", str(entry.config_file(request)),
                "--worktree", entry.absolute(request.get("worktree"), "worktree")]
        if request.get("log") is not None:
            argv += ["--log", entry.absolute(request["log"], "log")]
        mode = request.get("mode", "run")
        if mode == "status":
            argv.append("--status")
        elif mode == "prune_only":
            argv.append("--prune-only")
        else:
            argv += ["--", *request.get("cargo_args", [])]
    except (entry.EntryError, ValueError) as error:
        entry.refuse(error, "guarded-cargo")
    sys.path.insert(0, str(here / "lib"))
    import guarded_cargo
    entry.run(guarded_cargo.main, argv)
