#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:25:04Z:91bf77954b85483477c539bb3a0522162fb821e3f7fb22482d63cb7293337120:uZiA35mG1e2U79Ws4KHMuw/6xLegFEa3JW+A9HwIZ2+ZEk00PVv5yxHCYScOOlPpVMnvX8+HqXK6LgAt6l4/Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/contained-oci-provider
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
    entry = _load(here.parent / "dev-host/lib/node_entry.py", "node_entry")
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
