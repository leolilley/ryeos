---
category: "ryeos/concepts"
name: "spaces"
description: "Three-tier space resolution: project, user, system"
---

# Spaces

Items resolve through three spaces. First match wins.

```
project  →  user  →  system
```

## The three spaces

### Project space

**Path:** `.ai/` in the project root (the working directory or `--project` flag)

**Purpose:** Project-specific items. Directives, tools, and knowledge that apply to one project.

**Who writes:** The operator or AI agent working on the project.

**Examples:**
- `.ai/directives/deploy.md` — project deployment workflow
- `.ai/tools/project/lint.yaml` — project-specific lint tool
- `.ai/knowledge/project/conventions.md` — project coding conventions

### User space

**Path:** `~/.ai/` (or `$HOME/.ai/`)

**Purpose:** Cross-project personal items. Signing keys, trust store, personal directives.

**Who writes:** The operator (via `ryeos init`, `ryeos trust pin`, or manual creation).

**Key locations:**
- `~/.ai/config/keys/signing/private_key.pem` — Operator's Ed25519 signing key
- `~/.ai/config/keys/trusted/*.toml` — Trusted signer pins
- `~/.ai/directives/` — Personal directives available in all projects
- `~/.ai/knowledge/` — Personal knowledge base

### System space

**Path:** `$XDG_DATA_DIR/ryeos/` (default: `~/.local/share/ryeos/`)

**Purpose:** The immutable standard library. Kind schemas, parsers, handlers, protocols, and core tools.

**Who writes:** `ryeos init` installs bundles here. Items are signed by the publisher.

**Key locations:**
- `.ai/node/engine/kinds/` — Kind schemas
- `.ai/parsers/` — Parsers
- `.ai/handlers/` — Handlers
- `.ai/protocols/` — Protocols
- `.ai/services/` — Services
- `.ai/tools/` — Core tools
- `.ai/bin/<triple>/` — Compiled binaries

Override with `RYEOS_SYSTEM_SPACE_DIR` or `--system-data-dir`.

## Resolution example

Resolving `tool:ryeos/core/identity/public_key`:

1. Check `<project>/.ai/tools/ryeos/core/identity/public_key.{yaml,yml,md}` — not found
2. Check `~/.ai/tools/ryeos/core/identity/public_key.{yaml,yml,md}` — not found
3. Check `$XDG_DATA_DIR/ryeos/.ai/tools/ryeos/core/identity/public_key.{yaml,yml,md}` — found!

This means you can override any system item by placing a file with the same path in project or user space. Project space always wins.

## Bundle roots

The daemon also scans registered bundle roots for items. Bundle roots come from:

1. `<system_data_dir>/` — always included (the core install location)
2. Bundle registrations in `<state_dir>/.ai/node/bundles/` — additional bundles
3. Bundle registrations in `<system_data_dir>/.ai/node/bundles/` — system-level bundles

Bundle roots are an extension layer on top of the three spaces. They provide additional item directories that the engine scans.

## What goes where

| Item type | Project | User | System |
|---|---|---|---|
| Directives | Project workflows | Personal workflows | None |
| Tools | Project tools | Personal tools | Core tools |
| Knowledge | Project context | Personal notes | None |
| Kind schemas | Project-specific kinds | None | Standard kinds |
| Parsers | Project-specific parsers | None | Standard parsers |
| Handlers | None | None | Standard handlers |
| Protocols | None | None | Standard protocols |
| Config | Project config | Personal config | Default config |
| Signing keys | None | `~/.ai/config/keys/` | None |
| Trust store | None | `~/.ai/config/keys/trusted/` | None |
