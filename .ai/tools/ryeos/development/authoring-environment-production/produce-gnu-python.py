# ryeos:signed:2026-09-17T05:42:27Z:a4a30dc19828559db82163d8d1373d2123f39858053c2ac19b2d4b9cf75c8a36:LCCDmgsF9y2cGbvK0QWGQCp1F+QhDGLmRjyGbd+2gNdKqYsCpqs8jSs506lqZWzYzZNc1FSdjAu0tcUKIP2CBA==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96
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
#     properties: {}
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
