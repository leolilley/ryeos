# ryeos:signed:2026-09-11T10:27:02Z:d1a9afd9f363d799f0feb8f0937ac1f5c5a1734bb9db254c94903175dc876731:fHcZwgJ+daSc23xqaFLJ5T1QuwDyu6fMAEqTyN7D0nWYgxpeu4TN0H/aPAYcYFk/NRX2TxMXoBskfe3Es6XyCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/registry-production
#   version: "1.0.1"
#   description: Verify locked retained registry inputs and assemble a private local-registry result
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
#   config_resolve:
#     type: multi
#     specs:
#       - path: development/ryeos/registry-acquisition.yaml
#         mode: first_match
#       - path: development/ryeos/registry-production.yaml
#         mode: first_match
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#     - id: registry-inputs
#       kind: tree
#       mode: pinned
#       digest: de232765fc09f88197eab4a247147a207a410d4cab73e112e425926c12d708f7
#       mount_root: execution_runtime
#       mount: registry-inputs

"""Offline production only; no acquisition transport, Cargo or worker grant."""
import hashlib
import json
from pathlib import Path, PurePosixPath
import sys

from registry_inputs import RetainedInputs, assemble, bounded_file, ordinary_member, validate_config

INPUT_CONFIG = "development/ryeos/registry-acquisition.yaml"
PRODUCTION_CONFIG = "development/ryeos/registry-production.yaml"
INPUT_ROOT = Path("/ryeos/realizations/registry-inputs")


def main():
    if len(sys.argv) != 3 or sys.argv[1] != "--project-path":
        raise ValueError("missing admitted project context")
    project = Path(sys.argv[2])
    if not project.is_absolute():
        raise ValueError("project context must be absolute")
    raw = sys.stdin.buffer.read(65537)
    if len(raw) > 65536:
        raise ValueError("registry production request exceeds its bound")
    resolved = json.loads(raw)["resolved_config"]
    if set(resolved) != {INPUT_CONFIG, PRODUCTION_CONFIG}:
        raise ValueError("missing exact registry Config selections")
    config, production = resolved[INPUT_CONFIG], resolved[PRODUCTION_CONFIG]
    validate_config(config)
    if (not isinstance(production, dict) or set(production) != {
            "category", "name", "version", "schema", "lock_file", "output"} or
            production["category"] != "development/ryeos" or production["name"] != "registry-production" or
            production["version"] != "1.0.0" or production["schema"] != "ryeos.development.registry-production.v1"):
        raise ValueError("invalid registry production Config")
    lock = bounded_file(ordinary_member(project, production["lock_file"]),
                        config["limits"]["max_input_file_bytes"])
    output = PurePosixPath(production["output"])
    if (output.is_absolute() or output.as_posix() != production["output"] or
            len(output.parts) != 2 or output.parts[0] != "products" or output.parts[1] in (".", "..")):
        raise ValueError("registry output must be one selected products child")
    parent = project / output.parent
    if not parent.exists():
        parent.mkdir(mode=0o700)
    ordinary_member(project, output.parent.as_posix(), directory=True)
    assemble(lock, config, project / output, RetainedInputs(INPUT_ROOT, lock, config),
             source_kind="admitted_retained_inputs")
    receipt = bounded_file(project / output / "registry-inputs.json", 2 * 1024 * 1024)
    print(json.dumps({"ok": True, "output_path": output.as_posix(),
                      "receipt_sha256": hashlib.sha256(receipt).hexdigest(),
                      "packages": len(json.loads(receipt)["packages"]),
                      "network_acquisition": False, "binding_published": False}, sort_keys=True))


if __name__ == "__main__":
    main()
