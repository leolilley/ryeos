# ryeos:signed:2026-10-26T23:21:33Z:edfecf7dba26898aea4657f5ee789f5d3dd90d8e282739d1d59fd50995d7c1cd:1bLwY4dDftouAN2aYzrCcQrzNlTNaJNjfHZXo53Mol4O1V98g4mekCrKNC30MFBwZZsDufrRTDizcQE/Ob9vCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/repository-validation
#   version: "1.1.0"
#   description: Check migrated product owners delegate host operations to Lillux
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

from pathlib import Path
import sys

if __name__ == "__main__":
    sys.path.insert(0, str(Path(__file__).parent / "lib"))
    from validation import run
    run("host-capability-ownership")
