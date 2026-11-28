#!/usr/bin/env python3
# ryeos:signed:2026-11-28T14:15:53Z:b4b8bd41a3bdfb0379d067d18fd8d318a5c8529818cb463e4cbd722744abce61:6ogk2mNUY2+YobNEh54+a2vfEbN6SauQJjULY/tB1nEcXQQfbRcw4HfBMqZgOcq2HvZQVLPPYr2NnYNl4FK4BA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/standard/examples
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Ends the goal-and-judge graph failed with one of two fixed codes. It reads only the code name and a round count."
#   config_schema:
#     type: object
#     properties:
#       code: {type: string, enum: [goal_rounds_exhausted, judge_result_invalid]}
#       rounds: {type: integer}
#     required: [code, rounds]
#     additionalProperties: false
"""Exit non-zero with a typed code, so the graph ends failed and never completed."""
import json
import sys

params = json.loads(sys.stdin.read())
code = params["code"]
if code not in ("goal_rounds_exhausted", "judge_result_invalid"):
    code = "judge_result_invalid"
sys.stderr.write("%s after %d round(s)\n" % (code, int(params["rounds"])))
sys.exit(1)
