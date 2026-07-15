<!-- ryeos:signed:2026-07-14T01:54:46Z:35fb0a062cc197bf83d07375656a0e55731adfd186cbe9b0f53297e8f55117ba:cBH287LggilPnKLwM3zBAIJODie3ck1n5VBjFO9ZL8E1MVEmE6PZM7p8E08dbEgH2++MQVSbfEXDZ1FmDhqUDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
activation. See [Execution Sandbox](../node/execution-sandbox.md).
