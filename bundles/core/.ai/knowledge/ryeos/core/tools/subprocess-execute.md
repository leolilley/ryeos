---
category: ryeos/core/tools
tags: [tool, subprocess, executor]
version: "1.0.0"
description: Subprocess execute tool reference.
---

# Tool: ryeos/core/subprocess/execute

Invariant: subprocess execute is the canonical `@subprocess` target used by tool and streaming_tool kinds to launch terminal commands.

It owns command construction, working directory, environment, timeout, process group, and native async/resume metadata for generic subprocess execution.
