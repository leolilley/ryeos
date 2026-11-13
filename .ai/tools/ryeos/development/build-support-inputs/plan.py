#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:25:04Z:fe467fb370045da1bd6b6cafc9977d4bc7896caf7bb8c11a62da5de9f5728a9e:kvUaa64oxfLVsQbxDvlYZeb78UvD7uAuctF6LuLEcUjfm/3ekKcbl7VpTESk3ViKtQB3tPNbZmQjPBTTK5cuAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/build-support-inputs
#   version: "1.0.0"
#   description: Print the plan for rebuilding the pinned build-support-inputs tree, read from the project's pins
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false

"""Ordinary sandboxed Tool for the build-support-inputs dry run.

It reads repository YAML from the project and prints the rebuild plan; no
docker, no network, no host access. The rebuild itself is the host-authority
`execute` Tool beside it.
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
        entry.read_request()
        project = entry.project_path()
        if project is None:
            raise entry.EntryError("the runtime passed no project path")
        argv = ["--dry-run", "--repository-root", str(project)]
    except (entry.EntryError, ValueError) as error:
        entry.refuse(error, "build-support-inputs")
    rebuild = _load(here / "lib/rebuild-build-support-inputs.py", "rebuild_build_support_inputs")
    entry.run(rebuild.main, argv)
