<!-- ryeos:signed:2026-05-22T04:30:07Z:0d2229d4272c0348c64923cf55b3aeecdda186de2227e9542c0dfcfdf081383b:6VQ46Ub2kFGaLQqNLZ3+RhZ+VcP+7JseTw5dWYnIvRTUYCHCQBvuIS92ALsXzK5kfpigQTXGG1gBuNLKdQKLBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [callbacks, auth, uds, runtime, tokens, capabilities]
version: "1.0.0"
description: >
  Daemon-runtime callback authentication contract: callback capability
  tokens, thread-auth tokens, env injection, TTLs, caps enforcement, and
  revocation.
---

# Callback Authentication Protocol

Invariant: a runtime callback is accepted only when both the callback
capability token and thread-auth token validate, and the daemon-side
effective capability set authorizes the requested item before dispatch.

## Token types

The daemon mints two independent per-thread tokens in
`crates/core/app/src/callback_token.rs`:

- `CallbackCapability` (`cbt-...`) carries thread id, project path,
  composed `effective_caps`, expiry, and required `ExecutionProvenance`
  (`callback_token.rs:17-32`).
- `ThreadAuthState` (`tat-...`) carries the server-side acting principal
  and caller scopes (`callback_token.rs:157-164`).

Both token stores validate thread id and expiry. Callback capability
validation also checks the project path for dispatch calls.

## Environment injection

`mint_callback_env()` in `crates/core/executor/src/execution/runner.rs:572-631`
injects the runtime callback contract:

- `RYEOSD_SOCKET_PATH`
- `RYEOSD_CALLBACK_TOKEN`
- `RYEOSD_THREAD_ID`
- `RYEOSD_PROJECT_PATH`
- `RYEOSD_THREAD_AUTH_TOKEN`

Directive and graph runtimes fail closed when required callback env vars
are absent (`crates/runtimes/directive/src/main.rs:98-99`,
`crates/runtimes/graph/src/main.rs:106-122`).

## TTL

Callback token TTL defaults to 300 seconds and caps at 3600 seconds
(`callback_token.rs:10-14`, `callback_token.rs:151-154`). The same TTL
is used when minting callback capability and thread-auth state.

## Capability enforcement

The runtime cannot self-authorize callbacks. `runtime.dispatch_action`
loads the callback token, reads its composed `effective_caps`, and calls
`enforce_callback_caps()` before dispatch reaches the schema loop
(`crates/core/executor/src/execution/runtime_dispatch.rs:44-50`,
`runtime_dispatch.rs:73-101`). Empty caps are deny-all; wildcard and
path-prefix matching are delegated to the unified authorizer.

## Revocation symmetry

Inline executions track both tokens on `ExecutionGuard` and revoke them
on cleanup. Detached and resumed executions move token ownership into
the background task, where `CbTokenGuard` and `TatTokenGuard` revoke on
success, error, or panic (`runner.rs:13-15`, `runner.rs:1249-1311`).

## Provenance handoff

Callback children derive their workspace/engine provenance from the
parent token with `clone_for_borrowed_child()`; there is no fallback to
the daemon engine and no reconstruction from project-path strings.
