# ryeos:signed:2026-09-22T00:24:35Z:8d260728e744b2b3fbcfd0e89e4c5b80d08b2e45cbe418038b711e45f87cd80d:5TiwYLMeqQKNddSvpGHz+KH1I0rGvyJvAOuTEE7CgSxFcQ/u5LbAuNz7bJLLGTYAJ0BuJ106+lyXtnc60anhCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/authoring-environment-production
#   version: "1.1.0"
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
#   # authoring-build-support and the qualified platform product are selected
#   # by the enclosing producer Graph and reach this inline Tool through its
#   # admitted normalized realization set. Stage-0 is not bound here.
#     - id: source-inputs
#       kind: tree
#       mode: pinned
#       digest: c66ac1c984e0793416106cd2fefa1a45d1b00c17b5865ff751e41594a3d39857
#       mount_root: execution_runtime
#       mount: authoring-source-inputs

from utility_production import main

if __name__ == "__main__":
    main()
