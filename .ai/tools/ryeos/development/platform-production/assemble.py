# ryeos:signed:2026-09-22T00:24:35Z:66d7b8432bd8062edc6712703e9e728af00c0a2627643b3535847f8df76f15cf:nykzVSrH5revd0IGp6WTWk8/nvzGMooafO0rwnjee/DILA1urMbK8K4aQBZ4J/SGJBBEZisOAWzDrJLSoDmxAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/platform-production
#   version: "1.0.1"
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
#       digest: b74b15f0877a2c4941e8bb5660b9f827091aeeb2b2a50b00311101c903895c00
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
