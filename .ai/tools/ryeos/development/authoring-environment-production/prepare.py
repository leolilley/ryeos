# ryeos:signed:2026-09-08T10:36:31Z:1db4f0089861b3e8e0bed4cd5f78ca036cf05773db3d4291426133a733d5e621:O1OJAjmXx4jXXY2ovp3tkgxxjXqXn0LJiS54zQwmcqNA+Pl/74o5dc65KqJUDCdzS/QorH3qX7e3iWKIWSL5BA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
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
#       digest: a6b454a400b503830c71767a91abf649d61979ad1031ffe9840277b3d06fb616
#       metadata_hint: ryeos-authoring-source-inputs-v1-large-content
#       mount_root: execution_runtime
#       mount: authoring-source-inputs

from preparation import run_preparation

if __name__ == "__main__":
    run_preparation()
