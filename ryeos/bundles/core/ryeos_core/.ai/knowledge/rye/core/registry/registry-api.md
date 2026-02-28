<!-- rye:signed:2026-02-28T00:32:39Z:4fa7a49a558ae129a915c8f94e8505e20e22e41e279bf3f465fe9db4bf143b26:Qfu0aVoENcjNtBqO8OoQN-ME3oyPOLIdrsuyJ8l7knu6Os_a-CVU4REdsB37E-1rIH2auR6ljpaNHQ8SXJPdBw==:4b987fd4e40303ac -->

```yaml
name: registry-api
title: Registry API Reference
entry_type: reference
category: rye/core/registry
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - registry
  - api
  - sharing
  - distribution
references:
  - trust-model
  - "docs/registry/sharing-items.md"
  - "docs/registry/agent-integration.md"
```

# Registry API Reference

Endpoints, auth flow, and semantics for the Rye OS item registry.

## Service Overview

- **Server:** FastAPI at `services/registry-api/registry_api/`
- **Client tool:** `.ai/tools/rye/core/registry/registry.py`
- **Database:** Supabase (PostgreSQL)
- **Auth:** JWT (OAuth PKCE via GitHub)
- **Deployed:** Railway (or equivalent)

## API Endpoints

| Method | Path                                         | Description                           | Auth     |
| ------ | -------------------------------------------- | ------------------------------------- | -------- |
| GET    | `/health`                                    | Health check (DB connectivity)        | None     |
| GET    | `/v1/public-key`                             | Registry Ed25519 public key (PEM)     | None     |
| POST   | `/v1/push`                                   | Validate, sign, store an item         | Required |
| GET    | `/v1/pull/{item_type}/{item_id}`             | Download an item                      | Required |
| GET    | `/v1/search`                                 | Search items by query                 | Optional |
| DELETE | `/v1/items/{item_type}/{item_id}`            | Delete an item                        | Required |
| PATCH  | `/v1/items/{item_type}/{item_id}/visibility` | Set visibility (publish/unpublish)    | Required |
| POST   | `/v1/bundle/push`                            | Push bundle (manifest + files)        | Required |
| GET    | `/v1/bundle/pull/{bundle_id}`                | Pull bundle (manifest + files)        | Required |

## Item Identity

Canonical ID format: `{namespace}/{category}/{name}`

```
"leolilley/rye/core/registry/registry"
→ namespace = "leolilley"
→ category  = "rye/core/registry"
→ name      = "registry"
```

**Parsing rule:** first segment = namespace, last segment = name, everything between = category.

## Authentication

### Device Code Auth Flow

```
login  →  opens browser for OAuth PKCE (GitHub), waits with initial delay,
          polls for completion with a grace period for 404s, receives JWT
```

### Client Auth Actions

| Action        | Description                                              |
| ------------- | -------------------------------------------------------- |
| `signup`      | Create account with email/password                       |
| `login`       | Device auth flow (opens browser, polls for completion)   |
| `login_email` | Login via email/password                                 |
| `logout`      | Clear local auth session                                 |
| `whoami`      | Show current authenticated user                          |

### Server Auth

JWT-based. The `get_current_user` dependency extracts user from `Authorization` header. Some endpoints (search, public-key) allow unauthenticated access.

## Namespaces

- **User-scoped:** Each user has exactly one namespace matching their username
- **Push enforcement:** You can only push to your own namespace. `POST /v1/push` with `item_id="otheruser/..."` returns **403**
- **Delete enforcement:** Only the namespace owner can delete items
- **Namespace on pull:** Stripped from local path — `leolilley/utils/tool` → `.ai/tools/utils/tool.py`

⚠️ **Conflict risk:** `leolilley/utils/tool` and `otheruser/utils/tool` both write to `.ai/tools/utils/tool.py`

## Push Semantics

### Client-Side Flow

1. Read local item file from project or user space
2. Strip any existing signature
3. Send to `POST /v1/push` with auth token
4. Receive confirmation with item ID and version

### Server-Side Flow

1. **Namespace verification** — Authenticated user must match namespace
2. **Strip signature** — `strip_signature(content, item_type)` removes existing `rye:signed:` comment
3. **Validate content** — Structural validation (metadata fields, format)
4. **Registry signing** — `sign_with_registry(content, item_type, username)`:
   - SHA256 of normalized content
   - Ed25519 signature with registry private key
   - Appends `|rye-registry@{username}` provenance
5. **Database upsert** — Stores in Supabase table
6. **Version tracking** — Creates version record, marks previous versions as not latest

### Registry Signature Format

```
rye:signed:TIMESTAMP:HASH:SIG:FP|rye-registry@username
```

## Pull Semantics

### Client-Side Flow

1. `GET /v1/pull/{item_type}/{item_id}` — optionally with `?version=X`
2. Verify registry Ed25519 signature (hash match + signature validation)
3. On first pull, TOFU-pin registry public key as a trusted key identity document with `owner="rye-registry"`
4. Write to target space: `.ai/{item_type_plural}/{category}/{name}.{ext}`
5. Registry signature preserved in file

### File Destination Rules

| Item Type   | Registry ID                        | Local Path                                |
| ----------- | ---------------------------------- | ----------------------------------------- |
| Tool        | `leolilley/utilities/web-scraper`  | `.ai/tools/utilities/web-scraper.py`      |
| Directive   | `leolilley/core/bootstrap`         | `.ai/directives/core/bootstrap.md`        |
| Knowledge   | `leolilley/patterns/retry-backoff` | `.ai/knowledge/patterns/retry-backoff.md` |

### Version Resolution

- Default: latest version
- Explicit: `?version=1.0.0`
- Server marks `is_latest` flag on version records

