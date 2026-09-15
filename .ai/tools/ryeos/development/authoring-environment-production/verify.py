# ryeos:signed:2026-09-08T10:36:31Z:b0530e30a4840f7dbe2bd7123c6f0c9b4dde60d3660f57e6a08328667b0ee23e:Xvr8rgu7bEzwMx2YW0AOTeW99lDRE7fn3nJdL8mfJ+Ef6r7xVD20xdNWs/8DuZEWqwsYelvSwiYrIn4PsM8zCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/authoring-environment-production
#   version: "1.0.0"
#   description: Independently reproduce and compare the retained authoring environment
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
#       - path: development/ryeos/authoring-build-support.yaml
#         mode: first_match
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#   # assembly-inputs is selected by the enclosing producer Graph and reaches
#   # this inline Tool through the admitted normalized realization set.
#   # built-utilities is selected by that same Graph. Its exact product
#   # manifest, not this source file, supplies the produced command bytes.

from production import run_operation

if __name__ == "__main__":
    run_operation("verify")
