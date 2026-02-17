---
id: agent-integration
title: "Agent Integration with the Registry"
description: How AI agents discover, pull, and use registry items in practice — search, pull-and-use, trust verification, and bundle flows
category: registry
tags: [registry, agent, integration, pull, search, workflow, bundles]
version: "1.0.0"
---

# Agent Integration with the Registry

This document covers how AI agents interact with the Rye OS registry at runtime — discovering items, pulling them into a project, executing them, and working with bundles.

## Discovery Flow

Agents discover items through two mechanisms with different scopes:

### Local Search (via `rye_search`)

The `rye_search` MCP tool searches the local filesystem only — project `.ai/`, user `~/.ai/`, and system (bundled) spaces:

```
rye_search(
  item_type="tool",
  query="web scraper"
)
```

This returns items already present locally. It does **not** query the registry.

### Registry Search (via `rye_execute`)

To search the registry, the agent explicitly calls the registry tool:

```
rye_execute(
  item_type="tool",
  item_id="rye/core/registry/registry",
  parameters={
    "action": "search",
    "query": "web scraper",
    "item_type": "tool"
  }
)
```

The registry search hits `GET /v1/search?query=web+scraper&item_type=tool` on the server, performing case-insensitive matching on item names and descriptions. By default only public items are returned; authenticated users can include their own private items with `include_mine=true`.

### Why Search Is Explicit

Local search and registry search are intentionally separate. The agent must know about and explicitly invoke the `rye/core/registry/registry` tool to query the registry. This is by design — **explicit over implicit** — so the agent (and user) always knows when network calls are being made.

## Pull-and-Use Flow

Once an agent finds an item in the registry, the pull-and-use flow is:

### Step 1: Search the Registry

```
rye_execute(
  item_type="tool",
  item_id="rye/core/registry/registry",
  parameters={
    "action": "search",
    "query": "web scraper",
    "item_type": "tool"
  }
)
# Returns: [{ item_id: "leolilley/utilities/web-scraper", ... }]
```

### Step 2: Pull to Project

```
rye_execute(
  item_type="tool",
  item_id="rye/core/registry/registry",
  parameters={
    "action": "pull",
    "item_type": "tool",
    "item_id": "leolilley/utilities/web-scraper",
    "space": "project"
  }
)
```

The pull flow:

1. Fetches signed content from `GET /v1/pull/tool/leolilley/utilities/web-scraper`
2. Verifies the registry Ed25519 signature locally (hash match + signature validation)
3. On first pull, TOFU-pins the registry public key to `~/.ai/trusted_keys/registry.pem`
4. Writes the file to `.ai/tools/utilities/web-scraper.py` (category becomes the directory path, name becomes the filename)
5. The registry signature (`rye:signed:...|rye-registry@leolilley`) is preserved in the file

### Step 3: Execute Normally

```
rye_execute(
  item_type="tool",
  item_id="utilities/web-scraper",
  parameters={
    "url": "https://example.com",
    "selector": ".product-card"
  }
)
```

The pulled item is now a local tool. The `PrimitiveExecutor` resolves it from `.ai/tools/`, verifies its signature (the registry key is in the trust store), builds the execution chain, and runs it.

### File Destination

Items land at predictable paths based on their identity:

| Item Type   | Registry ID                          | Local Path                                          |
| ----------- | ------------------------------------ | --------------------------------------------------- |
| Tool        | `leolilley/utilities/web-scraper`    | `.ai/tools/utilities/web-scraper.py`                |
| Directive   | `leolilley/core/bootstrap`           | `.ai/directives/core/bootstrap.md`                  |
| Knowledge   | `leolilley/patterns/retry-backoff`   | `.ai/knowledge/patterns/retry-backoff.md`           |

The namespace (`leolilley`) is stripped from the local path — only category and name are used for the filesystem location.

## Directive That Uses Registry Items

Here's a concrete directive that searches, pulls, and executes a registry tool:

```xml
<!-- rye:signed:2026-02-18T00:00:00Z:HASH:SIG:FP -->
# Scrape Product Data

Fetches product data from a target URL using a registry-sourced scraper tool.

```xml
<metadata>
  <model tier="haiku" />
  <limits turns="10" spend="0.10" />
  <permissions>
    <read resource="filesystem" path=".ai/**" />
    <write resource="filesystem" path=".ai/tools/**" />
    <execute resource="tool" id="rye/core/registry/registry" />
    <execute resource="tool" id="utilities/web-scraper" />
    <network resource="http" host="*" />
  </permissions>
  <inputs>
    <input name="target_url" type="string" required="true" />
  </inputs>
  <outputs>
    <output name="products" type="array" />
  </outputs>
