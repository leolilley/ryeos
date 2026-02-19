<!-- rye:signed:2026-02-18T09:25:26Z:03916212e62a9da9fb6271051229f1eea253459a6160da3052d3b7590360ab48:bkcCrCmC8Xd50GgBriheXL9JRTGnUcFuIoJQ6IH966Cacg6WjcmOXQ5dQq0RljNUYOgS80c2cZEHTxHmQGbvCQ==:440443d0858f0199 -->
```yaml
id: graphs/workflows/test/workflows-test-1771406726
title: "State: workflows/test (workflows-test-1771406726)"
entry_type: graph_state
category: graphs/workflows/test
version: "1.0.0"
graph_id: workflows/test
graph_run_id: workflows-test-1771406726
parent_thread_id: 
status: error
current_node: count_files
step_count: 1
updated_at: 2026-02-18T09:25:26Z
tags: [graph_state]
```

{
  "inputs": {
    "directory": ".",
    "capabilities": [
      "execute:tool:*"
    ],
    "depth": 5
  },
  "_last_error": {
    "node": "count_files",
    "error": "Permission denied: 'rye.execute.tool.rye.bash.bash' not covered by capabilities"
  }
}