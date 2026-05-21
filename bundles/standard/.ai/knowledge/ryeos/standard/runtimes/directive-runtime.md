<!-- ryeos:signed:2026-05-21T11:11:49Z:71d0a47c105238484cb8dccd0473c0c3a0731ccea686f46e2c1c293e4a5ec317:TFaWtpucWS4cvZdAeRrBiNUjWx8/08TxMy/1/md9EzGvYveKmT1uRfi0Hjudyxy+0XgAqZkm8aEw9/RsXa/ICA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/runtimes
tags: [runtime, directive, llm, callbacks]
version: "1.0.0"
description: Directive runtime reference.
---

# Runtime: directive-runtime

Invariant: directive-runtime receives a frozen launch envelope and runs the LLM prompt/tool loop without re-resolving provider trust-sensitive configuration.

It consumes rendered context blocks, resolved provider snapshots, limits, tool inventory, callback environment, thread id, and vault bindings. Tool dispatches are callbacks to the daemon, authenticated by callback and thread-auth tokens and gated by effective caps.
