# ryeos:signed:2026-10-26T23:21:33Z:8f22d893d8364ba155f6fab2b58d3f85e407535c1b68ac2e965bb4b0c5c0b5e4:GxeVcTEfSFK+O9vEO47drjbw5OaffNp9xXxoqXIR86CGlbWKnO4qCtO0DWvG3B5Q5p1HMgy0WgpXwSuIPCOUAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/authoring-environment-production
#   version: "1.0.0"
#   description: Produce the exact relocatable GNU CPython distribution
#   executor_id: tool:ryeos/development/authoring-environment-production/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties:
#       profile: {type: string, enum: [fixed_absolute_v1, private_origin_v1]}
#     additionalProperties: false
#   config_resolve:
#     type: single
#     spec:
#       path: development/ryeos/gnu-python-production-inputs.yaml
#       mode: first_match
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#     - id: gnu-python-archives
#       kind: tree
#       mode: pinned
#       digest: ec7d4a2a749b0b709dabca0a7919c3a897a9b671be56332b67a659b6bd82cac6
#       metadata_hint: ryeos-gnu-python-inputs-v1-large-content
#       mount_root: execution_runtime
#       mount: gnu-python-archives
#   # The enclosing Graph owns the selected prepared-input slot. It reaches
#   # this inline Tool through the admitted normalized realization set.

from gnu_python_production import run_production

if __name__ == "__main__":
    run_production()
