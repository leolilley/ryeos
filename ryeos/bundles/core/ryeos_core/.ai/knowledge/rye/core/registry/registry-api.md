<!-- rye:signed:2026-04-09T00:09:13Z:99ab4c088b38bb26900f8428ac4eb14115556397903ad033405a944208000412:-OyhXqh0uT-SYZXsiiavgcHCKq1mYGqrNhkAnB3faBm3S8hvTsL44Y8JsTKMIC8zHyj5YFYF5rKPugkHaiEUDw:4b987fd4e40303ac -->

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

- **Server:** Built into ryeos-node at `ryeos-node/ryeos_node/`
- **Client tool:** `.ai/tools/rye/core/registry/registry.py`
- **Storage:** CAS-native (content-addressed, no external database)
- **Auth:** Ed25519 signed requests + API keys
- **Deployed:** Modal (as part of ryeos-node)

## API Endpoints

Registry endpoints are hosted on ryeos-node under the `/registry/*` path:

| Method | Path                                                  | Description                           | Auth     |
| ------ | ----------------------------------------------------- | ------------------------------------- | -------- |
| GET    | `/health`                                             | Health check                          | None     |
| GET    | `/public-key`                                         | Server Ed25519 public key (PEM)       | None     |
| POST   | `/registry/push`                                      | Validate and store an item            | Required |
| GET    | `/registry/pull/{item_type}/{item_id}`                | Download an item                      | Required |
| GET    | `/registry/search`                                    | Search items by query                 | Optional |
| DELETE | `/registry/items/{item_type}/{item_id}`               | Delete an item                        | Required |
| PATCH  | `/registry/items/{item_type}/{item_id}/visibility`    | Set visibility (publish/unpublish)    | Required |
| POST   | `/registry/bundle/push`                               | Push bundle (manifest + files)        | Required |
| GET    | `/registry/bundle/pull/{bundle_id}`                   | Pull bundle (manifest + files)        | Required |
| GET    | `/registry/bundle/search`                             | Search bundles by query               | Optional |
| POST   | `/registry/bundle/{bundle_id}/visibility`             | Set bundle visibility                 | Required |
| POST   | `/registry/identity`                                  | Register identity (namespace claim)   | Required |
| GET    | `/registry/identity/{namespace}`                      | Look up namespace owner               | None     |

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

Ed25519 signed requests and API keys. The `get_current_user` dependency extracts user from the `Authorization` header. Some endpoints (search, public-key, identity lookup) allow unauthenticated access.

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
4. **Signature verification** — Verifies the publisher's Ed25519 signature (publisher's signature is sole provenance — no re-signing by the registry)
5. **CAS storage** — Stores in the content-addressed registry
6. **Version tracking** — Creates version record, marks previous versions as not latest

### Signature Provenance

The publisher's Ed25519 signature is preserved as-is — the registry does not re-sign items. Provenance is established by the publisher's keypair and namespace claim.

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
| `include_mine` | No       | Include own unlisted items (requires auth)          |

**Visibility:** Only public items by default. Authenticated users can add `include_mine=true`.

**Category filtering:** Uses prefix matching for nested categories.

## Visibility Control

```
PATCH /v1/items/{item_type}/{item_id}/visibility
```

| Action      | Sets visibility to | Effect                             |
| ----------- | ------------------ | ---------------------------------- |
| `publish`   | `public`           | Visible in search, accessible      |
| `unpublish` | `unlisted`         | Not in search, accessible by ID    |

## Bundle Operations

### Bundle Push

`POST /v1/bundle/push` — stores manifest and all files as JSONB. Each version tracks:
- Content hash (CAS `object_hash` of manifest)
- `is_latest` flag

### Bundle Pull

`GET /v1/bundle/pull/{bundle_id}` — returns manifest and all files as JSON. Increments download count on each pull.

### Bundle Search

```
GET /v1/bundle/search?query=...&namespace=...&include_mine=true&limit=20
```

Same query parameters as item search but scoped to bundles. Only public bundles by default; authenticated users can add `include_mine=true`.

### Bundle Visibility

```
POST /v1/bundle/{bundle_id}/visibility
```

Same pattern as item visibility but scoped to bundles.

| Action      | Sets visibility to | Effect                             |
| ----------- | ------------------ | ---------------------------------- |
| `publish`   | `public`           | Visible in search, accessible      |
| `unpublish` | `unlisted`         | Not in search, accessible by ID    |

New bundles default to `public` on push.

### Bundle Pull Client Flow

1. `GET /v1/bundle/pull/{bundle_id}` → manifest + files JSON
2. Write manifest to `.ai/bundles/{bundle_id}/manifest.yaml`
3. Write each file to its relative path
4. `verify_item(manifest_path, ItemType.TOOL)` — manifest Ed25519 check
5. `validate_bundle_manifest()` — re-ingests files via CAS, compares `object_hash`
6. Write `install-receipt.json` — records installed files for clean uninstall

### Client Bundle Actions

| Action            | Description                                      |
| ----------------- | ------------------------------------------------ |
| `push_bundle`     | Push a bundle (manifest + files) to the registry |
| `pull_bundle`     | Pull a bundle from the registry to local space   |
| `search_bundle`   | Search bundles in registry                       |

## Download Counting

Pull endpoints increment a download counter per item/bundle version on each successful pull.

## Storage

The registry uses CAS-native storage on ryeos-node. Items and bundles are stored as content-addressed objects with metadata indexed for search and version tracking. Identity is managed via Ed25519 keypairs and namespace claims — no external database.

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

Registry configuration is part of ryeos-node's config (`ryeos_node/config.py`):

| Variable               | Description               |
| ---------------------- | ------------------------- |
| `HOST`                 | Server bind host          |
| `PORT`                 | Server bind port          |
| `LOG_LEVEL`            | Logging level             |

## Agent Integration

### Local vs Registry Search

| Mechanism                                  | Scope                               | Network |
| ------------------------------------------ | ----------------------------------- | ------- |
| `rye_fetch(query=...)`                     | Project `.ai/`, user `~/.ai/`, system | No   |
| `rye_execute(item_id="rye/core/registry/registry", action="search")` | Registry server | Yes |

Search is **explicit** — agents must consciously invoke the registry tool. No implicit network calls.

### Bundle Workflow

```python
# Search for bundles
rye_execute(item_id="rye/core/registry/registry",
            parameters={"action": "search_bundle", "query": "ryeos"})

# Publish a bundle
rye_execute(item_id="rye/core/registry/registry",
            parameters={"action": "publish_bundle", "bundle_id": "my-bundle"})
```

### CLI Install/Uninstall

```bash
rye install my-bundle[@version]    # Pull + verify + materialize
rye uninstall my-bundle            # Remove installed files via install receipt
```

Items are merged into `.ai/tools/`, `.ai/directives/`, etc. — found via normal space resolution.

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
| No automatic registry search      | `rye_fetch` is local-only; agent must explicitly call registry     |
| Auth required for pull             | Most operations need authentication via `action: login`            |
| No dependency resolution           | Pulling an item does not pull its dependencies                     |
| No auto-update                     | Pulled items are static snapshots                                  |
| Namespace stripped on pull          | Can cause conflicts between same-named items from different users  |
