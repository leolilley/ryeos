# ryeos:signed:2026-09-17T05:41:43Z:b75493f55c40a2fe6c99120be6ed4eea7c2d0b2579bc82e24c0c0a5f69010549:+rYcu2oUbgpPUsF/XPwvG3JKBlj41OmxsrIdoYSnhLXq5q9ex1i1sCauryU1X+1cCDrGET/4ZfgzHgOfna0eDA==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96
# ryeos-tool:
#   category: ryeos/development/authoring-environment-production
#   version: "1.0.0"
#   description: Build the finite authoring utilities from exact sources using admitted Stage0 and support
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
#       - path: development/ryeos/authoring-build-support.yaml
#         mode: first_match
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
#   # authoring-build-support is selected by the enclosing producer Graph and
#   # reaches this inline Tool through the admitted normalized realization set.
#     - id: platform
#       kind: tree
#       mode: pinned
#       digest: 98bceddd5b4024d5963eeac8c579e6d4e79c24577980fa9f88bce9ae3151d316
#       mount_root: execution_runtime
#       mount: platform
#     - id: source-inputs
#       kind: tree
#       mode: pinned
#       digest: c66ac1c984e0793416106cd2fefa1a45d1b00c17b5865ff751e41594a3d39857
#       mount_root: execution_runtime
#       mount: authoring-source-inputs

from utility_production import main

if __name__ == "__main__":
    main()
