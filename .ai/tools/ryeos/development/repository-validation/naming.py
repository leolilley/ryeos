# ryeos:signed:2026-09-17T06:36:51Z:cb6ed6ea6c35b9f3898fe615466b566167b36536e0cdb4c32e2ff18e86e0b337:SNPklZUTx+5QwS04HUqzE+1qiRhSnw9/QKy4LOrAmzF0Huo6lVfswhDK/LCZFa/VHtTRkPMMtadAwPrUjspxDw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96
# ryeos-tool:
#   category: ryeos/development/repository-validation
#   version: "1.0.0"
#   description: Check canonical repository vocabulary and scope examples
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
#     type: single
#     spec:
#       path: development/ryeos/repository-validation.yaml
#       mode: first_match
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python

# Reuse the already admitted sealed-source Python runtime. Tool ownership does
# not grant this operation to a worker; its request remains unchanged.
from pathlib import Path
import sys

if __name__ == "__main__":
    # Exact adjacent source is sealed by RyeOS; external CI uses the same code.
    sys.path.insert(0, str(Path(__file__).parent / "lib"))
    from validation import run
    run("naming")
