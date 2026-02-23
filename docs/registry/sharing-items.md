```yaml
id: sharing-items
title: "Sharing Items via Registry"
description: Push, pull, and discover items through the Rye OS registry
category: registry
tags: [registry, sharing, publish, pull, search]
version: "1.0.0"
```

# Sharing Items via Registry

The Rye OS registry is a centralized store for sharing directives, tools, and knowledge across projects and users. It combines a client-side tool (bundled with Rye) and a server-side FastAPI service for validation, signing, and storage.

## Concepts

### Item Identity

Every registry item has a canonical ID with three parts:

```
{namespace}/{category}/{name}
```

| Part        | Description                                        | Example             |
| ----------- | -------------------------------------------------- | ------------------- |
| `namespace` | Owner (first segment, no slashes)                  | `leolilley`         |
| `category`  | Folder path (middle segments, may contain slashes) | `rye/core/registry` |
| `name`      | Basename (last segment, no slashes)                | `registry`          |

Parsing rule: first segment = namespace, last segment = name, everything in between = category.

```
"leolilley/rye/core/registry/registry"
→ namespace = "leolilley"
→ category  = "rye/core/registry"
→ name      = "registry"
```

### Visibility

Items are either `public` (visible to all users) or `private` (visible only to the owner). Visibility is controlled via the `publish` and `unpublish` actions.

## The Registry Tool

**Location:** `.ai/tools/rye/core/registry/registry.py`

The registry tool is a Python script that runs through the executor chain like any other tool:

```python
__executor_id__ = "rye/core/runtimes/python/script"
```

### Authentication Actions

| Action       | Description                                                       |
| ------------ | ----------------------------------------------------------------- |
| `signup`     | Create account with email/password                                |
| `login`      | Start device auth flow (opens browser, supports OAuth via GitHub) |
| `login_poll` | Poll for auth completion after device flow starts                 |
| `logout`     | Clear local auth session                                          |
| `whoami`     | Show current authenticated user                                   |

Authentication uses OAuth PKCE flow with GitHub as the primary provider. The device auth flow opens a browser for login and polls for completion.

### Item Operations

#### Push (Publish to Registry)

Upload a local item to the registry:

```
action: push
item_type: tool
item_id: leolilley/utilities/my-tool
version: 1.0.0
```

The push flow:

1. Read the local item file from project or user space
2. Strip any existing signature
3. Send to registry API with authentication
4. Server validates content structure
5. Server signs with registry Ed25519 key (adds `|registry@username` provenance)
6. Server stores in Supabase database
7. Client receives confirmation with item ID and version

**Namespace enforcement:** You can only push to your own namespace. Attempting to push to `otheruser/...` returns a 403 error.

#### Pull (Download from Registry)

Download an item from the registry to local space:

```
action: pull
item_type: tool
item_id: leolilley/utilities/my-tool
space: project   # or "user"
```

The pull flow:

1. Request item from registry API by type and ID
2. Optionally specify version (defaults to latest)
3. Receive signed content with registry provenance
4. Write to the target space (`.ai/tools/{category}/{name}.py` in project or user space)
5. The item retains its registry signature for verification

#### Search

Find items in the registry:

```
action: search
query: "web scraper"
item_type: tool        # optional filter
namespace: leolilley   # optional filter
```

Search matches against item names and descriptions. By default only public items are returned. Authenticated users can include their own private items with `include_mine=true`.

#### Delete

Remove an item from the registry:

```
action: delete
item_type: tool
item_id: leolilley/utilities/my-tool
```

Only the namespace owner can delete items.

#### Publish / Unpublish

Control visibility:

```
action: publish     # Make public (visibility='public')
action: unpublish   # Make private (visibility='private')
item_type: tool
item_id: leolilley/utilities/my-tool
```

## Registry API Service

**Location:** `services/registry-api/registry_api/`

A separate FastAPI application that handles server-side operations. Deployed independently (on Railway) with Supabase as the database backend.

### Endpoints

