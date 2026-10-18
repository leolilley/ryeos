<!-- ryeos:signed:2026-08-18T22:04:50Z:90c203c7f402ceaedc06d07721a36b0ed45216cc4f062d362e7f2e5a9da2e9fa:gUIyJwCPzOYe4DuN5j/ALbCrgsWi4C+9uVXlJ2rxAQ8xdaHI9U/hOwGYhrqd0b1tLAh5PSpB5mSDRWoSEKT9AA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [callbacks, auth, uds, runtime, tokens, capabilities]
version: "1.1.0"
description: >
  Daemon-runtime callback authentication contract: callback capability
  tokens, thread-auth tokens, env injection, TTLs, caps enforcement, and
  revocation.
---

# Callback Authentication Protocol

Invariant: callback authentication is selected by the UDS method's access
class. Callback-token methods validate the capability token and its exact
thread/provenance/capability context. Thread-auth methods validate the per-thread
auth token and then apply their handler-specific capability/provenance checks.
Two-proof methods such as `runtime.poll_input` and `runtime.author_item` require
both. Exact-thread lifecycle methods are bound to the attached thread identity.

## Token types

The daemon mints two independent per-thread tokens in
`crates/daemon/ryeos-app/src/callback_token.rs`:

- `CallbackCapability` (`cbt-...`) carries thread id, server-retained
  project/state provenance,
  composed `effective_caps`, expiry, and required `ExecutionProvenance`.
- `ThreadAuthState` (`tat-...`) carries the server-side acting principal
  and caller scopes.

Both token stores validate thread id and expiry. Callback capability validation
uses the retained provenance captured when the token was minted; callback
requests cannot supply or replace a host project path. `RYE_PROJECT_PATH` is
only the subprocess's selected execution workspace.

## Environment injection

The verified terminator protocol is the sole callback-environment authority.
Its signed `env_injections` select values from the closed protocol vocabulary;
the launcher produces those values and carries them through final environment
composition as typed protocol bindings. `runtime`, `method_runtime`, and
`tool_callback` declare the daemon callback names they need:

- `RYEOSD_SOCKET_PATH`
- `RYEOSD_CALLBACK_TOKEN`
- `RYEOSD_THREAD_ID`
- `RYEOSD_PROJECT_STATE_SCOPE` — opaque logical-project namespace identity;
  stable across pinned COW successors and not itself an authority
- `RYEOSD_THREAD_AUTH_TOKEN`

The callback token and thread identity bind the durable project/state
authority server-side. A subprocess does not receive or submit the canonical
host project path as callback authority.

Callback capability authority is minted only when the verified descriptor's
callback channel/injections require it. Thread-auth authority is minted only
when the protocol asks for the `thread_auth_token` source. The default `tool`
schema selects `tool_callback` so signed manifest-backed bundle-event, vault,
and item-authoring callbacks remain available. Callback-free protocols such as
`opaque`, `tool_streaming`, and `cli_exec` receive neither credential and do
not expose the daemon socket inside an enforced sandbox.

Directive and graph runtimes fail closed when their required callback env vars
are absent.

## TTL

Launch-scoped callback and thread-auth tokens use the effective run duration
plus a five-minute finalization margin. A seven-day absolute backstop bounds
unlimited or pathological runs; runs that genuinely need more require token
renewal. When no duration is available, the launch lifetime is ten minutes.
Both token types receive the same lifetime and are invalidated when the owned
execution ends.

## Capability enforcement

The runtime cannot self-authorize callbacks. `runtime.dispatch_action`
loads the callback token, reads its composed `effective_caps`, and calls
`enforce_callback_caps()` before dispatch reaches the schema loop. Empty caps
are deny-all; wildcard and path-prefix matching are delegated to the unified
authorizer.

## Revocation symmetry

Inline executions track every minted token on `ExecutionGuard` and revoke it
on cleanup. Detached and resumed executions move optional token ownership into
the background task, where `CbTokenGuard` and `TatTokenGuard` revoke on success,
error, or panic. Callback-free launches install no authority.

## Provenance handoff

Callback children derive their workspace/engine provenance from the
parent token with `clone_for_borrowed_child()`; there is no fallback to
the daemon engine and no reconstruction from project-path strings.
