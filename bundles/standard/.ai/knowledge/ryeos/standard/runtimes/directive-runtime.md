<!-- ryeos:signed:2026-05-22T19:55:06Z:71d0a47c105238484cb8dccd0473c0c3a0731ccea686f46e2c1c293e4a5ec317:K4VASX5VufR+tgSEYjNlGu6ToG8+heq8KlBzUigjW/IfGpktsUXUzZNkFJkeJsqelzF0bNjJATTz+14smKWADA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard/runtimes
tags: [runtime, directive, llm, callbacks]
version: "1.0.0"
description: Directive runtime reference.
---

# Runtime: directive-runtime

Invariant: directive-runtime receives a frozen launch envelope and runs the LLM prompt/tool loop without re-resolving provider trust-sensitive configuration.

It consumes rendered context blocks, resolved provider snapshots, limits, tool inventory, callback environment, thread id, and vault bindings. Tool dispatches are callbacks to the daemon, authenticated by callback and thread-auth tokens and gated by effective caps.
