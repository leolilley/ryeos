# ryeos:signed:2026-09-17T06:36:50Z:b4312db0f8299b0c77ac7b7bbe0037af81ea9924531ce28512384c1b890b2bd7:cXEK74Nd8OS3LUeaHH2V5y7S6z6tgQE+KPJ6blu4xWf9KhmuV+B7xzwtAQRSUobSYMZRoh0+c2RenEG6aVirCQ==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96
# ryeos-tool:
#   category: ryeos/development/registry-production
#   version: "1.0.2"
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
#       digest: e8f5ded8d3327ed0ce91798f8f0264ac7bf4f120a85936d3a3dc675be7ad2dbe
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
