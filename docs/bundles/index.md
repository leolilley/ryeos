```yaml
id: bundles-index
title: "Optional Bundles"
description: Optional extension bundles that add domain-specific tools to Rye OS
category: bundles
tags: [bundles, ryeos-web, ryeos-code, ryeos-email, optional]
version: "1.0.0"
```

# Optional Bundles

Rye OS has a small core — the standard library ships file-system tools, bash, MCP, the orchestration engine, and infrastructure. Everything else is an **optional bundle** that you install separately.

Bundles are pip extras or standalone packages that register additional tools, directives, and knowledge entries into the system space via entry points.

## Available Bundles

| Bundle | Package | Install | Description |
| --- | --- | --- | --- |
| [Web](web/index.md) | `ryeos-web` | `pip install ryeos[web]` | Web search, page fetching, browser automation |
| [Code](code/index.md) | `ryeos-code` | `pip install ryeos[code]` | NPM, diagnostics, TypeScript, LSP code intelligence |
| [Email](email/index.md) | `ryeos-email` | `pip install ryeos[email]` | Provider-agnostic email — inbound routing, send, forward, drafting |

## How Bundles Work

Each bundle is a Python package containing a `.ai/` directory with tools, directives, and knowledge entries. Bundles register themselves via `ryeos.bundles` entry points in their `pyproject.toml`:

```toml
[project.entry-points."ryeos.bundles"]
email = "ryeos_email"
```

At startup, Rye OS discovers all registered bundles and adds their `.ai/` directories to the system space resolution chain. Bundle items are overridable — place a file with the same `item_id` in your project or user space to customize behavior.

## Bundle vs Standard Library

| Aspect | Standard Library | Optional Bundle |
| --- | --- | --- |
| Package | `ryeos` | `ryeos-web`, `ryeos-code`, `ryeos-email` |
| Install | Always included | Installed separately |
| Items | Infrastructure, core tools | Domain-specific tools |
| Location | `ryeos/rye/.ai/` | `ryeos_<bundle>/.ai/` |
| Override | Same mechanism | Same mechanism |
