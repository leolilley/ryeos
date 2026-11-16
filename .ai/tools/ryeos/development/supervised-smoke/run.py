#!/usr/bin/env python3
# ryeos:signed:2026-11-16T07:32:54Z:f05d4d00b936866702acaf05c86252d7fa6b512633e1a338c47a401f7594197f:8FpcRO2Nr1aX7VjbENnG4Ek4kHyzs0yJn2F5btCw8/QTTGL1OrlYHzLa2uz/D9XyWEhDmNswzh95GHx0Z9UNAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/supervised-smoke
#   version: "1.0.0"
#   description: Smoke a running supervised node and print a pass/fail table
#   executor_id: tool:ryeos/core/runtimes/python/script
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   host_authority: required
#   config_schema:
#     type: object
#     properties:
#       app_root:
#         type: string
#         description: Absolute app root of the running node to smoke
#       ryeos_bin:
#         type: string
#         description: Absolute path of the node's ryeos binary
#       manual_directives:
#         type: string
#         description: Absolute path of tests/e2e/manual-directives
#       allow_restart:
#         type: boolean
#         description: Also stop the node, run a standalone op, start it again
#       paid:
#         type: boolean
#         description: Also run the DeepSeek force_continue directive (a few cents)
#     required: [app_root, ryeos_bin]
#     additionalProperties: false

"""Node entry for supervised-smoke: a host-authority Tool.

It shells out to the node's `ryeos`, so it runs on the host under an exact
operator grant. The plain way to run it is outside the node, as the operator,
with `python3 -I lib/supervised_smoke.py --app-root <root>`; see
`.ai/knowledge/ryeos/development/supervised-smoke.md`.
`lib/supervised_smoke.py` stays the one implementation.
"""

from pathlib import Path
import json
import sys

MAX_REQUEST_BYTES = 65536


def absolute(request: dict, key: str) -> str:
    value = request.get(key)
    if not isinstance(value, str) or not Path(value).is_absolute():
        raise ValueError(f"{key} must be an absolute path")
    return value


if __name__ == "__main__":
    try:
        raw = sys.stdin.buffer.read(MAX_REQUEST_BYTES + 1)
        if len(raw) > MAX_REQUEST_BYTES:
            raise ValueError("request is too large")
        request = json.loads(raw or b"{}")
        if not isinstance(request, dict):
            raise ValueError("request is not a JSON object")
        argv = ["--app-root", absolute(request, "app_root"),
                "--ryeos-bin", absolute(request, "ryeos_bin")]
        if request.get("manual_directives") is not None:
            argv += ["--manual-directives", absolute(request, "manual_directives")]
        for key, flag in (("allow_restart", "--allow-restart"), ("paid", "--paid")):
            if request.get(key) is True:
                argv.append(flag)
    except (ValueError, json.JSONDecodeError) as error:
        print(f"supervised-smoke: {error}", file=sys.stderr)
        sys.exit(2)
    sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))
    import supervised_smoke
    sys.exit(supervised_smoke.main(argv))
