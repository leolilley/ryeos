<!-- ryeos:signed:2026-05-31T08:15:56Z:ae10bd69ac5ebb747455e42ed27a68d7e9737af482cbe606efbeafa8b916170d:buopH47gQOsLZKGVnkxNzE3rJK3N/ATpzWnElxdENHUk3KSo3iq1CkbnGn88rCdoRYJcvaTe/2JO4b211XCKAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/tools
tags: [tool, subprocess, executor]
version: "1.0.0"
description: Subprocess execute tool reference.
---

# Tool: ryeos/core/subprocess/execute

Invariant: subprocess execute is the canonical `@subprocess` target used by tool and streaming_tool kinds to launch terminal commands.

It owns command construction, working directory, environment, timeout, process group, and native async/resume metadata for generic subprocess execution.
