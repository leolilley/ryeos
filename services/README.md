# RYE Services

Backend services that power the RYE ecosystem.

## Services

| Service | Port | Description |
|---------|------|-------------|
| [ryeos-node](./ryeos-node/) | 8000 | Remote execution, CAS sync, and registry features (Ed25519 auth) |

## Architecture

```
┌─────────────────┐     ┌──────────────────────┐     ┌─────────────┐
│  Client (rye)   │────▶│  ryeos-node           │────▶│  Local CAS  │
│  CLI / MCP      │     │  (FastAPI + Ed25519)  │     │  + Registry │
└─────────────────┘     └──────────────────────┘     └─────────────┘
```

## Key Principle: Single Source of Truth

All services import and use the `rye` package directly:

```python
from rye.utils.validators import validate_parsed_data
from rye.utils.metadata_manager import MetadataManager
from rye.utils.parser_router import ParserRouter
```

This ensures:
- **Consistent validation** between client and server
- **No duplication** of validation schemas or parsing logic
- **Easy updates** - change validation in one place

## Development

```bash
# Start all services
docker-compose up

# Start specific service
cd registry-api
pip install -e .
uvicorn registry_api.main:app --reload
```

## Documentation

See [/docs/db/services/](../docs/db/services/) for detailed service documentation.
