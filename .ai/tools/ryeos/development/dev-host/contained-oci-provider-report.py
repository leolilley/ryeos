#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:47:49Z:5d6cfe29851ff3d66e496d58ea024f92a62fc503cb8ab8e8b7dfe3c046f5697d:Ae+HDaO0sjEzL4bHWe8SfBMIYNQQwiZ0xE9yvILGAowKgWx63PJjCotPvRQtaDsxhN+YbX6jjiQadvFHyVRcBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/dev-host
#   version: "1.0.0"
#   description: Check a saved contained node doctor report for OCI-provider process-scope proofs
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties:
#       report:
#         type: string
#         description: Project-relative path of a saved `ryeos node doctor --json` report
#     required: [report]
#     additionalProperties: false

"""Ordinary sandboxed Tool for the contained-oci-provider report check.

A pure function of a saved doctor JSON report in the project: no host access,
no network. Prints the evidence the external `--out` form writes.
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
    import json
    import tempfile
    here = Path(__file__).resolve().parent
    entry = _load(here / "lib/node_entry.py", "node_entry")
    try:
        request = entry.read_request()
        project = entry.project_path()
        report = request.get("report")
        if project is None or not isinstance(report, str) or not report or Path(report).is_absolute() \
                or ".." in Path(report).parts:
            raise entry.EntryError("report must be a project-relative path")
        out = Path(tempfile.mkdtemp(prefix="ryeos-oci-evidence-"))
        argv = ["--report", str(project / report), "--out", str(out)]
    except (entry.EntryError, ValueError) as error:
        entry.refuse(error, "contained-oci-provider")
    prove = _load(here / "lib/prove-contained-oci-provider.py", "prove_contained_oci_provider")
    code = prove.main(argv)
    print((out / "oci-provider-evidence.json").read_text(), end="")
    sys.exit(code)
