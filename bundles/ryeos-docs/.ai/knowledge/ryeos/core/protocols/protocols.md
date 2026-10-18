<!-- ryeos:signed:2026-09-06T01:55:11Z:faf00a85628597d873a9d51222b2e7c15c8a3d0296e847eff2a6a401b8974eb7:ju4FWqp+EiTeJadHUxJ6YAxQf7dVTcXS4SYb2S3S9RZ1xbWOeXf9kZ/Q36oDEUn5yU+WaPhLerq3N7BrpXGWAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [reference, protocols, wire, subprocess]
version: "1.2.0"
description: >
  Signed subprocess wire protocols and schema-driven protocol selection.
---

# Protocols

Protocols define how the daemon communicates with subprocess tools
and runtimes. Each protocol specifies the stdin/stdout shapes,
environment injections, capabilities, and lifecycle.

## Tool Callback V1 (`protocol:ryeos/core/tool_callback`)

The default protocol selected by the `tool` kind. It retains opaque terminal
output while making callback authority explicit in signed protocol data.

| Aspect      | Value                         |
|-------------|-------------------------------|
| **stdin**   | `opaque` — executor-plan-owned bytes |
| **stdout**  | `opaque_bytes` — raw bytes, terminal mode |
| **env**     | `RYE_THREAD_ID`, `RYE_PROJECT_PATH`, plus declared `RYEOSD_*` socket/token/thread/state-scope/auth bindings |
| **lifecycle** | `detached_ok` — process can outlive parent |
| **callback**  | `http` — authenticated daemon callback channel |

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

## Runtime V1 Protocol (`protocol:ryeos/core/runtime`)

Full-featured protocol for runtime spawns (directive-runtime,
graph-runtime). Used by the `runtime` kind.

| Aspect      | Value                         |
|-------------|-------------------------------|
| **stdin**   | `launch_envelope` — structured launch payload |
| **stdout**  | `runtime_result` — structured result |
| **env**     | `RYEOSD_SOCKET_PATH`, `RYEOSD_CALLBACK_TOKEN`, `RYEOSD_THREAD_ID`, `RYEOSD_PROJECT_STATE_SCOPE`, `RYEOSD_THREAD_AUTH_TOKEN` |
| **lifecycle** | `managed` — daemon tracks process lifetime |
| **callback**  | `http` — HTTP callback channel for async |

The runtime receives a `LaunchEnvelope` on stdin containing the
composed item, context blocks, parameters, and execution config.
It returns a `RuntimeResult` on stdout.

The HTTP callback channel allows the runtime to call back into the
daemon for tool dispatch, event logging, and state persistence.

## Method Runtime V1 (`protocol:ryeos/core/method_runtime`)

The protocol selected by a kind schema's `execution.method_dispatch.protocol`
for a runtime that implements declared item methods. The runtime registry still
selects the signed implementation binary; it does not select or override this
wire contract.

| Aspect      | Value                         |
|-------------|-------------------------------|
| **stdin**   | `method_call_envelope` — verified item/corpus payload plus declared method and bound arguments |
| **stdout**  | `method_call_result` — structured terminal method result |
| **env**     | declared daemon callback bindings, including `RYEOSD_THREAD_AUTH_TOKEN` |
| **lifecycle** | `managed` — daemon tracks process lifetime |
| **callback**  | `http` — authenticated daemon callback channel |

Accepted-launch preflight, boot validation, and live dispatch all require this
exact contract, including the canonical `RYEOSD_THREAD_AUTH_TOKEN` binding. A runtime
serving a method-dispatch-only kind is invoked through that kind's method
surface; launching its `runtime:` item directly through `runtime` is
rejected because the envelopes are not interchangeable.

## Streaming Tool Protocol (`protocol:ryeos/core/tool_streaming`)

Protocol selected explicitly by ordinary Tools that emit streaming output.

| Aspect      | Value                         |
|-------------|-------------------------------|
| **stdin**   | `opaque` — unchanged signed executor-plan input |
| **stdout**  | `streaming_chunks` — length-prefixed JSON frames |
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
| `tool`          | explicit item selection: `tool_callback`, `opaque`, or `tool_streaming` |
| `runtime`       | `runtime`                |
| method-bearing kind | its `execution.method_dispatch.protocol` (currently `method_runtime`) |

The signed kind schema owns protocol selection. Tool authors must provide
`execution_protocol` from its closed allowlist. Other schemas may select a fixed
descriptor through their subprocess terminator or method declaration. The
launcher follows verified wire mechanics without a kind-name/protocol-name table.