</metadata>
```

## Process

### Step 1: Ensure scraper tool is available

Search locally for the web-scraper tool:
```
rye_search(item_type="tool", query="web-scraper")
```

If not found locally, pull from registry:
```
rye_execute(
  item_type="tool",
  item_id="rye/core/registry/registry",
  parameters={
    "action": "pull",
    "item_type": "tool",
    "item_id": "leolilley/utilities/web-scraper",
    "space": "project"
  }
)
```

### Step 2: Run the scraper

```
rye_execute(
  item_type="tool",
  item_id="utilities/web-scraper",
  parameters={
    "url": "{target_url}",
    "selector": ".product-card"
  }
)
```

### Step 3: Return results

Return the scraped product data as the `products` output.
```

Key points in the directive:
- **Permissions declare both tools** — the registry tool (for pull) and the target tool (for execution)
- **Local-first pattern** — search locally before pulling from registry
- **Pull goes to project space** — so the tool is available for future executions without re-pulling

## Trust Verification on Pull

When a pulled item is later executed, the integrity system verifies it the same way as any local item:

1. **Signature check** — `verify_item()` finds the `rye:signed:...|rye-registry@leolilley` comment
2. **Hash check** — Recomputes SHA256 of content and compares to the embedded hash
3. **Ed25519 check** — Verifies the signature using the public key matching the fingerprint
4. **Trust store check** — Looks up the fingerprint in `~/.ai/trusted_keys/`. The registry's key was pinned during the first pull (TOFU), so it's found at `~/.ai/trusted_keys/registry.pem`

If the registry key has not been pinned (e.g., the agent has never pulled before), `verify_item()` raises `IntegrityError("Untrusted key ...")`. The fix is to pull any item from the registry, which triggers TOFU pinning.

### Manual Key Trust

For items signed by individual users (not the registry), their public key must be explicitly added to the trust store:

```
rye_sign(
  action="trust",
  public_key_pem="<PEM content>"
)
```

This writes the key to `~/.ai/trusted_keys/{fingerprint}.pem`.

## Bundle Pull Flow

Bundles are versioned collections of items with a signed manifest. Pulling a bundle retrieves everything at once.

### Step 1: Pull the Bundle

```
rye_execute(
  item_type="tool",
  item_id="rye/core/registry/registry",
  parameters={
    "action": "pull_bundle",
    "bundle_id": "rye-core",
    "project_path": "/path/to/project"
  }
)
```

### Step 2: What Happens Internally

1. **Fetch from registry** — `GET /v1/bundle/pull/rye-core` returns the manifest and all files as JSON
2. **Write manifest** — Saved to `.ai/bundles/rye-core/manifest.yaml`
3. **Write all files** — Each file is written to its relative path (e.g., `.ai/tools/rye/core/registry/registry.py`)
4. **Verify manifest** — `verify_item(manifest_path, ItemType.TOOL)` checks the manifest's Ed25519 signature
5. **Verify per-file hashes** — The bundler's `validate_bundle_manifest()` computes SHA256 of each file and compares to the manifest's recorded hash

### Step 3: Verify the Bundle

After pulling, the agent can explicitly verify bundle integrity:

```
rye_execute(
  item_type="tool",
  item_id="rye/core/bundler/bundler",
  parameters={
    "action": "verify",
    "bundle_id": "rye-core"
  }
)
```

This returns a report:

```json
{
  "status": "verified",
  "manifest_valid": true,
  "files_checked": 42,
  "files_ok": 42,
  "files_missing": [],
  "files_tampered": []
}
```

### Bundle Verification Details

Bundle verification is layered:

| Layer                | What's Checked                                                          |
| -------------------- | ----------------------------------------------------------------------- |
| **Manifest signature** | The manifest YAML has an inline `rye:signed:` Ed25519 signature       |
| **Per-file SHA256**  | Every file listed in the manifest has a `sha256` field compared to disk |
| **Inline signatures** | Files marked `inline_signed: true` also have their individual Ed25519 signatures verified via `verify_item()` |
| **Missing files**    | Any file in the manifest not found on disk is flagged                   |

This means even non-signable assets (images, data files) are covered by the manifest's per-file SHA256 hashes, while signable items (`.py`, `.md`, `.yaml`) have dual protection.

## Current Limitations

| Limitation                          | Description                                                                                     |
| ----------------------------------- | ----------------------------------------------------------------------------------------------- |
| **No automatic registry search**    | `rye_search` only searches the local filesystem. The agent must explicitly call the registry tool to discover remote items. |
| **Agent must know the registry tool** | The agent needs to know that `rye/core/registry/registry` exists and how to call it with the right action/parameters. |
| **Authentication required for pull** | Most registry operations require authentication. The agent must have logged in via `action: login` before pulling. |
| **No dependency resolution**        | Pulling an item does not automatically pull its dependencies. If a tool depends on other tools, they must be pulled separately. |
| **No auto-update**                  | Pulled items are static snapshots. There is no mechanism to check for or apply updates to previously pulled items. |
| **Namespace stripped on pull**       | The namespace is removed from the local path, so `leolilley/utils/tool` and `otheruser/utils/tool` would conflict at `.ai/tools/utils/tool.py`. |
