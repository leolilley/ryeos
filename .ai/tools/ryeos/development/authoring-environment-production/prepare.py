# ryeos:signed:2026-09-17T05:41:43Z:d05ae3a109743cd8b7606f95f2366f59e88c96320ead6786b3bb7772155f422c:Hs6JQMcB8U5iY1r9LSBFcfY5gQj5NscOC+rVKNd0QeDtbkTV295vwGYpFiKEmdyEHoaLhdvUBdHd0rQtFVP5Cw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96
# ryeos-tool:
#   category: ryeos/development/authoring-environment-production
#   version: "1.0.0"
#   description: Prepare the exact authoring assembly inputs from admitted source archives
#   executor_id: tool:ryeos/development/authoring-environment-production/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false
#   config_resolve:
#     type: multi
#     specs:
#       - path: development/ryeos/authoring-environment-inputs.yaml
#         mode: first_match
#       - path: development/ryeos/authoring-utility-sources.yaml
#         mode: first_match
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#     - id: source-inputs
#       kind: tree
#       mode: pinned
#       digest: c66ac1c984e0793416106cd2fefa1a45d1b00c17b5865ff751e41594a3d39857
#       metadata_hint: ryeos-authoring-source-inputs-v2-large-content
#       mount_root: execution_runtime
#       mount: authoring-source-inputs

from preparation import run_preparation

if __name__ == "__main__":
    run_preparation()
