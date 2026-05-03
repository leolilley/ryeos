# Protocols-as-Data Reference

## Overview

Rye's dispatch layer has exactly two terminators — `in_process` and `subprocess`.
Variation lives not in new terminator types but in **protocol descriptors**: signed
YAML documents that compose a closed vocabulary of typed primitives to define the
wire contract between the daemon and a child binary.

This document describes what's hardcoded, what's data, and how to extend the system.

## What's Hardcoded vs What's Data

| Aspect | Hardcoded (daemon code) | Data (signed YAML) |
|--------|------------------------|-------------------|
| Terminator types | `InProcess`, `Subprocess` (2, never more) | — |
| Stdin shapes | `parameters_json`, `none` | Protocol descriptor's `stdin.shape` |
| Stdout shapes | `opaque_bytes`, `runtime_result_v1`, `streaming_chunks_v1` | Protocol descriptor's `stdout.shape` |
| Stdout modes | `terminal`, `streaming` | Protocol descriptor's `stdout.mode` |
| Lifecycle modes | `managed`, `detached_ok` | Protocol descriptor's `lifecycle.mode` |
| Env injection sources | `callback_token_url`, `thread_id`, `project_path` | Protocol descriptor's `env_injections[].source` |
| Capabilities | `allows_pushed_head`, `allows_target_site`, `allows_detached` | Protocol descriptor's `capabilities.*` |
| Callback channels | `http_v1` | Protocol descriptor's `callback_channel` |
| Protocol identity | — | Protocol descriptor's `name` + `category` |

The **vocabulary** (left column) is the daemon's syscall set. The **protocols** (right
column) are programs composed from those syscalls. Adding a new protocol requires no
daemon code change. Adding a new vocabulary primitive requires a daemon code change and
is an architectural decision.

## Protocol Descriptor Schema

Located at `.ai/protocols/<category>/<name>.yaml` in a bundle.

```yaml
kind: protocol
name: runtime_v1           # unique within category
category: rye/core         # namespace
abi_version: v1
description: Human-readable summary

stdin:
  shape: parameters_json   # or "none"

stdout:
  shape: runtime_result_v1 # or "opaque_bytes", "streaming_chunks_v1"
  mode: terminal           # or "streaming"

env_injections:
  - { name: RYE_THREAD_ID, source: thread_id }
  - { name: RYE_PROJECT_PATH, source: project_path }

capabilities:
  allows_pushed_head: true
  allows_target_site: true
  allows_detached: false

lifecycle:
  mode: managed            # or "detached_ok"

callback_channel: http_v1  # optional, for managed lifecycle
```

## Bundled Protocols

Three protocols ship in the core bundle:

| Protocol | Stdin | Stdout | Lifecycle | Purpose |
|----------|-------|--------|-----------|---------|
| `opaque` | `parameters_json` | `opaque_bytes` / terminal | `detached_ok` | Simple tools (capture stdout) |
| `runtime_v1` | `parameters_json` | `runtime_result_v1` / terminal | `managed` | Runtimes (LLM threads, graph engine) |
| `tool_streaming_v1` | `parameters_json` | `streaming_chunks_v1` / streaming | `managed` | Streaming tools (length-prefixed JSON frames) |

## How to Add a New Protocol

Create a new `.yaml` file in `.ai/protocols/rye/core/` (or a new category) composing
existing vocabulary primitives. Sign it with `rye-bundle-tool sign-items`. Create a
kind schema whose terminator references the new protocol. No daemon code change needed.

Example: a `remote_http_v1` protocol for dispatching to remote HTTP endpoints would
use `stdin: { shape: none }`, `stdout: { shape: opaque_bytes, mode: terminal }`,
`lifecycle: { mode: managed }`, and add new env injection sources if needed (which
*would* require a vocabulary change).

## How to Add a New Vocabulary Primitive

This requires a daemon code change. The process:

1. Add the new variant to the relevant vocabulary enum in
   `ryeos-engine/src/protocol_vocabulary/`.
2. Update all match arms (builders, decoders, compatibility matrices).
3. Add tests for the new variant.
4. Gate clean.
5. Protocol descriptors can now reference the new primitive.

Vocabulary changes are architectural decisions because they expand the daemon's
trusted computing base. The closed vocabulary is a security boundary — it limits
what protocol descriptors can express to what the daemon has been designed to handle.

## Threat Model

Protocol descriptors are signed YAML loaded from base roots only (system bundles
installed by the operator). A malicious protocol descriptor cannot:

- Escalate beyond the declared capabilities (enforced by dispatch code).
- Inject unexpected env vars (the vocabulary restricts source names).
- Bypass the stdin/stdout shape contracts (enforced by the frame reader/writer).

A compromised base root (attacker controls the bundle) can do anything the daemon
can do — this is the same threat model as any package manager's trusted repository.

## Kind Schema Terminator Reference

In a kind schema's `execution` block:

```yaml
execution:
  terminator:
    kind: subprocess
    protocol: protocol:rye/core/opaque
```

or:

```yaml
execution:
  terminator:
    kind: in_process
    registry: services
```

The `protocol` field is a canonical ref into the `ProtocolRegistry`. The `registry`
field names an in-process handler registry. These are the only two terminator forms.
