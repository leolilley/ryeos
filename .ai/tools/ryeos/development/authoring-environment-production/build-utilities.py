# ryeos:signed:2026-09-08T10:36:31Z:4601a10ca4a52ccf7df567f7596c86a0c4332cd4a5078a1e7c363d8e7e09795c:M0N/GIP92y2QY25BcL8bnHiUfirCnPm8S5ZS0ps6anV7LdMaZvJpGu7H3K9VZSXR7peOBoMHVLNuHhbFuD5tAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
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
#       digest: a6b454a400b503830c71767a91abf649d61979ad1031ffe9840277b3d06fb616
#       mount_root: execution_runtime
#       mount: authoring-source-inputs

from utility_production import main

if __name__ == "__main__":
    main()
