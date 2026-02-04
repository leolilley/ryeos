# Registry API Service

Python backend service that handles registry operations with server-side validation.

## Overview

The Registry API is a FastAPI service that:

1. Receives push requests from `rye` clients
2. Validates content using the same `rye` validation pipeline
3. Signs content with registry provenance (`|registry@username`)
4. Stores in Supabase database

## Why Python?

The `rye` package (`pip install rye-os`) contains the validation logic. By using a Python backend, we:

- **Reuse existing validators**: Same `validate_parsed_data()` function used client-side
- **Single source of truth**: Validation rules defined once in extractors
- **No duplication**: No need to port validation to TypeScript/plpgsql

## Signing Flow

### Two-Stage Signing

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              CLIENT SIDE                                     │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  1. User creates/edits item                                                 │
│  2. User runs `rye sign` (or push auto-signs)                              │
│  3. Client validates content using extractors                               │
│  4. Client signs: rye:validated:2026-02-04T10:00:00Z:abc123...             │
│  5. Client pushes to registry API                                           │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                              SERVER SIDE                                     │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  1. Authenticate user via Supabase JWT                                      │
│  2. Strip existing signature from content                                   │
│  3. Re-validate content using rye validators (same as client)              │
│  4. If validation fails → return error (content rejected)                   │
│  5. If validation passes → sign with registry provenance:                   │
│                                                                             │
│     rye:validated:2026-02-04T10:00:05Z:def456...|registry@leo              │
│                                                                             │
│  6. Insert signed content into database                                     │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Signature Formats

| Context      | Format                                            | Example                                                      |
| ------------ | ------------------------------------------------- | ------------------------------------------------------------ |
| **Local**    | `rye:validated:timestamp:hash`                    | `rye:validated:2026-02-04T10:00:00Z:abc123...`               |
| **Registry** | `rye:validated:timestamp:hash\|registry@username` | `rye:validated:2026-02-04T10:00:00Z:abc123...\|registry@leo` |

### Trust Model

The `|registry@username` suffix can **only** be added by the registry server after:

1. Verifying the user's authentication (valid Supabase JWT)
2. Verifying the user owns the item or has publish rights
3. Validating the content passes all schema checks

This means:

- **Local signatures**: User claims "I validated this"
- **Registry signatures**: Registry claims "I validated this AND user `@username` published it"

### Why Not Cryptographic Signing?

The current approach uses **hash-based integrity** (SHA256), not cryptographic signatures. This is sufficient because:

1. **Server-side trust**: The registry server is trusted to only add `|registry@username` for authenticated users
2. **Same as npm/PyPI**: Major package registries use server-side authentication, not client-side crypto
3. **Simpler key management**: No need for user keypairs, rotation, revocation

**Future option**: The `signing_keys` table exists for cryptographic signing if needed:

```
rye:validated:timestamp:hash|registry@username:ed25519_signature
```

This would allow offline verification without trusting the registry server.

---

## API Endpoints

### POST /v1/push

Push an item to the registry.

**Request:**

```json
{
  "item_type": "directive|tool|knowledge",
  "item_id": "my-directive",
  "content": "<!-- rye:validated:... -->\n<directive name=\"my-directive\">...",
  "version": "1.0.0",
  "changelog": "Initial release",
  "metadata": {
    "category": "core",
    "tags": ["automation"]
  }
}
```

**Response (success):**

```json
{
  "status": "published",
  "item_type": "directive",
  "item_id": "my-directive",
  "version": "1.0.0",
  "signature": {
    "timestamp": "2026-02-04T10:00:05Z",
    "hash": "def456...",
    "registry_username": "leo"
  }
}
```

**Response (validation error):**

```json
{
  "status": "error",
  "error": "Validation failed",
  "issues": [
    "Missing required field: description",
    "Field 'version' must be semver format (X.Y.Z), got 'v1'"
  ]
}
```

### GET /v1/pull/{item_type}/{item_id}

Pull an item from the registry.

**Response:**

```json
{
  "item_type": "directive",
  "item_id": "my-directive",
  "version": "1.0.0",
  "content": "<!-- rye:validated:2026-02-04T10:00:05Z:def456...|registry@leo -->\n...",
  "author": "leo",
  "signature": {
    "timestamp": "2026-02-04T10:00:05Z",
    "hash": "def456...",
    "registry_username": "leo"
  }
}
```

---

## Implementation

### Dependencies

```python
# requirements.txt
fastapi>=0.100.0
uvicorn>=0.23.0
supabase>=2.0.0
rye-os>=1.0.0  # The rye package with validators
python-jose>=3.3.0  # JWT validation
```

### Core Service

