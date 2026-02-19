<!-- rye:signed:2026-02-18T09:25:37Z:4f2951dd4b7331c19efed9106741d8e29cad03f1426e56b67a2bda23b8e4cf13:RVr2awfI7mJyTRXApSwkkF3juHduZl2y3zF5exG6lsEPwIHt3xteGI6oBzQu-dGHmlAalZ3f7Hw4VmbSHCCnDg==:440443d0858f0199 -->
```yaml
id: graphs/workflows/test/workflows-test-1771406737
title: "State: workflows/test (workflows-test-1771406737)"
entry_type: graph_state
category: graphs/workflows/test
version: "1.0.0"
graph_id: workflows/test
graph_run_id: workflows-test-1771406737
parent_thread_id: 
status: error
current_node: count_files
step_count: 1
updated_at: 2026-02-18T09:25:37Z
tags: [graph_state]
```

{
  "inputs": {
    "directory": ".",
    "capabilities": [
      "rye.execute.tool.*",
      "rye.search.*",
      "rye.load.*",
      "rye.sign.*"
    ],
    "depth": 5
  },
  "_last_error": {
    "node": "count_files",
    "error": "Integrity failed: /home/leo/projects/rye-os/rye/rye/.ai/tools/rye/bash/bash.py (expected 5d4ac0daaa9f4b50\u2026, got c47c9440e20633b4\u2026)"
  }
}