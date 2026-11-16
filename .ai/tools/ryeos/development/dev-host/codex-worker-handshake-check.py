#!/usr/bin/env python3
# ryeos:signed:2026-11-13T06:47:49Z:6d4357163df828b1e4b25de6bf4c4dc293261b5f584c2a09238df9abb53cc8b7:Thjrg7QfA9OsTKcQf/UUS5p0EJrujcFTgACSydc5t4/XYS4GZuJS+yBL6B1aRgPv8T8RpiL3rgKqWqmAZ0XaBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/dev-host
#   version: "1.0.0"
#   description: Run a candidate Codex binary through the worker handshake profile, on the host
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   host_authority: required
#   config_schema:
#     type: object
#     properties:
#       worktree:
#         type: string
#         description: Absolute path of the worktree that holds the profile
#       codex:
#         type: string
#         description: Absolute path of the candidate Codex executable
#       profile_dir:
#         type: string
#       profile:
#         type: string
#       handshake_timeout:
#         type: number
#         description: Seconds each handshake step may take
#     required: [worktree, codex]
#     additionalProperties: false

"""Node entry for codex-worker-handshake-check: a host-authority Tool.

It executes an arbitrary candidate Codex binary on the host, so it runs under
an operator grant. `lib/codex_worker_handshake_check.py` stays the one
implementation.
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
        argv = ["--worktree", entry.absolute(request.get("worktree"), "worktree"),
                "--codex", entry.absolute(request.get("codex"), "codex")]
        if request.get("profile_dir") is not None:
            argv += ["--profile-dir", entry.absolute(request["profile_dir"], "profile_dir")]
        entry.option(argv, request, "profile", "--profile")
        entry.option(argv, request, "handshake_timeout", "--timeout")
    except (entry.EntryError, ValueError) as error:
        entry.refuse(error, "codex-worker-handshake-check")
    sys.path.insert(0, str(here / "lib"))
    import codex_worker_handshake_check
    entry.run(codex_worker_handshake_check.main, argv)
