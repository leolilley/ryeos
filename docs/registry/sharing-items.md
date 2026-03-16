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

Items and bundles have two visibility levels: `public` (visible to all users) and `unlisted` (accessible by direct ID but not returned in search results). New bundles default to `public` on push. Visibility is controlled via the `publish` and `unpublish` actions for individual items.

## The Registry Tool

**Location:** `.ai/tools/rye/core/registry/registry.py`

The registry tool is a Python script that runs through the executor chain like any other tool:

```python
__executor_id__ = "rye/core/runtimes/python/script"
```

### Authentication Actions

| Action            | Description                                                                        |
| ----------------- | ---------------------------------------------------------------------------------- |
| `signup`          | Create account with email/password                                                 |
| `login`           | Start device auth flow (opens browser, polls for completion, supports OAuth via GitHub) |
| `login_email`     | Login via email/password                                                           |
| `logout`          | Clear local auth session                                                           |
| `whoami`          | Show current authenticated user                                                    |
| `create_api_key`  | Create a persistent API key (`rye_sk_...`) from a bootstrap JWT                    |
| `list_api_keys`   | List all API keys for the authenticated user                                       |
| `revoke_api_key`  | Revoke an existing API key                                                         |

Authentication uses an API key model. The initial setup uses OAuth PKCE flow with GitHub to obtain a temporary bootstrap JWT, which is then exchanged for a persistent API key via `create_api_key`. The JWT is used only once — all subsequent requests use the API key. For CI/serverless, set the `RYE_REGISTRY_API_KEY` environment variable directly.

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

Search matches against item names and descriptions. By default only public items are returned. Authenticated users can include their own unlisted items with `include_mine=true`.

#### Delete

Remove an item from the registry:

```
action: delete
item_type: tool
item_id: leolilley/utilities/my-tool
```

Only the namespace owner can delete items.

#### Push Bundle

Upload a bundle (manifest + files) to the registry:

```
action: push_bundle
bundle_id: leolilley/my-bundle
version: 1.0.0
```

#### Pull Bundle

Download a bundle from the registry:

```
action: pull_bundle
bundle_id: leolilley/my-bundle
```

Optionally specify a version with `version`. Defaults to the latest version.

#### Search Bundles

Find bundles in the registry:

```
action: search_bundle
query: "core utilities"
namespace: leolilley   # optional filter
limit: 20              # optional, default 20
```

Search matches against bundle names and descriptions. Only public bundles are returned by default.

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
| GET    | `/v1/bundle/search`                          | Search bundles by query                            | Optional |

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

Bundles are versioned collections of items (manifest + files) that can be pushed and pulled as a unit. Like individual items, bundles support visibility control and search.

**Push:** Stores the manifest and all bundle files as JSONB in Supabase. Each version tracks a content hash (object_hash (CAS-based) of manifest) and an `is_latest` flag.

**Pull:** Returns the manifest and all files for a specific version (or latest). Increments download count on each pull.

**Search:** Queries bundles by name and description with optional namespace filtering. Only public bundles are returned by default.

Bundle visibility is `public` or `unlisted`. New bundles default to `public` on push.

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

The registry API authenticates via API keys (format: `rye_sk_...`). The `get_current_user` dependency extracts and validates the API key from the Authorization header. JWTs are used only during the initial bootstrap flow to create the first API key. Some endpoints (search, public key) allow unauthenticated access.

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

3. Login and create API key:
   execute(tool="rye/core/registry/registry", action="login")
   execute(tool="rye/core/registry/registry", action="create_api_key")

4. Push to registry (authenticates with API key):
   execute(tool="rye/core/registry/registry", action="push",
           item_type="tool",
           item_id="myuser/utilities/web-scraper",
           version="1.0.0")

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

### Publishing a bundle (CLI)

The full build → push workflow using the CLI (bundles default to public on push):

```bash
# Build a bundle manifest from a package directory
rye registry bundle build ryeos/bundles/core --bundle-id ryeos-core

# Push the bundle to the registry
rye registry bundle push ryeos-core
```

### Searching and pulling bundles (CLI)

```bash
# Search for public bundles
rye registry bundle search "core utilities"

# Narrow search by namespace
rye registry bundle search "core" --namespace leolilley --limit 10

# Pull a bundle into your project
rye registry bundle pull ryeos-core

# Pull a specific version
rye registry bundle pull ryeos-core --version 1.2.0
```

### Installing and uninstalling bundles (CLI)

```bash
# Install a bundle from the registry (pull + verify + materialize)
rye install my-bundle
rye install my-bundle@1.0.0
rye install namespace/my-bundle

# Install to project space (default is user space)
rye install my-bundle --space project

# Uninstall a bundle (removes installed files + metadata)
rye uninstall my-bundle
rye uninstall my-bundle --space project
```

Installation writes a lockfile at `.ai/bundles/{bundle_id}/.bundle-lock.json` tracking installed files. Uninstallation reads this lockfile to remove exactly what was installed.
