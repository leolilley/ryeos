#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:25:04Z:c3c6b6db5f5b0ba90ab0fe0c7b6065467210b4fc9571812d30a6123883799f3d:P7py/cGzz0+euoy0qEQFsXUGOQYzrDWd+VfVlzRlEcI6Hiuy+cW7a3ka2Ad4DoFaJTygz0U0N/3YafSnBYdTDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/contained-oci-provider
#   version: "1.0.0"
#   description: Prove a running contained node's process-scope proofs come from the OCI provider, via docker exec on the host
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   host_authority: required
#   config_schema:
#     type: object
#     properties:
#       container:
#         type: string
#         description: The running contained node's docker container
#       out:
#         type: string
#         description: Absolute directory the evidence is written to
#     required: [container, out]
#     additionalProperties: false

"""Node entry for contained-oci-provider `--container`: a host-authority Tool.

It runs `docker exec <container> ryeos node doctor --json` on the host, so it
runs under an operator grant. The report check is the ordinary `report` Tool.
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
        container = request.get("container")
        if not isinstance(container, str) or not container or container.startswith("-"):
            raise entry.EntryError("container must name a docker container")
        argv = ["--container", container, "--out", entry.absolute(request.get("out"), "out")]
    except (entry.EntryError, ValueError) as error:
        entry.refuse(error, "contained-oci-provider")
    prove = _load(here / "lib/prove-contained-oci-provider.py", "prove_contained_oci_provider")
    entry.run(prove.main, argv)
