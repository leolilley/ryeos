# ryeos-remote-mcp

Stateless MCP-over-HTTP proxy for `ryeos-remote` on Modal.

## What it does

Exposes the 4 Rye tools (execute, search, load, sign) as both MCP and REST
interfaces, proxying all requests to Modal's `ryeos-remote` `/execute` endpoint.
Also provides CAS sync passthrough endpoints.

## What it does NOT have

- No rye engine, no CAS store, no volume, no `.ai/` directory
- No bundler, no resolver, no executor
- Just `httpx` + `mcp` SDK + `starlette`

## Endpoints

### MCP

- `POST /mcp` — MCP streamable HTTP (stateless, JSON responses)

### REST

- `POST /execute` — proxy to Modal `/execute`
- `POST /search` — wraps params and proxies to Modal `/execute`
- `POST /load` — wraps params and proxies to Modal `/execute`
- `POST /sign` — wraps params and proxies to Modal `/execute`
- `POST /objects/{action}` — CAS sync passthrough (has/put/get)
- `POST /push` — push passthrough
- `GET /health` — 200 OK

## Environment Variables

- `RYEOS_REMOTE_MODAL_URL` — Base URL of the Modal ryeos-remote service

## Running

```bash
RYEOS_REMOTE_MODAL_URL=https://... uvicorn ryeos_remote_mcp.server:app --host 0.0.0.0 --port 8000
```
