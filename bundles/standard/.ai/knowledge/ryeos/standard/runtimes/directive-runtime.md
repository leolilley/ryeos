---
category: ryeos/standard/runtimes
tags: [runtime, directive, llm, callbacks]
version: "1.0.0"
description: Directive runtime reference.
---

# Runtime: directive-runtime

Invariant: directive-runtime receives a frozen launch envelope and runs the LLM prompt/tool loop without re-resolving provider trust-sensitive configuration.

It consumes rendered context blocks, resolved provider snapshots, limits, tool inventory, callback environment, thread id, and vault bindings. Tool dispatches are callbacks to the daemon, authenticated by callback and thread-auth tokens and gated by effective caps.
