<!-- ryeos:signed:2026-05-22T03:35:36Z:8084f3f8411744af1a4566095806bab11efc76a92b86f0e3419667bc7043e66e:eVOI16K8YsutQ6xQMB8gLnVKHGwBZBrOvmRMRl3PwWSA3J1YjVqatqsu15xd4fqbNGv3rxo9OLHpnatjx3TbAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core/node
tags: [architecture, routes, http, dispatcher, compilation, hot-reload]
version: "1.0.0"
description: >
  The data-driven route system — how 16 signed YAML files define the
  entire HTTP surface of the daemon. Covers compilation, the single
  fallback dispatcher, 5 response modes, 8 invoker types, per-route
  semaphores, auth as a route property, and ArcSwap hot-reload.
---

# Route System

The entire HTTP surface of the daemon is defined by signed YAML files
in `.ai/node/routes/`. There are zero hardcoded axum routes. Every
endpoint — authenticated execution, unauthenticated discovery, vault
operations, thread streaming — is declared as data and compiled at
startup.

## How It Works

### Step 1: YAML defines the full route contract

A single YAML file declares everything about an endpoint:

```yaml
id: core/execute
path: /execute
methods: [POST]
auth: ryeos_signed
request:
  body: json
limits:
  body_bytes_max: 10485760
  timeout_ms: 300000
  concurrent_max: 100
response:
  mode: execute
```

### Step 2: Startup compiles YAML into typed dispatch units

The route builder reads all YAML files and for each route:

1. **Validates** — duplicate IDs, path collisions, valid HTTP methods,
   unknown auth verifiers, unknown response modes
2. **Compiles auth** — `auth: ryeos_signed` → `CompiledRyeosSignedVerifier`
3. **Compiles response mode** — `mode: execute` → `CompiledExecuteMode`
4. **Creates per-route semaphore** — from `limits.concurrent_max`
5. **Compiles path captures** — `/threads/{thread_id}` → capture groups

A malformed YAML file causes the daemon to refuse to start with a
specific error naming the file and the problem.

### Step 3: Every request hits one function

The axum server registers a single fallback handler:

```rust
axum::Router::new().fallback(route_dispatcher)
```

The dispatcher does:

1. Match path + method against the compiled path matcher
2. Acquire the per-route semaphore (or return 503)
3. Check body size limit, read bounded body
4. Run the auth invoker → produces a `RoutePrincipal`
5. Hand off to the compiled response mode

### Step 4: Response modes bridge HTTP to business logic

Five built-in modes, each a trait implementation:

| Mode | What it does | Source |
|---|---|---|
| `static` | Returns fixed status/body (base64-encoded) | None |
| `json` | Calls a service or engine dispatch, returns JSON | `service:`, `tool:`, `directive:`, `graph:` |
| `execute` | Full dispatch pipeline (token → engine) | Implicit (from body) |
| `event_stream` | SSE stream (gateway or subscription) | `dispatch_launch` or `thread_events` |
| `launch` | Fire-and-forget dispatch, returns 202 | `response.source_config` |

The `accepted` key is an alias for `launch`.

Each mode is strict about what it accepts. The `execute` mode rejects a
route that declares `response.source` or static fields — it's a
dedicated pipeline. The `json` mode requires `response.source` and
rejects `execute` fields. You can't accidentally wire the wrong handler
type to the wrong mode.

## Invokers

The `CompiledRouteInvocation` trait unifies auth, services, streaming,
and launch into one interface:

```rust
trait CompiledRouteInvocation {
    fn contract() -> &RouteInvocationContract;  // static metadata
    async fn invoke(ctx) -> Result<RouteInvocationResult>;  // runtime
}
```

Eight invoker implementations:

