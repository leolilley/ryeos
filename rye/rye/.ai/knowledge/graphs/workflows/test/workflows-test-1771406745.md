<!-- rye:signed:2026-02-18T09:25:45Z:fb82b6816d5d5c141e858446526ea9abba43e032e83c2ff1018334c8bc25b01e:c4gnMRRm2vExQA67Ipr4RSFCpiS8ysLwkinATFxuVWF5PGZ-7pHbHXstd0HVwWosNDKHImJ7pVMW2BQXNrteAQ==:440443d0858f0199 -->
```yaml
id: graphs/workflows/test/workflows-test-1771406745
title: "State: workflows/test (workflows-test-1771406745)"
entry_type: graph_state
category: graphs/workflows/test
version: "1.0.0"
graph_id: workflows/test
graph_run_id: workflows-test-1771406745
parent_thread_id: 
status: error
current_node: count_files
step_count: 1
updated_at: 2026-02-18T09:25:45Z
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
    "error": "Command not found: find . -name '*.py' -not -path '*/.venv/*' -not -path '*/__pycache__/*' | wc -l"
  }
}