## Search Semantics

```
GET /v1/search?query=web+scraper&item_type=tool&namespace=leolilley&limit=20&offset=0
```

| Parameter      | Required | Description                                         |
| -------------- | -------- | --------------------------------------------------- |
| `query`        | Yes      | Case-insensitive match on `name` and `description`  |
| `item_type`    | No       | Filter by type (`tool`, `directive`, `knowledge`)   |
| `namespace`    | No       | Filter by namespace                                 |
| `limit`        | No       | Max results (default 20)                            |
| `offset`       | No       | Pagination offset                                   |
| `include_mine` | No       | Include own private items (requires auth)           |

**Visibility:** Only public items by default. Authenticated users can add `include_mine=true`.

**Category filtering:** Uses prefix matching for nested categories.

## Visibility Control

```
PATCH /v1/items/{item_type}/{item_id}/visibility
```

| Action      | Sets visibility to | Effect                 |
| ----------- | ------------------ | ---------------------- |
| `publish`   | `public`           | Visible to all users   |
| `unpublish` | `private`          | Visible only to owner  |

## Bundle Operations

### Bundle Push

`POST /v1/bundle/push` — stores manifest and all files as JSONB. Each version tracks:
- Content hash (SHA256 of manifest)
- `is_latest` flag

### Bundle Pull

`GET /v1/bundle/pull/{bundle_id}` — returns manifest and all files as JSON. Increments download count on each pull.

### Bundle Pull Client Flow

1. `GET /v1/bundle/pull/{bundle_id}` → manifest + files JSON
2. Write manifest to `.ai/bundles/{bundle_id}/manifest.yaml`
3. Write each file to its relative path
4. `verify_item(manifest_path, ItemType.TOOL)` — manifest Ed25519 check
5. `validate_bundle_manifest()` — per-file SHA256 comparison

### Client Bundle Actions

| Action        | Description                                      |
| ------------- | ------------------------------------------------ |
| `push_bundle` | Push a bundle (manifest + files) to the registry |
| `pull_bundle` | Pull a bundle from the registry to local space   |

## Download Counting

Pull endpoints increment a download counter per item/bundle version on each successful pull.

## Database Schema

| Table                | Purpose                                        |
| -------------------- | ---------------------------------------------- |
| `users`              | User accounts (id, username)                   |
| `tools`              | Tool registry entries                          |
| `directives`         | Directive registry entries                     |
| `knowledge`          | Knowledge registry entries                     |
| `tool_versions`      | Tool version history                           |
| `directive_versions` | Directive version history                      |
| `knowledge_versions` | Knowledge version history                      |
| `bundles`            | Bundle metadata                                |
| `bundle_versions`    | Bundle version history (manifest + files)      |

## API Key Authentication

In addition to JWT (OAuth), the registry supports API key authentication for programmatic access and CI/CD pipelines.

### API Key Endpoints

| Method | Path                  | Description              | Auth     |
| ------ | --------------------- | ------------------------ | -------- |
| POST   | `/v1/api-keys`        | Create a new API key     | Required |
| GET    | `/v1/api-keys`        | List user's API keys     | Required |
| DELETE | `/v1/api-keys/{id}`   | Revoke an API key        | Required |

### API Key Format

API keys use the `rye_sk_` prefix (e.g., `rye_sk_a1b2c3d4...`). Pass via the `Authorization: Bearer rye_sk_...` header. API keys replace the deprecated `RYE_REGISTRY_TOKEN` environment variable — use `RYE_REGISTRY_API_KEY` instead.

### Client API Key Actions

| Action           | Description                              |
| ---------------- | ---------------------------------------- |
| `create_api_key` | Create a new API key (`rye_sk_` prefix)  |
| `list_api_keys`  | List all API keys for the current user   |
| `revoke_api_key` | Revoke an API key by ID                  |

## Server Configuration

| Variable               | Description               |
| ---------------------- | ------------------------- |
| `SUPABASE_URL`         | Supabase project URL      |
| `SUPABASE_SERVICE_KEY` | Supabase service role key |
| `HOST`                 | Server bind host          |
| `PORT`                 | Server bind port          |
| `LOG_LEVEL`            | Logging level             |

## Agent Integration

### Local vs Registry Search

| Mechanism                                  | Scope                               | Network |
| ------------------------------------------ | ----------------------------------- | ------- |
| `rye_search(query=...)`                    | Project `.ai/`, user `~/.ai/`, system | No   |
| `rye_execute(item_id="rye/core/registry/registry", action="search")` | Registry server | Yes |

Search is **explicit** — agents must consciously invoke the registry tool. No implicit network calls.

### Pull-and-Use Pattern

```python
# 1. Search registry
rye_execute(item_id="rye/core/registry/registry",
            parameters={"action": "search", "query": "web scraper", "item_type": "tool"})

# 2. Pull to project
rye_execute(item_id="rye/core/registry/registry",
            parameters={"action": "pull", "item_type": "tool",
                        "item_id": "leolilley/utilities/web-scraper", "space": "project"})

# 3. Execute locally
rye_execute(item_id="utilities/web-scraper",
            parameters={"url": "https://example.com"})
```

## Known Limitations

| Limitation                        | Description                                                        |
| --------------------------------- | ------------------------------------------------------------------ |
| No automatic registry search      | `rye_search` is local-only; agent must explicitly call registry    |
| Auth required for pull             | Most operations need authentication via `action: login`            |
| No dependency resolution           | Pulling an item does not pull its dependencies                     |
| No auto-update                     | Pulled items are static snapshots                                  |
| Namespace stripped on pull          | Can cause conflicts between same-named items from different users  |