| Invoker | Output | Purpose |
|---|---|---|
| `CompiledNoneVerifier` | Principal | Anonymous access (no auth) |
| `CompiledRyeosSignedVerifier` | Principal | Ed25519 signature verification |
| `CompiledHmacVerifier` | Principal | Config-driven HMAC-SHA256 |
| `CompiledServiceInvocation` | Json | In-process service dispatch |
| `CompiledDispatchInvoker` | Json | Engine dispatch (resolve + execute) |
| `CompiledLaunchInvocation` | Accepted | Fire-and-forget launch |
| `CompiledGatewayStreamInvocation` | Stream | SSE gateway stream |
| `CompiledSubscriptionStreamInvocation` | Stream | Per-thread SSE subscription |

The contract enforcement layer (`invoke_checked`) verifies both sides:
the mode checks the invoker's declared output type, then verifies the
actual runtime result matches. If an auth verifier accidentally returns
Json instead of Principal, it's caught.

## Compile-Time Validation

Over 75 validation checks run at route table build time. The most
significant:

- **Duplicate route IDs** — two routes with the same `id`
- **Path collisions** — two patterns with the same segment structure
  sharing an HTTP method
- **Unknown auth verifier** — `auth` must be `none`, `ryeos_signed`, or
  `hmac`
- **Unknown response mode** — `mode` must be a registered mode key
- **Mode-specific validation** — each mode rejects fields that don't
  belong to it (e.g., `execute` mode rejects `response.source`)
- **Undeclared path captures** — `${path.unknown}` in `source_config`
  when `unknown` is not in the route path
- **Auth-mode coupling** — `execute` mode requires `auth: ryeos_signed`
- **HMAC config validation** — secret env var must exist, replay
  protection must be configured, header validation, signature encoding

A bad YAML file can never reach production. It fails at daemon startup.

## Auth as a Route Property

Each route independently declares its auth mechanism. Auth is compiled
into the route and runs as part of the dispatch chain — there is no
global auth middleware.

| Auth | Verifier | Produces |
|---|---|---|
| `none` | `CompiledNoneVerifier` | Anonymous principal |
| `ryeos_signed` | `CompiledRyeosSignedVerifier` | Principal with fingerprint + scopes |
| `hmac` | `CompiledHmacVerifier` | Principal with delivery ID + metadata |

This means a webhook endpoint from Stripe can use HMAC while the
`/execute` endpoint uses Ed25519 signatures — no conflict, no middleware
ordering concerns.

## Per-Route Semaphores

Each route gets its own `tokio::sync::Semaphore` from `concurrent_max`:

- Non-blocking: uses `try_acquire_owned()` — returns 503 immediately if
  all permits are taken (no queuing)
- Per-route isolation: a saturated `/objects/put` does not affect
  `/health` or `/threads/{id}`
- The permit lives until the request completes (auth + body read +
  response), so concurrent_max is a true concurrency limit

## Path Capture Interpolation

Route path captures are compiled in two phases:

1. **Compile time** (`validate_path_templates`): walks the JSON tree and
   checks every `${path.<name>}` references a declared capture group
2. **Runtime** (`interpolate_path`): substitutes actual capture values
   from the matched request

For example, a route with `path: /threads/{thread_id}` and
`source_config: { "thread_id": "${path.thread_id}" }` gets the actual
thread ID from the URL path injected into the service handler input.

Only `${path.*}` is supported. `${headers.*}` and `${body.*}` are
rejected at startup.

## Hot-Reload via ArcSwap

The route table is wrapped in `Arc<ArcSwap<RouteTable>>`.

- **Read path**: `route_table.load_full()` returns a clone of the
  current `Arc<RouteTable>`. Wait-free for readers — no blocking.
- **Write path**: `route_table.store(Arc::new(new_table))` atomically
  replaces the table. New requests see the new table immediately.
- **In-flight safety**: requests holding a prior `load_full()` reference
  complete on the old table.

Combined with bundles: install a new bundle with additional routes,
reload, and new endpoints appear without restarting the daemon.

A SHA-256 fingerprint of the sorted route IDs is computed after each
build, providing a stable version identity for the route table.

## Route YAML Files Are Signed Items

The route YAML files are signed bundle items, just like tools, services,
and kind schemas. The route table is not just data-driven — it is
cryptographically attested. You know that the route saying
`auth: none` on `/health` was placed there by the bundle publisher, not
tampered with after the fact.
