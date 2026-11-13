<!-- ryeos:signed:2026-11-13T02:52:29Z:a3e85f90dd2ec71a0b5698ca50d50feb09a4c4a5bf3d00be53ca39de9ffa6f0a:WHi9YxFkCvvcxYMb7xTIsRyRK1PNo2H3cadlZqYye5j0UqhdTPOFzmz7BTU5PEc9+BkfJSWgL2loJqLNDWnlAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [reference, api, http, routes, remote]
version: "1.1.0"
description: >
  The daemon HTTP API — routes, authentication, service mapping, and
  remote-operation surfaces.
---

# HTTP API Reference

The daemon exposes an HTTP API for CLI, MCP, and remote daemon access.
Most endpoints require `ryeos_signed` authentication: Ed25519-signed
requests from a key present in the target node's authorized-key store.

Unauthenticated discovery endpoints:

- `GET /health`
- `GET /public-key`
- `GET /ingest-ignore`
- `POST /admission/claim` (token-gated bootstrap; see below)

All other routes require signed auth and then enforce the capabilities
listed by the target service descriptor.

## Execution Routes

### `POST /execute`

Execute an item or service. Authenticated.

- **Source:** dispatch engine / service registry
- **Max body:** 10 MB
- **Timeout:** 5 minutes
- **Request:** JSON execution envelope (`item_ref`, `parameters`,
  `project_path`, and optional project source fields)
- **Response:** execution result envelope

#### Transient refusal: `process_creation_busy`

When the daemon cannot create a handler process it needs to resolve the
requested item, because fork-sensitive descriptor authority stayed held past
the five-second setup deadline, it refuses with HTTP 503 and:

```json
{
  "code": "process_creation_busy",
  "error": "process creation is busy; retry after 250 ms",
  "retryable": true,
  "retry_after_ms": 250
}
```

The refusal is raised only while resolving the root item, before the request
is admitted, so nothing ran and the same request may be reissued after
`retry_after_ms`. The body names no holder; the daemon log records the held
lease sites. A stranded fork window or a missing handler binary is not this
refusal and is not retryable.

### `POST /execute/stream`

Execute with an SSE response stream. Authenticated.

- **Source:** launch dispatcher
- **Max body:** 1 MB
- **Timeout:** none; stream uses keep-alives
- **Keep-alive:** 15s
- **Response:** Server-Sent Events

## Discovery and Identity Routes

### `GET /health`

Health check. No auth required.

- **Source:** `service:health/status`
- **Response:** daemon health/status JSON

### `GET /public-key`

Return the daemon node public identity. No auth required.

- **Source:** `service:identity/public_key`
- **Response fields:** `principal_id`, `fingerprint`, and vault
  fingerprint fields used by remote configuration

### `GET /ingest-ignore`

Return the node ingest-ignore rules. No auth required.

- **Source:** `service:system/ingest-ignore`
- **Used by:** `ryeos remote configure`, `remote push`, and
  `remote execute`
- **Response:** ignore config, typically `{ "patterns": [...] }`

### `POST /authorize-key`

Authorize a public key with scoped capabilities. Authenticated.

- **Source:** `service:identity/authorize-key`
- **Required cap:** `ryeos.execute.service.identity/authorize-key`
- **Request:** `{ public_key, label, scopes }`
- **Response:** authorized-key metadata including fingerprint and scopes

### `POST /admission/claim`

Claim a one-time local admission token. No prior authorized key is
required, but the request is token-gated and self-signed by the node key
being admitted.

- **Source:** `service:admission/claim`
- **Auth:** none; validates token hash file, token expiry, requested
  scopes subset, nonce/timestamp, and claimant signature
- **Request:** `{ token, public_key, label, scopes, nonce, timestamp_unix, signature }`
- **Response:** authorized-key metadata including fingerprint and scopes
- **Used by:** `ryeos remote admit`

## Thread Routes

### `GET /threads`

List threads. Authenticated.

- **Source:** `service:threads/list`
- **Query:** `limit` optional
- **Used by:** `ryeos remote threads`

### `GET /threads/{thread_id}`

Get thread detail. Authenticated.

- **Source:** `service:threads/get`
- **Used by:** `ryeos remote thread-status`
- **Response:** thread status, result, artifacts, and facets

### `GET /threads/{thread_id}/events/stream`

Stream thread events in real time. Authenticated.

- **Response:** SSE event stream
- **Keep-alive:** 15s

### `POST /threads/{thread_id}/cancel`

Cancel a running thread. Authenticated.

- **Source:** thread cancellation control surface
- **Response:** cancellation confirmation or error

## Object and CAS Routes

### `POST /objects/has`

Check whether typed CAS entries exist on this node. Requests carry separate
`object_hashes` and `blob_hashes` arrays; responses partition each namespace
into found and missing hashes. A digest found as a blob never satisfies an
object request, or vice versa.

- **Source:** `service:objects/has`
- **Required cap:** `ryeos.execute.service.objects/has`
- **Used by:** remote push/execute upload planning

### `POST /objects/put`

Upload CAS blobs/objects to this node.

- **Source:** `service:objects/put`
- **Required cap:** `ryeos.execute.service.objects/put`
- **Used by:** remote push/execute

### `POST /objects/get`

Fetch CAS blobs/objects by hash.

- **Source:** `service:objects/get`
- **Required cap:** `ryeos.execute.service.objects/get`
- **Used by:** remote pull, remote execute pull-back, remote bundle install
- **Failure mode:** callers should treat missing requested hashes as
  fail-closed errors

### `POST /push-head`

Write a principal-scoped pushed HEAD snapshot for remote execution.

- **Source:** `service:system/push-head`
- **Required cap:** `ryeos.execute.service.system/push-head`
- **Request:** `{ project_path, snapshot_hash }`
- **Used by:** remote push and remote execute

## Bundle Routes

### `POST /bundle/export`

Export an installed bundle as CAS file hashes.

- **Source:** `service:bundle/export`
- **Required cap:** `ryeos.execute.service.bundle/export`
- **Used by:** `ryeos remote bundle-install`

## Vault Routes

### `POST /vault/set`

Set a secret in the node vault.

- **Source:** `service:vault/set`
- **Required cap:** `ryeos.execute.service.vault/set`

### `GET /vault/list`

List secret names in the node vault.

- **Source:** `service:vault/list`
- **Required cap:** `ryeos.execute.service.vault/list`

### `POST /vault/delete`

Delete a secret from the node vault.

- **Source:** `service:vault/delete`
- **Required cap:** `ryeos.execute.service.vault/delete`

Vault route bodies are protected by request signing for authentication
and integrity, but `vault/set` sends secret values in the HTTP body.
Use TLS in production.

## Remote Flow Map

Remote commands are local orchestrators that call this HTTP surface on a
target node. See [Remote Command Reference](../remote/remote-command-reference.md)
for command syntax and the local-capability vs remote-scope matrix.
