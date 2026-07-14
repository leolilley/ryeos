<!-- ryeos:signed:2026-07-14T10:12:30Z:69cbcf77bdd80ae9cb6efe03d3ee5e24288fdbc474b547b3d0af4c40caef933c:C2FSq4cyWGIxQZWkD2lDYfIja+C+XorGbNAE5xCFuDMRNJ7rTP3uawoc3oDQlxwy5bI7X1xX5jvIzYsOHcm9AQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [reference, protocols, wire, subprocess]
version: "1.0.0"
description: >
  Signed subprocess wire protocols and schema-driven protocol selection.
---

# Protocols

Protocols define how the daemon communicates with subprocess tools
and runtimes. Each protocol specifies the stdin/stdout shapes,
environment injections, capabilities, and lifecycle.

## Tool Callback V1 (`protocol:ryeos/core/tool_callback_v1`)

The default protocol selected by the `tool` kind. It retains opaque terminal
output while making callback authority explicit in signed protocol data.

| Aspect      | Value                         |
|-------------|-------------------------------|
| **stdin**   | `opaque` — executor-plan-owned bytes |
| **stdout**  | `opaque_bytes` — raw bytes, terminal mode |
| **env**     | `RYE_THREAD_ID`, `RYE_PROJECT_PATH`, plus declared `RYEOSD_*` socket/token/thread/project/auth bindings |
| **lifecycle** | `detached_ok` — process can outlive parent |
| **callback**  | `http_v1` — authenticated daemon callback channel |

The executor plan supplies stdin bytes and the daemon reads opaque bytes from
stdout. Default wrappers normally serialize params as JSON, while explicit
`input_data` remains opaque protocol data. The daemon mints callback and
thread-auth credentials because the descriptor requests them. Effective
capabilities still come only from verified item and manifest authority; an
empty capability set denies capability-gated resource operations. Exact-thread
and chain-local lifecycle methods still apply their documented token/access
class.

## Opaque Protocol (`protocol:ryeos/core/opaque`)

The callback-free form of terminal opaque execution. It has the same
plan-owned opaque stdin and opaque terminal stdout, declares only
`RYE_THREAD_ID` and `RYE_PROJECT_PATH`, and exposes no daemon socket. It is
available for schemas that deliberately do not need callbacks, but is not the
default `tool` protocol.

## Runtime V1 Protocol (`protocol:ryeos/core/runtime_v1`)

Full-featured protocol for runtime spawns (directive-runtime,
graph-runtime). Used by the `runtime` kind.

| Aspect      | Value                         |
|-------------|-------------------------------|
| **stdin**   | `launch_envelope_v1` — structured launch payload |
| **stdout**  | `runtime_result_v1` — structured result |
| **env**     | `RYEOSD_SOCKET_PATH`, `RYEOSD_CALLBACK_TOKEN`, `RYEOSD_THREAD_ID`, `RYEOSD_PROJECT_PATH`, `RYEOSD_THREAD_AUTH_TOKEN` |
| **lifecycle** | `managed` — daemon tracks process lifetime |
| **callback**  | `http_v1` — HTTP callback channel for async |

The runtime receives a `LaunchEnvelope` on stdin containing the
composed item, context blocks, parameters, and execution config.
It returns a `RuntimeResult` on stdout.

The HTTP callback channel allows the runtime to call back into the
daemon for tool dispatch, event logging, and state persistence.

## Method Runtime V1 (`protocol:ryeos/core/method_runtime_v1`)

The protocol selected by a kind schema's `execution.method_dispatch.protocol`
for a runtime that implements declared item methods. The runtime registry still
selects the signed implementation binary; it does not select or override this
wire contract.

| Aspect      | Value                         |
|-------------|-------------------------------|
| **stdin**   | `method_call_envelope_v1` — verified item/corpus payload plus declared method and bound arguments |
| **stdout**  | `method_call_result_v1` — structured terminal method result |
| **env**     | declared daemon callback bindings, including `RYEOSD_THREAD_AUTH_TOKEN` |
| **lifecycle** | `managed` — daemon tracks process lifetime |
| **callback**  | `http_v1` — authenticated daemon callback channel |

Accepted-launch preflight, boot validation, and live dispatch all require this
exact contract, including the canonical `RYEOSD_THREAD_AUTH_TOKEN` binding. A runtime
serving a method-dispatch-only kind is invoked through that kind's method
surface; launching its `runtime:` item directly through `runtime_v1` is
rejected because the envelopes are not interchangeable.

## Streaming Tool Protocol (`protocol:ryeos/core/tool_streaming_v1`)

Protocol for tools that emit streaming output. Used by the
`streaming_tool` kind.

| Aspect      | Value                         |
|-------------|-------------------------------|
| **stdin**   | `parameters_json` — JSON params |
| **stdout**  | `streaming_chunks_v1` — length-prefixed JSON frames |
| **env**     | `RYE_THREAD_ID`, `RYE_PROJECT_PATH` |
| **lifecycle** | `managed` — daemon tracks process |
| **callback**  | `none` |

During execution, the tool emits length-prefixed JSON frames on
stdout. Each frame is a JSON object with streaming chunk data.
The daemon reads these frames and forwards them to the event stream.

## Protocol Selection

The kind schema determines which protocol to use:

| Kind            | Protocol                    |
|-----------------|-----------------------------|
| `tool`          | `tool_callback_v1`          |
| `streaming_tool` | `tool_streaming_v1`        |
| `runtime`       | `runtime_v1`                |
| method-bearing kind | its `execution.method_dispatch.protocol` (currently `method_runtime_v1`) |

You don't specify the protocol on each item. The signed kind schema selects a
signed protocol descriptor—through its subprocess terminator or method
dispatch declaration—and the launcher follows that descriptor without a
kind-name/protocol-name table in code.
