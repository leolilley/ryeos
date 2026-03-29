# RYE Services

Backend services that power the RYE ecosystem.

## Services

| Service | Port | Description |
|---------|------|-------------|
| [ryeos-node](./ryeos-node/) | 8000 | Execution node — CAS sync, directive/tool execution, webhooks, registry (Ed25519 auth) |

## Architecture

```
┌─────────────────┐     ┌──────────────────────┐     ┌─────────────┐
│  Client (rye)   │────▶│  ryeos-node           │────▶│  Local CAS  │
│  CLI / MCP      │     │  (FastAPI + Ed25519)  │     │  + Registry │
└─────────────────┘     └──────────────────────┘     └─────────────┘
```

## Deploy

```bash
# Modal
cd ryeos-node && modal deploy modal_app.py

# Docker
cd ryeos-node && docker compose up

# Local
cd ryeos-node && ./run.sh
```

See [ryeos-node/README.md](./ryeos-node/README.md) for full deployment docs.
