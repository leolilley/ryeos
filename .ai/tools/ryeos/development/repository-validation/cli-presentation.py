# ryeos:signed:2026-09-07T08:39:47Z:7dda7a9597ac883fda056e858725857d281233c8844bd0efb1ec36d723da6430:DG7T1fmeWYedh+Msu3Z8wygu+J4+oCj9x3L1OMf2Q2HNBhG4lh+aQzTWgHuUD7ncUBKm7ObdAQ0gvxVxm2XAAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/repository-validation
#   version: "1.0.0"
#   description: Check CLI terminal writes use their owning renderers
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

# Reuse the already admitted sealed-source Python runtime. Tool ownership does
# not grant this operation to a worker; its request remains unchanged.
from pathlib import Path
import sys

if __name__ == "__main__":
    # Exact adjacent source is sealed by RyeOS; external CI uses the same code.
    sys.path.insert(0, str(Path(__file__).parent / "lib"))
    from validation import run
    run("cli-presentation")
