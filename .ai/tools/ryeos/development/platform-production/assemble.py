# ryeos:signed:2026-09-17T06:36:50Z:a14d06fed3198ea167f63a02df81e039585295becac3bc88b2ef07ec86e21c91:d/Hyq8FmzUuPVxmqHxnbX6CUSzEz4qU9IPLYKd1X7MyBXtxFhXs5vp3xtqqLSOd+Hmc35pQFdNwXPLanctmCAA==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96
# ryeos-tool:
#   category: ryeos/development/platform-production
#   version: "1.0.0"
#   description: Reproduce the exact admitted development platform as a retained product
#   executor_id: tool:ryeos/development/authoring-environment-production/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   workspace_access: immutable_current_generation
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#     - id: platform
#       kind: tree
#       mode: pinned
#       digest: 98bceddd5b4024d5963eeac8c579e6d4e79c24577980fa9f88bce9ae3151d316
#       mount_root: execution_runtime
#       mount: platform

"""Offline reproduction of the admitted platform; acquisition remains external."""
import json
from pathlib import Path
import shutil
import sys

SOURCE = Path("/ryeos/realizations/platform")
OUTPUT = Path("products/development-platform")


def main():
    if len(sys.argv) != 3 or sys.argv[1] != "--project-path":
        raise ValueError("missing admitted project context")
    project = Path(sys.argv[2])
    if not project.is_absolute():
        raise ValueError("project context must be absolute")
    request = sys.stdin.buffer.read(65537)
    if len(request) > 65536:
        raise ValueError("platform production request exceeds its bound")
    body = json.loads(request)
    if body.get("resolved_config") not in ({}, None):
        raise ValueError("platform production accepts no resolved configuration")
    if not SOURCE.is_dir() or SOURCE.is_symlink():
        raise ValueError("admitted platform root is not an ordinary directory")
    destination = project / OUTPUT
    if destination.exists() or destination.is_symlink():
        raise ValueError("platform product destination already exists")
    destination.parent.mkdir(mode=0o700, exist_ok=True)
    shutil.copytree(SOURCE, destination, symlinks=True)
    print(json.dumps({"ok": True, "output_path": OUTPUT.as_posix(),
                      "network_acquisition": False, "binding_published": False},
                     sort_keys=True))


if __name__ == "__main__":
    main()
