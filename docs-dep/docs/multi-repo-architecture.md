# Multi-Repo Architecture Proposal

## Overview

This document proposes splitting the monolithic RYE codebase into separate repositories to achieve cleaner separation of concerns, reduce dependency bloat, and enable independent development lifecycles.

## Current Problem

The current `rye/` package contains:
- MCP server protocol handling
- Core execution engine (PrimitiveExecutor)
- File parsers and validators
- Trust store and integrity checking
- All tool implementations

This creates issues:
1. **Services must install entire MCP server** just to use `MetadataManager` or `ItemType` constants
2. **CLI forced to use MCP protocol** when direct library calls would be more efficient
3. **No clear boundaries** between protocol adapters and business logic
4. **Heavy dependency chain** for any consumer (mcp, pyyaml, cryptography, etc.)

## Proposed Architecture

### 4-Repository Structure

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              rye-core                                       │
│                   (Pure execution library)                                  │
├─────────────────────────────────────────────────────────────────────────────┤
│  Core execution engine without protocol dependencies                         │
│  Dependencies: lilux, pyyaml, cryptography                                   │
└─────────────────────────────────────────────────────────────────────────────┘
       ▲                    ▲                    ▲                    ▲
       │                    │                    │                    │
  imports              imports              imports              imports
       │                    │                    │                    │
  ┌────┴──────┐      ┌──────┴──────┐      ┌──────┴──────┐      ┌──────┴──────┐
  │ rye-mcp   │      │  rye-cli    │      │   services  │      │  other apps │
  │ MCP server│      │  CLI tool   │      │ (HTTP APIs) │      │             │
  └───────────┘      └─────────────┘      └─────────────┘      └─────────────┘
```

### Repository Details

#### 1. rye-core
**Purpose**: Core execution library, protocol-agnostic

**Contents**:
```
rye_core/
├── executor/
│   ├── primitive_executor.py      # Chain resolution and execution
│   ├── chain_validator.py         # Chain validation logic
│   └── lockfile_resolver.py       # Lockfile handling
├── parsers/
│   ├── markdown_xml.py            # Directive parsing
│   ├── markdown_frontmatter.py    # Knowledge parsing
│   ├── yaml.py                    # YAML tool configs
│   └── python_ast.py              # Python tool metadata extraction
├── validators/
│   ├── integrity.py               # Ed25519 signature verification
│   └── signature_formats.py       # Signature format handling
├── trust/
│   └── trust_store.py             # Key management and TOFU
├── utils/
│   ├── metadata_manager.py        # Metadata extraction
│   ├── path_utils.py              # Space resolution
│   ├── resolvers.py               # Project/user/system resolution
│   └── parser_router.py           # Parser dispatch
├── tools/
│   ├── search.py                  # Search implementation
│   ├── load.py                    # Load implementation
│   ├── execute.py                 # Execute implementation
│   └── sign.py                    # Sign implementation
└── .ai/                           # All tool implementations
    └── tools/
```

**Dependencies**:
```toml
[project]
name = "rye-core"
dependencies = [
    "lilux",
    "pyyaml",
    "cryptography",
    "packaging>=21.0",
]
```

**Consumers**: rye-mcp, rye-cli, registry-api, any other service

#### 2. rye-mcp
**Purpose**: MCP protocol adapter

**Contents**:
```
rye_mcp/
├── server.py                      # MCP server entry point
└── adapters/                      # Thin wrappers around rye-core
    ├── search_adapter.py
    ├── load_adapter.py
    ├── execute_adapter.py
    └── sign_adapter.py
```

**Dependencies**:
```toml
[project]
name = "rye-mcp"
dependencies = [
    "rye-core",
    "mcp",
]

[project.scripts]
rye-mcp = "rye_mcp.server:main"
```

**Key Insight**: This is just a protocol adapter. All business logic delegates to rye-core.

#### 3. rye-cli
**Purpose**: Human-friendly CLI interface

**Contents**:
```
rye_cli/
├── __init__.py
├── main.py                        # CLI entry point
├── commands/
│   ├── search.py                  # rye search
│   ├── run.py                     # rye run
│   ├── load.py                    # rye load
│   ├── sign.py                    # rye sign
│   └── threads.py                 # rye threads list/watch/resume
└── formatters/
    ├── table.py                   # Tabular output
    ├── json.py                    # JSON output
    └── progress.py                # Progress bars
```

**Dependencies**:
```toml
[project]
name = "rye-cli"
dependencies = [
    "rye-core",
    "click>=8.0",  # or typer
    "rich",        # for colors/tables/progress
]

