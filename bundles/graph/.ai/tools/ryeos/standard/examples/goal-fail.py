#!/usr/bin/env python3
# ryeos:signed:2026-11-28T14:22:18Z:8b8008a5a43fd17f65881f1690e5500a60b9e33309545509e6b5aeef7d8e8f56:4KWBD5sp+Q4Xzpt+68wwEPcXYNw7rb3VlEiJHORPkLX3Oljl9m05HAPVl8H6lA7dEj2Og/1KM2ScLsywdSpvCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/standard/examples
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Ends the goal-and-judge graph failed with one of three fixed codes. It reads only the code name and a round count."
#   config_schema:
#     type: object
#     properties:
#       code: {type: string, enum: [goal_rounds_exhausted, judge_result_invalid, goal_config_invalid]}
#       rounds: {type: integer}
#     required: [code, rounds]
#     additionalProperties: false
"""Exit non-zero with a typed code, so the graph ends failed and never completed."""
import json
import sys

params = json.loads(sys.stdin.read())
code = params["code"]
if code not in ("goal_rounds_exhausted", "judge_result_invalid", "goal_config_invalid"):
    code = "judge_result_invalid"
sys.stderr.write("%s after %d round(s)\n" % (code, int(params["rounds"])))
sys.exit(1)
