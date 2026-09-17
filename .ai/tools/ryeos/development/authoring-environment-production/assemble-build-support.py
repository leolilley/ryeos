# ryeos:signed:2026-09-17T05:42:27Z:f9163739f1b595212c7a2f106eec2f08cb82017f920ce1d69a515732bbc5845a:z76fhLksduVjPaHLiH1CfX8vUcfEqAxSp6fwojnqs6DMYjxTvUNpYxXGsUN7aK9UtPxAkfFhvZu6r9OXanauAw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96
# ryeos-tool:
#   category: ryeos/development/authoring-environment-production
#   version: "1.0.0"
#   description: Assemble exact shell and Make support from admitted immutable inputs
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
#       path: development/ryeos/authoring-build-support-inputs.yaml
#       mode: first_match
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#     - id: build-support-inputs
#       kind: tree
#       mode: pinned
#       digest: 8e24fb2c2adb1fd4e79ee19b313811906cba6c634b6eb574f0d7f202c7c8a222
#       mount_root: execution_runtime
#       mount: build-support-inputs

from build_support import run_support_assembly

if __name__ == "__main__":
    run_support_assembly()
