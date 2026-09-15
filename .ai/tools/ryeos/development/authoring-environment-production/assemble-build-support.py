# ryeos:signed:2026-09-07T07:43:46Z:f9163739f1b595212c7a2f106eec2f08cb82017f920ce1d69a515732bbc5845a:luhsIjfvqSEiTCpwY6KNGHffgt8yG552dVtD1VsTNTWWpMyuJdlAtQRljZ0l8OXaZLo7XJ7Q/dUAu7KrHOweCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
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
