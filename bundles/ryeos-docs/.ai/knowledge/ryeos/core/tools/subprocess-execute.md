<!-- ryeos:signed:2026-09-06T01:55:13Z:332bc96ff2dd7c438972fe2dff58c3155b2e15824699f4affeab8a5e521d9dfb:UFUZZae/LsXjLp6kZoOW1Nyw22Zb37SdKl21tH0e6AWKo9Ifug6uGuph/obcgHd8HWdFPqDraCGJsYfnc9RJDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/tools
tags: [tool, subprocess, executor]
version: "1.2.0"
description: Subprocess execute tool reference.
---

# Tool: ryeos/core/subprocess/execute

Invariant: subprocess execute is the canonical `@subprocess` target used by ordinary Tools, including explicitly selected framed-output Tools to launch terminal commands.

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
immutable isolation snapshot before Lillux spawn. Tools cannot override policy or
activation. See [Execution Isolation](../node/execution-isolation.md).
