```yaml
id: remove-http-primitive
title: "Remove HttpClientPrimitive — One Execution Primitive"
description: Remove HttpClientPrimitive as a separate execution primitive. HTTP tools become regular tools that resolve through subprocess like everything else. Every chain terminates at lillux exec.
category: future
tags: [lillux, http, primitives, simplification]
version: "0.1.0"
status: proposed
```

# Remove HttpClientPrimitive — One Execution Primitive

> **Status:** Proposed

## The Problem

`HttpClientPrimitive` exists as a separate execution primitive alongside `SubprocessPrimitive`. This creates a false distinction: "two types of tools." In reality, an HTTP call is just what a tool does internally. It's not a kernel concern. A Python tool that calls an API resolves through the normal chain like any other Python tool. The HTTP library it uses is a dependency, not a primitive.

## The Fix

Remove `HttpClientPrimitive`. Tools that currently terminate at the HTTP primitive become regular tools with a Python (or other language) runtime that resolve through subprocess like everything else.

```
Before:  tool → http_client primitive (Python httpx, bypasses Lillux)
After:   tool → python runtime → Execute primitive → lillux exec run
```

Every tool chain terminates at Execute. One primitive. No special cases.

## What Changes

### Code

- **Remove** `rye/runtime/http_client.py` (`HttpClientPrimitive` class)
- **Remove** HTTP entry from `PRIMITIVE_MAP` in `rye/executor/primitive_executor.py`
- **Rename** `SubprocessPrimitive` → `ExecutePrimitive` in `rye/primitives/subprocess.py` (rename file to `execute.py`). "Subprocess" is a Unix implementation detail. With one execution primitive it should just be called what it is: Execute. Matches Lillux's own naming (`lillux exec`) and the action primitive name.
- **Update** `PRIMITIVE_MAP` to reference `ExecutePrimitive` as the single execution primitive
- **Convert** tools that currently use the HTTP primitive into regular Python tools with `httpx` as a dependency
- **Update** `rye/core/primitives/http` item (or remove it) since it's no longer a terminal primitive
- **Update** `rye/core/primitives/subprocess` item → rename to `rye/core/primitives/execute`

### Docs — User Facing

- **`docs/manifesto.md`** — already updated, no longer mentions two execution primitives
- **`docs/tools/`** — update any tool authoring guides that reference the HTTP primitive as a chain target
- **`docs/internals/execution-chain.md`** — remove HTTP as a primitive option, document that all chains terminate at subprocess

### Docs — Agent Facing

- **System knowledge** — update any knowledge items that describe the two-primitive model (search for "http_client", "HttpClientPrimitive", "http primitive" in `.ai/knowledge/`)
- **AGENTS.md / instructions** — update references to "two execution primitives" or "subprocess and HTTP"
- **Tool creation directives** — update `rye/core/create_tool` and related directives if they offer HTTP primitive as a chain target option

### Tests

- **`tests/`** — update or remove tests for `HttpClientPrimitive`
- **Integration tests** — verify converted HTTP tools work correctly through subprocess chain

## Migration

Tools currently using the HTTP primitive need to be converted. Each becomes a Python tool with a thin script:

```python
import httpx
import json
import sys

params = json.loads(sys.argv[1])
response = httpx.post(params["url"], json=params.get("body"), headers=params.get("headers", {}))
print(json.dumps({"status": response.status_code, "body": response.json()}))
```

The tool YAML points to a Python runtime instead of the HTTP primitive. Everything else (signing, validation, chain resolution) stays the same.

## Why

**One primitive.** Every tool chain terminates at Execute. The mental model is: tools resolve through chains to Lillux. Period.

**No bypass.** HTTP was the one path that didn't go through the Rust trust boundary. Removing it means every execution goes through Lillux. The manifesto's "no fallback, no bypass" becomes structurally true.

**Simplicity.** One fewer primitive to maintain, document, and explain. HTTP isn't special. It's just what some tools do.
