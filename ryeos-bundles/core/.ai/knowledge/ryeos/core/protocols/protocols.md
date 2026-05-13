---
category: ryeos/core
tags: [reference, protocols, wire, subprocess]
version: "1.0.0"
description: >
  The three subprocess wire protocols — opaque, runtime_v1, and
  tool_streaming_v1.
---

# Protocols

Protocols define how the daemon communicates with subprocess tools
and runtimes. Each protocol specifies the stdin/stdout shapes,
environment injections, capabilities, and lifecycle.

## Opaque Protocol (`protocol:ryeos/core/opaque`)

The simplest protocol, used by the `tool` kind.

| Aspect      | Value                         |
|-------------|-------------------------------|
| **stdin**   | `parameters_json` — JSON params |
| **stdout**  | `opaque_bytes` — raw bytes, terminal mode |
| **env**     | `RYE_THREAD_ID`, `RYE_PROJECT_PATH` |
| **lifecycle** | `detached_ok` — process can outlive parent |
| **callback**  | `none` — no callback channel |

The daemon writes params as JSON to stdin, reads opaque bytes from
stdout. No structured communication during execution.

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
| `tool`          | `opaque`                    |
| `streaming_tool` | `tool_streaming_v1`        |
| `runtime`       | `runtime_v1`                |

You don't need to specify the protocol explicitly — it's wired into
the kind schema.