```python
# registry_api/main.py
from fastapi import FastAPI, Depends, HTTPException
from supabase import create_client
from rye.utils.validators import validate_parsed_data, apply_field_mapping
from rye.utils.parser_router import ParserRouter
from rye.utils.metadata_manager import MetadataManager

app = FastAPI(title="RYE Registry API")
parser_router = ParserRouter()

@app.post("/v1/push")
async def push_item(request: PushRequest, user: User = Depends(get_current_user)):
    """Validate and publish an item to the registry."""

    # 1. Strip existing signature
    content = MetadataManager.remove_signature(request.item_type, request.content)

    # 2. Parse content
    parser_type = get_parser_for_type(request.item_type)
    parsed = parser_router.parse(parser_type, content)
    if "error" in parsed:
        raise HTTPException(400, detail={"error": "Parse failed", "details": parsed["error"]})

    # 3. Apply field mapping (same as client)
    parsed = apply_field_mapping(request.item_type, parsed)

    # 4. Validate using rye validators (same as client sign tool)
    validation = validate_parsed_data(
        item_type=request.item_type,
        parsed_data=parsed,
        file_path=None,  # No file path on server
        location="registry",
    )

    if not validation["valid"]:
        raise HTTPException(400, detail={
            "error": "Validation failed",
            "issues": validation["issues"]
        })

    # 5. Sign with registry provenance
    signed_content = sign_with_registry(content, request.item_type, user.username)

    # 6. Insert to database
    await insert_version(request, signed_content, user)

    return {"status": "published", ...}


def sign_with_registry(content: str, item_type: str, username: str) -> str:
    """Sign content with registry provenance."""
    import hashlib
    from datetime import datetime, timezone

    # Compute hash of content (without signature)
    content_hash = hashlib.sha256(content.encode()).hexdigest()
    timestamp = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    # Registry signature format
    signature = f"{timestamp}:{content_hash}|registry@{username}"

    # Inject using MetadataManager (handles comment format)
    return MetadataManager.sign_content_with_signature(item_type, content, signature)
```

### Deployment

The Registry API can be deployed as:

1. **Supabase Edge Function companion**: Runs alongside Supabase, called via HTTP
2. **Standalone service**: Deployed on Railway, Fly.io, or similar
3. **Docker container**: Self-hosted alongside database

```yaml
# docker-compose.yml
services:
  registry-api:
    build: ./registry-api
    environment:
      SUPABASE_URL: ${SUPABASE_URL}
      SUPABASE_SERVICE_KEY: ${SUPABASE_SERVICE_KEY}
    ports:
      - "8000:8000"
```

---

## Client Integration

The `registry.py` tool's `_push()` function should:

1. **Auto-sign locally** (same as `sign` tool) - validates and adds local signature
2. **Push to registry API** - server re-validates and adds registry signature
3. **No `|registry@username` handling** - server handles this

```python
# In registry.py _push()
async def _push(item_type: str, item_id: str, ...):
    # 1. Load and validate locally (same as sign tool)
    content = path.read_text()
    validation = validate_parsed_data(item_type, parsed, file_path, ...)
    if not validation["valid"]:
        return {"status": "error", "issues": validation["issues"]}

    # 2. Sign locally
    signed_content = MetadataManager.sign_content(item_type, content, ...)

    # 3. Push to registry API (server will re-validate and add registry signature)
    response = await http.post(
        f"{registry_url}/v1/push",
        json={
            "item_type": item_type,
            "item_id": item_id,
            "content": signed_content,
            "version": version,
        },
        auth_token=token,
    )

    # 4. Server returns content with registry signature
    # Update local file with registry-signed version
    if response["status"] == "published":
        path.write_text(response["signed_content"])

    return response
```

---

## Validation on Pull

When pulling content, the client should verify:

1. **Signature exists**: Content has `|registry@username` suffix
2. **Hash matches**: Recompute hash and compare
3. **Username matches author**: Signature username matches DB author field

```python
# In registry.py _pull()
async def _pull(item_type: str, item_id: str, ...):
    response = await http.get(f"{registry_url}/v1/pull/{item_type}/{item_id}")

    content = response["content"]
    author = response["author"]

    # Verify signature
    sig_info = MetadataManager.extract_signature(item_type, content)
    if not sig_info:
        return {"status": "error", "error": "No signature found"}

    if sig_info.get("registry_username") != author:
        return {"status": "error", "error": "Signature username mismatch"}

    # Verify hash
    content_without_sig = MetadataManager.remove_signature(item_type, content)
    computed_hash = hashlib.sha256(content_without_sig.encode()).hexdigest()
    if computed_hash != sig_info["hash"]:
        return {"status": "error", "error": "Content integrity check failed"}

    # Write to local
    dest.write_text(content)
    return {"status": "pulled", ...}
```
