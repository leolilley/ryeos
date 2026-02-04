# Registry API

Server-side validation and signing service for the RYE registry.

## Overview

This service handles registry push/pull operations with:
- **Server-side validation** using the same `rye` validators as the client
- **Registry signing** - adds `|registry@username` suffix after authentication
- **Content integrity** - verifies hashes on pull

## Quick Start

```bash
# Install dependencies
pip install -e .

# Set environment variables
export SUPABASE_URL="https://your-project.supabase.co"
export SUPABASE_SERVICE_KEY="your-service-role-key"
export SUPABASE_JWT_SECRET="your-jwt-secret"

# Run development server
uvicorn registry_api.main:app --reload --port 8000
```

## API Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/v1/push` | Validate and publish an item |
| GET | `/v1/pull/{item_type}/{item_id}` | Pull an item with verification |
| GET | `/v1/search` | Search registry items |
| GET | `/health` | Health check |

## Architecture

```
Client (rye registry push)
    │
    ▼
POST /v1/push
    │
    ├── 1. Authenticate via Supabase JWT
    ├── 2. Parse content (markdown_xml, python_ast, etc.)
    ├── 3. Validate using rye validators
    ├── 4. Sign with |registry@username
    └── 5. Insert to Supabase database
```

## Configuration

| Variable | Required | Description |
|----------|----------|-------------|
| `SUPABASE_URL` | Yes | Supabase project URL |
| `SUPABASE_SERVICE_KEY` | Yes | Service role key for DB access |
| `SUPABASE_JWT_SECRET` | Yes | JWT secret for token validation |
| `ALLOWED_ORIGINS` | No | CORS origins (default: `*`) |
| `LOG_LEVEL` | No | Logging level (default: `INFO`) |

## Development

```bash
# Install dev dependencies
pip install -e ".[dev]"

# Run tests
pytest

# Run with auto-reload
uvicorn registry_api.main:app --reload
```

## Docker

```bash
# Build
docker build -t registry-api .

# Run
docker run -p 8000:8000 \
  -e SUPABASE_URL=... \
  -e SUPABASE_SERVICE_KEY=... \
  -e SUPABASE_JWT_SECRET=... \
  registry-api
```

## Documentation

See [/docs/db/services/registry-api.md](../../docs/db/services/registry-api.md) for full documentation.
