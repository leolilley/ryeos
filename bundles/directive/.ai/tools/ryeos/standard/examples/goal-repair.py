#!/usr/bin/env python3
# ryeos:signed:2026-11-28T16:39:11Z:03bb52856e45bde0c335edb5a37eb347f418e2c7085e00e1568dba20224ae933:vI6V00ThIV4o/hQ0Q78FI9vNBsnDPObkCfWoT1jjR6WpVatFTrhNuZ6d7c8rpZZwPyi+HoCBr8wrnMgKpssJBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/standard/examples
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Builds the repair text for a list of failed criterion keys from the author's signed repair map. A key that is not in the map ends the run failed with judge_result_invalid."
#   config_schema:
#     type: object
#     properties:
#       failed: {type: array}
#       repair: {type: object}
#     required: [failed, repair]
#     additionalProperties: false
"""Join the signed repair text for each failed key; refuse any key the author did not sign."""
import json
import sys

params = json.loads(sys.stdin.read())
repair = params["repair"]
parts = []
for key in params["failed"]:
    if not isinstance(key, str) or key not in repair or not isinstance(repair[key], str):
        sys.stderr.write("judge_result_invalid: the failed list names a key that is not a signed criterion\n")
        sys.exit(1)
    parts.append(repair[key])
print(json.dumps({"text": " ".join(parts)}))
