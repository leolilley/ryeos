# Registry Services

Backend services that power the RYE registry. These services run server-side to ensure security and validation integrity.

## Quick Links

- **[Deployment Guide](./DEPLOYMENT.md)** - Complete setup for production and local development
- **[Registry API](./registry-api.md)** - API endpoints and signing flow documentation

## Architecture Overview

```
┌─────────────────┐     ┌──────────────────────┐     ┌─────────────┐
│  Client (MCP)   │────▶│  Registry API        │────▶│  Supabase   │
│  registry tool  │     │  (FastAPI/Python)    │     │  (RLS)      │
└─────────────────┘     └──────────────────────┘     └─────────────┘
        │                        │
        │                        ├── Authenticates user (JWT)
        │                        ├── Validates content (rye validators)
        │                        ├── Signs with registry provenance
        │                        └── Inserts via service role key
        │
        └── Signs locally (standard rye signature)
            Pushes content + metadata
```

All registry operations (search, pull, push) go through the Registry API. Direct database access is blocked by RLS policies.

## Services

| Service          | Purpose                             | Documentation                        |
| ---------------- | ----------------------------------- | ------------------------------------ |
| **registry-api** | Handles push/pull/search operations | [registry-api.md](./registry-api.md) |

## Why Server-Side Validation?

The client-side `sign` tool validates and signs content locally. However, for registry publishing, we need **server-side validation** because:

1. **Prevent Malicious Content**: A modified client could skip validation and push invalid/malicious content
2. **Trusted Provenance**: Only the server can add the `|registry@username` suffix after verifying authentication
3. **Consistent Validation**: Same validation rules apply to all publishers
4. **Single API Surface**: All operations go through one endpoint, simplifying security

## Database Security

RLS (Row Level Security) is enabled on all tables:

- **Public read**: Anyone can read published items
- **Write via API only**: Direct inserts/updates blocked; must go through Registry API
- **Service role bypass**: API uses service role key to write

See [../schema/003_rls_api_only.sql](../schema/003_rls_api_only.sql) for RLS policies.

## Getting Started

See **[DEPLOYMENT.md](./DEPLOYMENT.md)** for:

- Supabase setup
- Registry API deployment
- Local development
- Security checklist
