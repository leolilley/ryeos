<!-- ryeos:signed:2026-07-16T02:18:49Z:ed090e9551ab49f0ff679118057fed67aa171eeacf2bcbac4ef077bf7e79be0d:TEQatNWSGsiF1ecBp0/f5HK9CHz7njEkvCcjLN5NSezW4rbrCNJ8KFwelvJ5cHWMUvYWHVjAAoi91Ei6M1reCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/tools
tags: [tool, subprocess, executor]
version: "1.1.0"
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

After this terminal constructs the request, the engine applies the node's
immutable sandbox snapshot before Lillux spawn. Tools cannot override policy or
activation. See [Execution Isolation](../node/execution-isolation.md).
