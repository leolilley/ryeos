#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:47:49Z:a4ce7d8310ede4bf3bda0bb105e54d6b9b00f25ef9ea6bce7111035a8fd8083c:kXnYghdyWwV1MmsCNtvVEmo1HxWuCwY/pFE8kYVW4ppoQKNTsZxZ5FF6U5PJ2pIcFiZ0NCZN99yvi00k7KQhDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/dev-host
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
runs under an operator grant. The report check is the ordinary `contained-oci-provider-report` Tool.
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
        container = request.get("container")
        if not isinstance(container, str) or not container or container.startswith("-"):
            raise entry.EntryError("container must name a docker container")
        argv = ["--container", container, "--out", entry.absolute(request.get("out"), "out")]
    except (entry.EntryError, ValueError) as error:
        entry.refuse(error, "contained-oci-provider")
    prove = _load(here / "lib/prove-contained-oci-provider.py", "prove_contained_oci_provider")
    entry.run(prove.main, argv)
