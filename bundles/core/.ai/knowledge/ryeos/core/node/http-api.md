---
category: ryeos/core
tags: [reference, api, http, routes]
version: "1.0.0"
description: >
  The daemon's HTTP API — all routes, authentication,
  request/response formats.
---

# HTTP API Reference

The daemon exposes an HTTP API for remote access. The CLI and MCP
servers communicate with the daemon over this API.

## Authentication

Most endpoints require `ryeos_signed` authentication — requests must
include a valid signature from a trusted key. Two endpoints are
unauthenticated: `/health` and `/public-key`.

## Routes

Core contributes control-plane routes. Standard contributes workflow thread
event/cancel routes. Every operational API handler has a matching signed
`service:` descriptor under the core or standard bundle.

### `GET /health`
Health check. No auth required.

- **Timeout:** 5s
- **Concurrency:** 256
- **Response:** JSON `{ status, version, uptime }`

### `GET /public-key`
Get the daemon's node public key. No auth required.

- **Timeout:** 5s
- **Concurrency:** 256
- **Response:** JSON public key document

### `POST /execute`
Execute an item. Authenticated.

- **Max body:** 10MB
- **Timeout:** 5 minutes
- **Concurrency:** 100
- **Request:** JSON with `item_ref`, `params`, `project_path`
- **Response:** Execution result

### `POST /execute/stream`
Execute an item with streaming response. Authenticated.

- **Max body:** 1MB
- **No timeout** (streaming)
- **Keep-alive:** 15s
- **Concurrency:** 32
- **Response:** SSE event stream

### `GET /threads/{thread_id}`
Get thread detail. Authenticated.

- **Timeout:** 30s
- **Concurrency:** 64
- **Response:** JSON with thread status, result, artifacts, facets

### `GET /threads/{thread_id}/events/stream`
Stream thread events in real-time. Authenticated.

- **Keep-alive:** 15s
- **Concurrency:** 64
- **Response:** SSE event stream

### `POST /threads/{thread_id}/cancel`
Cancel a running thread. Authenticated.

- **Timeout:** 30s
- **Concurrency:** 64
- **Response:** JSON confirmation

## Object and CAS Routes

- `POST /objects/has` — check whether CAS objects exist.
- `POST /objects/get` — fetch CAS objects by hash.
- `POST /objects/put` — ingest CAS objects.
- `POST /push-head` — push a project HEAD snapshot for remote execution.
- `POST /ingest-ignore` — evaluate ignore/ingest behavior for project snapshots.

These routes back pushed-head and cross-node transfer flows. Object fetches
fail closed if requested hashes are missing.

## Vault Routes

- `POST /vault/set`
- `GET /vault/list`
- `POST /vault/delete`

Vault routes mutate or read sealed daemon vault state. Runtime executions see
only resolved vault bindings from preflight, not raw vault storage.

## Bundle and Identity Routes

- `POST /bundle/export` — export bundle CAS state for transfer.
- `POST /authorize-key` — authorize a caller key.
- `GET /thread-status` — remote/thread status query surface.

The public identity route is intentionally unauthenticated; authorization and
bundle export require signed access.
