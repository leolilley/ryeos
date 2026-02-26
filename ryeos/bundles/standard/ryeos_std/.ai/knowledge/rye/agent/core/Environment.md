<!-- rye:signed:2026-02-26T05:02:40Z:4d9a83031581ae34a3da39faac30f85bbb9065247211773472e12cb1cba61f0d:2hskOKVgHbUQoWENNAQSQSa8P9r7nSy_qASu4ifyn0XUU3kJ1NKl2Ec37Fod2HmF2qIG3hDCHKAK0jGi0GvdAw==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: Environment
title: Thread Environment
entry_type: context
category: rye/agent/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-24T00:00:00Z
tags:
  - environment
  - runtime
  - thread-started
```

## Thread Environment

- **Project**: ${project_path}
- **Model**: ${model}
- **Thread depth**: ${depth} (0 = root)
- **Parent thread**: ${parent_thread_id} (none if root)
- **Budget**: ${spend_limit} USD, ${max_turns} turns
- **Capabilities**: ${capabilities_summary}