| Method | Path                                         | Description                                        | Auth     |
| ------ | -------------------------------------------- | -------------------------------------------------- | -------- |
| GET    | `/health`                                    | Health check (database connectivity)               | None     |
| GET    | `/v1/public-key`                             | Registry Ed25519 public key (PEM) for TOFU pinning | None     |
| POST   | `/v1/push`                                   | Validate, sign, and store an item                  | Required |
| GET    | `/v1/pull/{item_type}/{item_id}`             | Download an item                                   | Required |
| GET    | `/v1/search`                                 | Search items by query                              | Optional |
| DELETE | `/v1/items/{item_type}/{item_id}`            | Delete an item                                     | Required |
| PATCH  | `/v1/items/{item_type}/{item_id}/visibility` | Set visibility                                     | Required |
| POST   | `/v1/bundle/push`                            | Push a bundle (manifest + files)                   | Required |
| GET    | `/v1/bundle/pull/{bundle_id}`                | Pull a bundle                                      | Required |

### Push Flow (Server-Side)

When the registry receives a push request:

1. **Namespace verification** — Confirms the authenticated user matches the namespace
2. **Strip signature** — Removes any existing signature from content
3. **Validate content** — Runs structural validation (metadata fields, format)
4. **Registry signing** — Signs with the registry's Ed25519 key, adding `|registry@username` provenance
5. **Database upsert** — Stores/updates in the appropriate Supabase table
6. **Version tracking** — Creates version record, marks previous versions as not latest

### Search Endpoint

```
GET /v1/search?query=web+scraper&item_type=tool&namespace=leolilley&limit=20&offset=0
```

Search performs case-insensitive matching on `name` and `description` fields. Category filtering uses prefix matching to support nested categories. Visibility filtering shows public items by default; authenticated users can include their own private items.

### Bundle Operations

Bundles are versioned collections of items (manifest + files) that can be pushed and pulled as a unit:

**Push:** Stores the manifest and all bundle files as JSONB in Supabase. Each version tracks a content hash (SHA256 of manifest) and an `is_latest` flag.

**Pull:** Returns the manifest and all files for a specific version (or latest). Increments download count on each pull.

### Database Tables

The registry uses Supabase tables:

| Table                                                         | Purpose                                        |
| ------------------------------------------------------------- | ---------------------------------------------- |
| `users`                                                       | User accounts (id, username)                   |
| `tools`                                                       | Tool registry entries                          |
| `directives`                                                  | Directive registry entries                     |
| `knowledge`                                                   | Knowledge registry entries                     |
| `tool_versions` / `directive_versions` / `knowledge_versions` | Version history                                |
| `bundles`                                                     | Bundle metadata                                |
| `bundle_versions`                                             | Bundle version history with manifest and files |

### Authentication

The registry API uses JWT-based authentication. The `get_current_user` dependency extracts and validates the user from the Authorization header. Some endpoints (search, public key) allow unauthenticated access.

### Configuration

Server configuration via environment variables (in `registry_api/config.py`):

| Variable               | Description               |
| ---------------------- | ------------------------- |
| `SUPABASE_URL`         | Supabase project URL      |
| `SUPABASE_SERVICE_KEY` | Supabase service role key |
| `HOST`                 | Server bind host          |
| `PORT`                 | Server bind port          |
| `LOG_LEVEL`            | Logging level             |

### Registry Public Key

The registry exposes its Ed25519 public key at `GET /v1/public-key` (PEM format). Clients use this for Trust On First Use (TOFU) pinning — on first pull, the client stores the registry's public key and verifies future downloads against it.

## Workflow Example

### Publishing a tool to the registry

```
1. Create tool locally:
   my-project/.ai/tools/utilities/web-scraper.py

2. Sign it:
   sign(item_type="tool", item_id="utilities/web-scraper")

3. Login to registry:
   execute(tool="rye/core/registry/registry", action="login")

4. Push to registry:
   execute(tool="rye/core/registry/registry", action="push",
           item_type="tool",
           item_id="myuser/utilities/web-scraper",
           version="1.0.0")

5. Make it public:
   execute(tool="rye/core/registry/registry", action="publish",
           item_type="tool",
           item_id="myuser/utilities/web-scraper")
```

### Using a registry item in another project

```
1. Search for tools:
   execute(tool="rye/core/registry/registry", action="search",
           query="web scraper", item_type="tool")

2. Pull to project:
   execute(tool="rye/core/registry/registry", action="pull",
           item_type="tool",
           item_id="myuser/utilities/web-scraper",
           space="project")

3. The tool is now at:
   other-project/.ai/tools/utilities/web-scraper.py
   (with registry signature intact)

4. Use it:
   execute(tool="utilities/web-scraper", ...)
```
