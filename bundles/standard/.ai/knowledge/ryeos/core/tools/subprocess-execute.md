<!-- ryeos:signed:2026-06-05T00:57:59Z:7c2c9cc7b1aab4fc5516ad7f67a04f34dced88bd7c9b09057c6dc2c497683d71:KYeS/gcxmlkITZ65coHjR4ECtr25+ZjddnAgbakMXwfQ7UVzB/4tMGMYmz/abxrNK4cZCfiWBpQqeRICMZZsBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/tools
tags: [tool, subprocess, executor]
version: "1.0.0"
description: Subprocess execute tool reference.
---

# Tool: ryeos/core/subprocess/execute

Invariant: subprocess execute is the canonical `@subprocess` target used by tool and streaming_tool kinds to launch terminal commands.

It is an internal terminal executor, not a public root-executable command runner. Do not execute `tool:ryeos/core/subprocess/execute` directly. Its `executor_id: null` is the marker that ends an executor chain.

To run a command, define a wrapper tool and execute that wrapper:

```yaml
executor_id: "@subprocess"
config:
  command: "..."
  args: []
```

The `config_schema` on this terminal describes the wrapper `config:` block consumed by `@subprocess`; it is not a public caller-parameter schema for direct execution.

It owns command construction, working directory, environment, timeout, process group, and native async/resume metadata for generic subprocess execution.