[project.scripts]
rye = "rye_cli.main:cli"
```

**Key Difference from MCP**: Direct library calls, no protocol overhead, streaming output, human-friendly formatting.

#### 4. services (registry-api, etc.)
**Purpose**: HTTP API services

**Current Issue** (from `services/registry-api/pyproject.toml`):
```toml
dependencies = [
    # ...
    # rye-os is installed separately in Docker or via: pip install ../../rye
]
```

**After Separation**:
```toml
dependencies = [
    "fastapi>=0.109.0",
    "rye-core",  # Lightweight, no MCP
]
```

## Migration Strategy

### Phase 1: Extract rye-core (Week 1-2)

1. Create new `rye-core` repository
2. Move modules from `rye/rye/`:
   - `executor/` → `rye_core/executor/`
   - `utils/` → `rye_core/utils/`
   - `handlers/` → `rye_core/handlers/`
   - `constants.py` → `rye_core/constants.py`
3. Move `.ai/tools/` → `rye_core/.ai/tools/`
4. Update imports: `from rye.X` → `from rye_core.X`
5. Publish to internal package index

### Phase 2: Refactor rye → rye-mcp (Week 2)

1. Rename `rye/` directory to `rye-mcp/`
2. Remove all core modules (now in rye-core)
3. Create thin MCP adapter layer
4. Update imports: `from rye.X` → `from rye_core.X`
5. Update entry point: `rye = rye_mcp.server:main`

### Phase 3: Create rye-cli (Week 3)

1. New repository `rye-cli/`
2. Import rye-core directly
3. Implement commands:
   - `rye search --type directive "query"`
   - `rye run directive/name --input key=value`
   - `rye load --type tool tool/id`
   - `rye sign --type directive directive/name`
   - `rye threads list`
   - `rye threads watch <thread_id>`
4. Add rich output formatting

### Phase 4: Update Services (Week 3-4)

1. Update `services/registry-api/`:
   ```python
   # Before
   from rye.utils.metadata_manager import MetadataManager
   
   # After
   from rye_core.utils.metadata_manager import MetadataManager
   ```
2. Remove manual installation of full rye package
3. Depend only on `rye-core` in pyproject.toml

### Phase 5: Clean Up (Week 4)

1. Update documentation
2. Update Dockerfiles
3. Update CI/CD pipelines
4. Archive old monolithic repo

## Benefits

### For Services (like registry-api)

**Before**:
```toml
dependencies = [
    "fastapi",
    "rye-os",  # Pulls in: mcp, lilux, pyyaml, cryptography, packaging
]
```

**After**:
```toml
dependencies = [
    "fastapi",
    "rye-core",  # Pulls in: lilux, pyyaml, cryptography, packaging
    # No MCP! 50MB+ smaller
]
```

### For CLI

**Before**: Must spawn MCP server process, communicate via JSON-RPC
```bash
# MCP-based CLI
$ rye search --type directive foo
# Spawns: rye-mcp server (200MB+)
# Protocol overhead per call
```

**After**: Direct library calls
```bash
# Native CLI
$ rye search --type directive foo
# Direct Python call, instant response
# Streaming output, progress bars
```

### For MCP Server

**Before**: Mixed concerns - protocol + business logic
**After**: Single responsibility - just protocol translation

```python
# rye-mcp server.py - After separation
from rye_core.tools.search import SearchTool
from mcp.server import Server

class RYEMCPServer:
    def __init__(self):
        self.search = SearchTool()  # From rye-core
    
    async def handle_search(self, arguments):
        # Just protocol handling, no business logic
        result = await self.search.handle(**arguments)
        return format_as_mcp_content(result)
```

## Dependency Graph

```
rye-core
├── lilux
├── pyyaml
├── cryptography
└── packaging

rye-mcp
├── rye-core
└── mcp

rye-cli
├── rye-core
├── click
└── rich

services/registry-api
├── rye-core
├── fastapi
└── uvicorn
```

## Versioning Strategy

- **rye-core**: Semantic versioning (1.0.0, 1.1.0, etc.)
  - Breaking changes require major version bump
  - All consumers must update together

- **rye-mcp**: Independent versioning
  - Can add MCP-specific features without core changes
  - Follows rye-core major version (e.g., rye-mcp 2.x requires rye-core 1.x)

- **rye-cli**: Independent versioning
  - UX improvements don't affect core
  - Follows rye-core major version

## Open Questions

1. **Tool implementations**: Should `.ai/tools/` stay in rye-core or be a separate package?
   - *Recommendation*: Keep in rye-core initially, extract later if needed

2. **Test organization**: How to handle integration tests that cross repos?
   - *Recommendation*: Unit tests in each repo, integration tests in rye-core

3. **Documentation**: Where does user-facing docs live?
   - *Recommendation*: rye-core repo for API docs, separate docs site for user guides

4. **Release coordination**: How to coordinate releases across repos?
   - *Recommendation*: rye-core drives the release, others follow

## Implementation Priority

1. **High**: Extract rye-core (unblocks services immediately)
2. **High**: Refactor rye → rye-mcp (maintains existing functionality)
3. **Medium**: Create rye-cli (new functionality)
4. **Medium**: Update services (benefits from separation)

## Conclusion

This separation:
- **Eliminates dependency bloat** for services
- **Enables efficient CLI** without protocol overhead
- **Creates clear boundaries** between protocol adapters and business logic
- **Allows independent evolution** of MCP, CLI, and core engine
- **Follows Unix philosophy**: do one thing well

The migration can be done incrementally without breaking existing functionality.
