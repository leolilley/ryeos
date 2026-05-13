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
