# RYE System Bundle Migration Plan

## Target Structure

```
.ai/tools/rye/                      # System space - immutable, pip-installed
├── core/                           # Core tooling foundations
│   ├── primitives/                 # Execution primitives (schemas)
│   ├── runtimes/                   # Language runtimes
│   ├── parsers/                    # Content parsing
│   ├── extractors/                 # Metadata extraction
│   │   ├── tool/
│   │   ├── directive/
│   │   └── knowledge/
│   ├── protocol/                   # Communication protocols
│   ├── sinks/                      # Event output
│   └── system/                     # System info (paths, time, runtime)
│
├── agent/                          # LLM agent execution
│   ├── capabilities/               # Sandboxing (enforced via safety harness)
│   │   ├── scopes/                 # Capability definitions (fs, db, net, git, mcp, process)
│   │   └── tokens/                 # Ed25519 signed capability tokens
│   ├── telemetry/                  # Agent monitoring via safety harness
│   ├── llm/                        # LLM config SCHEMA (user provides actual config)
│   ├── harness/                    # Agent harness (LLM loop + tool calls)
│   └── threads/                    # Thread management
│
├── registry/                       # Primary items distribution
├── rag/                            # Semantic search (user extends)
├── telemetry/                      # System monitoring, MCP logging access
└── mcp/                            # MCP protocol tools for rye-os
```

---

## Current State (rye-os)

### ✅ COMPLETE - core/
| Directory | Status | Notes |
|-----------|--------|-------|
| primitives/ | ✅ | Schemas only (http_client, subprocess, errors, __init__) |
| runtimes/ | ✅ | auth, env_resolver, node_runtime, python_runtime |
| parsers/ | ✅ | markdown_frontmatter, markdown_xml, python_ast, yaml |
| extractors/ | ✅ | Reorganized into tool/, directive/, knowledge/ subdirs |
| protocol/ | ✅ | jsonrpc_handler.py |
| sinks/ | ✅ | file_sink, null_sink, websocket_sink |
| system/ | ✅ | system.py with paths, time, runtime (USER_SPACE required) |

### ⚠️ PARTIALLY DONE - agent/
Files exist but at WRONG level. Currently:
```
agent/
├── __init__.py
├── db.py, fs.py, git.py, mcp.py, net.py, process.py  # These should be in capabilities/scopes/
```
Need to reorganize into proper structure (capabilities/, harness/, threads/, telemetry/, llm/)

### ❌ MISSING - Top-level tools
| Directory | Status |
|-----------|--------|
| registry/ | ❌ MISSING |
| rag/ | ❌ MISSING |
| telemetry/ | ❌ MISSING |
| mcp/ | ❌ MISSING |

---

## Source Files (Backups)

### Primary Source: `/home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/`

### Secondary Source: `/home/leo/projects/kiwi-mcp/.ai/tools/`

---

## Migration Tasks

> **IMPORTANT FOR IMPLEMENTER:** 
> - Always update signature comments from `kiwi-mcp:validated` to `rye:validated`
> - Update any imports from `kiwi_mcp.*` to use relative imports or `rye.*`
> - Do NOT delete existing working code - adapt it
> - Test with `python -m py_compile <file>` after changes

---

### 1. core/protocol/ ✅ DONE
### 2. core/sinks/ ✅ DONE

---

### 3. agent/capabilities/ - REORGANIZE

**Current State (WRONG):**
```
rye/rye/.ai/tools/rye/agent/
├── __init__.py
├── db.py
├── fs.py
├── git.py
├── mcp.py
├── net.py
└── process.py
```

**Target State (CORRECT):**
```
rye/rye/.ai/tools/rye/agent/capabilities/
├── __init__.py           # NEW - exports from scopes and tokens
├── scopes/
│   ├── __init__.py       # MOVE from agent/__init__.py (adapt)
│   ├── db.py             # MOVE from agent/db.py
│   ├── fs.py             # MOVE from agent/fs.py
│   ├── git.py            # MOVE from agent/git.py
│   ├── mcp.py            # MOVE from agent/mcp.py
│   ├── net.py            # MOVE from agent/net.py
│   └── process.py        # MOVE from agent/process.py
└── tokens/
    ├── __init__.py       # COPY from backup
    └── capability_tokens.py  # COPY from backup
```

**Step-by-Step Instructions:**

**Step 3.1:** Create directory structure
```bash
mkdir -p rye/rye/.ai/tools/rye/agent/capabilities/scopes
mkdir -p rye/rye/.ai/tools/rye/agent/capabilities/tokens
```

**Step 3.2:** Move existing scope files
```bash
mv rye/rye/.ai/tools/rye/agent/db.py rye/rye/.ai/tools/rye/agent/capabilities/scopes/
mv rye/rye/.ai/tools/rye/agent/fs.py rye/rye/.ai/tools/rye/agent/capabilities/scopes/
mv rye/rye/.ai/tools/rye/agent/git.py rye/rye/.ai/tools/rye/agent/capabilities/scopes/
mv rye/rye/.ai/tools/rye/agent/mcp.py rye/rye/.ai/tools/rye/agent/capabilities/scopes/
mv rye/rye/.ai/tools/rye/agent/net.py rye/rye/.ai/tools/rye/agent/capabilities/scopes/
mv rye/rye/.ai/tools/rye/agent/process.py rye/rye/.ai/tools/rye/agent/capabilities/scopes/
mv rye/rye/.ai/tools/rye/agent/__init__.py rye/rye/.ai/tools/rye/agent/capabilities/scopes/
```

**Step 3.3:** Copy tokens from backup
```bash
# Source: /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/capability_tokens/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/capability_tokens/__init__.py \
   rye/rye/.ai/tools/rye/agent/capabilities/tokens/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/capability_tokens/capability_tokens.py \
   rye/rye/.ai/tools/rye/agent/capabilities/tokens/
```

**Step 3.4:** Create new __init__.py files

Create `rye/rye/.ai/tools/rye/agent/__init__.py`:
```python
"""RYE Agent - LLM agent execution within RYE."""
```

Create `rye/rye/.ai/tools/rye/agent/capabilities/__init__.py`:
```python
"""Agent capabilities - sandboxing enforced via SafetyHarness."""

from .scopes import db, fs, git, mcp, net, process
from .tokens import capability_tokens

__all__ = ["db", "fs", "git", "mcp", "net", "process", "capability_tokens"]
```

**Step 3.5:** Update imports in tokens/capability_tokens.py
- Change any `kiwi_mcp` imports to relative imports
- Update signature from `kiwi-mcp:validated` to `rye:validated`

**Step 3.6:** Verify
```bash
python -m py_compile rye/rye/.ai/tools/rye/agent/capabilities/scopes/*.py
python -m py_compile rye/rye/.ai/tools/rye/agent/capabilities/tokens/*.py
```

---

### 4. agent/telemetry/ - COPY AND ADAPT

**Purpose:** Tools for monitoring SPAWNED AGENTS (not the MCP server itself)

**Source Files:**
```
/home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/telemetry/
├── lib.py          # Core TelemetryStore class
├── status.py       # Show telemetry status
├── configure.py    # Enable/disable telemetry
├── clear.py        # Clear telemetry data
├── export.py       # Export telemetry
└── run_with.py     # Run with telemetry wrapper
```

**Target:**
```
rye/rye/.ai/tools/rye/agent/telemetry/
├── __init__.py     # NEW
├── lib.py
├── status.py
├── configure.py
├── clear.py
├── export.py
└── run_with.py
```

**Step-by-Step Instructions:**

**Step 4.1:** Create directory and copy files
```bash
mkdir -p rye/rye/.ai/tools/rye/agent/telemetry

cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/telemetry/lib.py \
   rye/rye/.ai/tools/rye/agent/telemetry/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/telemetry/status.py \
   rye/rye/.ai/tools/rye/agent/telemetry/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/telemetry/configure.py \
   rye/rye/.ai/tools/rye/agent/telemetry/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/telemetry/clear.py \
   rye/rye/.ai/tools/rye/agent/telemetry/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/telemetry/export.py \
   rye/rye/.ai/tools/rye/agent/telemetry/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/telemetry/run_with.py \
   rye/rye/.ai/tools/rye/agent/telemetry/
```

**Step 4.2:** Create __init__.py
```python
"""Agent telemetry - monitoring spawned agent execution."""

from .lib import TelemetryStore

__all__ = ["TelemetryStore"]
```

**Step 4.3:** Update imports in all files
In each .py file:
- Change `from telemetry_lib import` to `from .lib import`
- Change any `kiwi_mcp` to relative imports
- Update signature from `kiwi-mcp:validated` to `rye:validated`

**Step 4.4:** Fix lib.py path import
The file has: `from lilux.utils.path_service import ...`
This should work if lilux is installed. If not, may need to update.

**Step 4.5:** Verify
```bash
python -m py_compile rye/rye/.ai/tools/rye/agent/telemetry/*.py
```

---

### 5. agent/llm/ - CREATE NEW

**Purpose:** LLM config SCHEMA (users provide actual provider configs in userspace)

**Target:**
```
rye/rye/.ai/tools/rye/agent/llm/
├── __init__.py
├── config_schema.py    # Schema defining valid LLM config structure
└── pricing.yaml        # Reference data for model pricing
```

**Step-by-Step Instructions:**

**Step 5.1:** Create directory
```bash
mkdir -p rye/rye/.ai/tools/rye/agent/llm
```

**Step 5.2:** Copy pricing.yaml from backup
```bash
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/llm/pricing.yaml \
   rye/rye/.ai/tools/rye/agent/llm/
```

**Step 5.3:** Create __init__.py
```python
"""LLM configuration - schema for user-provided LLM configs."""
```

**Step 5.4:** Create config_schema.py
```python
"""
LLM Config Schema

Defines the structure of a valid LLM configuration.
Users provide actual configs with API keys in their userspace.
"""

__version__ = "1.0.0"
__tool_type__ = "schema"
__category__ = "llm"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "provider": {
            "type": "string",
            "description": "LLM provider (anthropic, openai, google, etc.)",
        },
        "model": {
            "type": "string",
            "description": "Model identifier",
        },
        "api_key": {
            "type": "string",
            "description": "API key (use ${ENV_VAR} for env reference)",
        },
        "api_url": {
            "type": "string",
            "description": "API endpoint URL (optional, uses provider default)",
        },
        "max_tokens": {
            "type": "integer",
            "default": 4096,
        },
        "temperature": {
            "type": "number",
            "default": 0.7,
        },
        "timeout": {
            "type": "integer",
            "default": 120,
            "description": "Request timeout in seconds",
        },
    },
    "required": ["provider", "model", "api_key"],
}
```

**DO NOT COPY:**
- anthropic_messages.yaml (userspace)
- openai_chat.yaml (userspace)

---

### 6. agent/harness/ - COPY AND ADAPT

**Purpose:** Core agent loop - creates LLM threads, handles tool calls, permissions, hooks, logging

**Source Files:**
```
/home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/
├── safety_harness.py     → harness/safety_harness.py
└── thread_directive.py   → harness/directive_runner.py (RENAME)
```

**Target:**
```
rye/rye/.ai/tools/rye/agent/harness/
├── __init__.py
├── safety_harness.py      # Core LLM loop, tool processing, permissions
└── directive_runner.py    # Runs directives with harness
```

**Step-by-Step Instructions:**

**Step 6.1:** Create directory
```bash
mkdir -p rye/rye/.ai/tools/rye/agent/harness
```

**Step 6.2:** Copy and rename files
```bash
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/safety_harness.py \
   rye/rye/.ai/tools/rye/agent/harness/

cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/thread_directive.py \
   rye/rye/.ai/tools/rye/agent/harness/directive_runner.py
```

**Step 6.3:** Create __init__.py
```python
"""Agent harness - LLM loop and tool call processing."""

from .safety_harness import SafetyHarness

__all__ = ["SafetyHarness"]
```

**Step 6.4:** Update imports in safety_harness.py

Find and replace these patterns:
```python
# OLD:
from kiwi_mcp.safety_harness.capabilities import ...
from .capabilities import ...

# NEW:
from ..capabilities.scopes import db, fs, git, mcp, net, process
```

Also update:
- `kiwi-mcp:validated` → `rye:validated` in signature
- Any other `kiwi_mcp` references

**Step 6.5:** Update imports in directive_runner.py

Find and replace:
```python
# OLD:
from kiwi_mcp.handlers.directive.handler import ...

# NEW:
from rye.handlers.directive.handler import ...
```

**Step 6.6:** Verify
```bash
python -m py_compile rye/rye/.ai/tools/rye/agent/harness/*.py
```

---

### 7. agent/threads/ - COPY AND CLEAN

**Purpose:** Thread management utilities (spawn, pause, resume, registry)

**Source Files to COPY:**
```
/home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/
├── spawn_thread.py
├── thread_registry.py
├── pause_thread.py
├── resume_thread.py
├── inject_message.py
├── read_transcript.py
└── expression_evaluator.py
```

**DO NOT COPY (already moved or removed):**
- capabilities/ → already in agent/capabilities/scopes/
- capability_tokens/ → already in agent/capabilities/tokens/
- safety_harness.py → already in agent/harness/
- thread_directive.py → already in agent/harness/directive_runner.py
- anthropic_thread.yaml → REMOVED (userspace)
- openai_thread.yaml → REMOVED (userspace)

**Target:**
```
rye/rye/.ai/tools/rye/agent/threads/
├── __init__.py
├── spawn_thread.py
├── thread_registry.py
├── pause_thread.py
├── resume_thread.py
├── inject_message.py
├── read_transcript.py
└── expression_evaluator.py
```

**Step-by-Step Instructions:**

**Step 7.1:** Create directory
```bash
mkdir -p rye/rye/.ai/tools/rye/agent/threads
```

**Step 7.2:** Copy thread files
```bash
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/spawn_thread.py \
   rye/rye/.ai/tools/rye/agent/threads/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/thread_registry.py \
   rye/rye/.ai/tools/rye/agent/threads/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/pause_thread.py \
   rye/rye/.ai/tools/rye/agent/threads/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/resume_thread.py \
   rye/rye/.ai/tools/rye/agent/threads/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/inject_message.py \
   rye/rye/.ai/tools/rye/agent/threads/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/read_transcript.py \
   rye/rye/.ai/tools/rye/agent/threads/
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/threads/expression_evaluator.py \
   rye/rye/.ai/tools/rye/agent/threads/
```

**Step 7.3:** Create __init__.py
```python
"""Thread management - spawn, pause, resume, registry."""

from .thread_registry import ThreadRegistry
from .spawn_thread import spawn_thread

__all__ = ["ThreadRegistry", "spawn_thread"]
```

**Step 7.4:** Update imports in all files
- `kiwi-mcp:validated` → `rye:validated`
- Update any capability imports to `from ..capabilities.scopes import ...`
- Update any harness imports to `from ..harness import ...`

**Step 7.5:** Verify
```bash
python -m py_compile rye/rye/.ai/tools/rye/agent/threads/*.py
```

---

### 8. registry/ - COPY AND ADAPT

**Purpose:** Primary items distribution - push/pull to Supabase registry

**Source:**
```
/home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/registry/registry.py
```

**Target:**
```
rye/rye/.ai/tools/rye/registry/
├── __init__.py
└── registry.py    # ~1300 lines, full implementation
```

**Step-by-Step Instructions:**

**Step 8.1:** Create directory and copy
```bash
mkdir -p rye/rye/.ai/tools/rye/registry

cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/registry/registry.py \
   rye/rye/.ai/tools/rye/registry/
```

**Step 8.2:** Create __init__.py
```python
"""Registry - primary items distribution (push/pull/auth)."""

from .registry import (
    ACTIONS,
    execute,
)

__all__ = ["ACTIONS", "execute"]
```

**Step 8.3:** Update imports in registry.py

Find and replace:
```python
# OLD:
from lilux.utils.path_service import get_rye_state_dir

# Check if this import exists and works. If lilux is installed, should be fine.
# If not, may need to create a local path utility.
```

Also:
- `kiwi-mcp:validated` → `rye:validated`
- Update primitive imports if needed:
  ```python
  # These should reference core/primitives or core/runtimes
  from ..core.primitives.http_client import ...
  from ..core.runtimes.auth import ...
  ```

**Step 8.4:** Verify
```bash
python -m py_compile rye/rye/.ai/tools/rye/registry/registry.py
```

---

### 9. rag/ - COPY AND ADAPT

**Purpose:** Semantic search - user extends with their embedding API config

**Source:**
```
/home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/rag/rag.py
```

**Target:**
```
rye/rye/.ai/tools/rye/rag/
├── __init__.py
└── rag.py    # ~550 lines, vector search implementation
```

**Step-by-Step Instructions:**

**Step 9.1:** Create directory and copy
```bash
mkdir -p rye/rye/.ai/tools/rye/rag

cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/rag/rag.py \
   rye/rye/.ai/tools/rye/rag/
```

**Step 9.2:** Create __init__.py
```python
"""RAG - semantic search with vector embeddings."""

from .rag import RAGTool

__all__ = ["RAGTool"]
```

**Step 9.3:** Update signature
- `kiwi-mcp:validated` → `rye:validated`

**Step 9.4:** Verify dependencies
The file uses: `httpx`, `numpy`, `sqlite3`
- httpx and numpy should be in rye dependencies
- sqlite3 is stdlib

**Step 9.5:** Verify
```bash
python -m py_compile rye/rye/.ai/tools/rye/rag/rag.py
```

---

### 10. telemetry/ (top-level) - CREATE NEW

**Purpose:** Tool for agent to access MCP server's own logging (NOT spawned agents)

**Target:**
```
rye/rye/.ai/tools/rye/telemetry/
├── __init__.py
└── mcp_logs.py    # Access to MCP server logging
```

**Step-by-Step Instructions:**

**Step 10.1:** Create directory
```bash
mkdir -p rye/rye/.ai/tools/rye/telemetry
```

**Step 10.2:** Create __init__.py
```python
"""Telemetry - access to MCP server logging and diagnostics."""

from .mcp_logs import get_logs, get_log_stats

__all__ = ["get_logs", "get_log_stats"]
```

**Step 10.3:** Create mcp_logs.py
```python
"""
MCP Server Logs Tool

Provides access to the RYE MCP server's own logging and diagnostics.
This is for inspecting the MCP server itself, NOT spawned agents.
For spawned agent telemetry, see agent/telemetry/.
"""

__version__ = "1.0.0"
__tool_type__ = "telemetry"
__category__ = "telemetry"

import logging
from pathlib import Path
from typing import Optional, List, Dict, Any
from datetime import datetime, timezone

# Default log location - can be overridden
DEFAULT_LOG_DIR = Path.home() / ".ai" / "logs" / "rye"


async def get_logs(
    level: Optional[str] = None,
    since: Optional[str] = None,
    limit: int = 100,
    log_dir: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Get recent MCP server logs.
    
    Args:
        level: Filter by log level (DEBUG, INFO, WARNING, ERROR)
        since: ISO timestamp - only logs after this time
        limit: Maximum number of log entries to return
        log_dir: Override default log directory
    
    Returns:
        Dict with log entries and metadata
    """
    logs_path = Path(log_dir) if log_dir else DEFAULT_LOG_DIR
    
    if not logs_path.exists():
        return {
            "entries": [],
            "total": 0,
            "log_dir": str(logs_path),
            "exists": False,
        }
    
    # TODO: Implement log parsing based on actual log format
    # For now, return placeholder
    return {
        "entries": [],
        "total": 0,
        "log_dir": str(logs_path),
        "exists": True,
        "note": "Log parsing not yet implemented - check actual log format",
    }


async def get_log_stats(log_dir: Optional[str] = None) -> Dict[str, Any]:
    """
    Get statistics about MCP server logs.
    
    Returns:
        Dict with log statistics (counts by level, size, etc.)
    """
    logs_path = Path(log_dir) if log_dir else DEFAULT_LOG_DIR
    
    if not logs_path.exists():
        return {
            "exists": False,
            "log_dir": str(logs_path),
        }
    
    # Count log files
    log_files = list(logs_path.glob("*.log"))
    total_size = sum(f.stat().st_size for f in log_files)
    
    return {
        "exists": True,
        "log_dir": str(logs_path),
        "file_count": len(log_files),
        "total_size_bytes": total_size,
        "files": [f.name for f in log_files[:10]],  # First 10 files
    }
```

**Step 10.4:** Verify
```bash
python -m py_compile rye/rye/.ai/tools/rye/telemetry/*.py
```

---

### 11. mcp/ (top-level) - CREATE NEW

**Purpose:** MCP protocol tools - user connects any MCP server

**Target:**
```
rye/rye/.ai/tools/rye/mcp/
├── __init__.py
├── connect.py         # Connect to any MCP server
└── discover.py        # Discover available tools from MCP
```

**Step-by-Step Instructions:**

**Step 11.1:** Create directory
```bash
mkdir -p rye/rye/.ai/tools/rye/mcp
```

**Step 11.2:** Copy and adapt from backup
```bash
# These are the generic MCP tools (NOT context7 or other third-party)
cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/mcp/call.py \
   rye/rye/.ai/tools/rye/mcp/connect.py

cp /home/leo/projects/kiwi-mcp/rye-lilux/.ai.bak/tools/rye/agent/mcp/discover.py \
   rye/rye/.ai/tools/rye/mcp/
```

**DO NOT COPY:**
- context7.py (third-party, userspace)
- mcp_tool_template.py (not needed in system)

**Step 11.3:** Create __init__.py
```python
"""MCP - connect to any MCP server."""

from .connect import call_mcp_tool
from .discover import discover_mcp_tools

__all__ = ["call_mcp_tool", "discover_mcp_tools"]
```

**Step 11.4:** Update imports in connect.py and discover.py
- `kiwi-mcp:validated` → `rye:validated`
- Remove any third-party references
- Update MCP client imports if needed

**Step 11.5:** Verify
```bash
python -m py_compile rye/rye/.ai/tools/rye/mcp/*.py
```

---

## Decisions (Resolved)

### 1. capabilities/ structure
**Decision:** Reorganize into two subdirectories:
- `capabilities/scopes/` - Capability definitions (fs, db, net, git, mcp, process)
- `capabilities/tokens/` - Ed25519 signed capability tokens

### 2. Provider-specific configs
**Decision:** Schema in system, configs in userspace
- `agent/llm/` contains a **schema** defining valid LLM config structure
- NO provider-specific yamls (anthropic/openai) in system bundle
- Users configure their providers in userspace

### 3. agent/mcp/
**Decision:** Not a separate concern. Agent recursion is a natural byproduct of agents using rye-os MCP. The SafetyHarness exposes rye-os tools to LLM threads. If a directive needs to spawn another agent, it uses rye-os tooling. No special "agent/mcp/" needed.

### 4. Primitives
**Decision:** Schemas only. Current rye-os primitives are correct. Lilux kernel handles execution.

### 5. telemetry/ vs agent/telemetry/
**Decision:** Confirmed distinction:
- `telemetry/` = Tools to access MCP server's own logging
- `agent/telemetry/` = Tools to monitor spawned agent execution via SafetyHarness

---

## Execution Order

Execute tasks IN ORDER. Each task depends on previous ones.

| # | Task | Status | Time Est. |
|---|------|--------|-----------|
| 1 | core/protocol/ | ✅ DONE | - |
| 2 | core/sinks/ | ✅ DONE | - |
| 3 | agent/capabilities/ | ⏳ TODO | 15 min |
| 4 | agent/telemetry/ | ⏳ TODO | 15 min |
| 5 | agent/llm/ | ⏳ TODO | 10 min |
| 6 | agent/harness/ | ⏳ TODO | 20 min |
| 7 | agent/threads/ | ⏳ TODO | 15 min |
| 8 | registry/ | ⏳ TODO | 15 min |
| 9 | rag/ | ⏳ TODO | 10 min |
| 10 | telemetry/ | ⏳ TODO | 10 min |
| 11 | mcp/ | ⏳ TODO | 15 min |

**Total estimated time: ~2 hours**

---

## Import Path Reference

When updating imports, use these mappings:

| Old Import | New Import |
|------------|------------|
| `kiwi_mcp.safety_harness.capabilities` | `..capabilities.scopes` |
| `kiwi_mcp.handlers.directive.handler` | `rye.handlers.directive.handler` |
| `from telemetry_lib import` | `from .lib import` |
| `from .capabilities import` | `from ..capabilities.scopes import` |
| Signature: `kiwi-mcp:validated` | `rye:validated` |

**Lilux imports** (should work if lilux is installed):
- `lilux.utils.path_service`
- `lilux.runtime.auth`

---

## Final Validation Checklist

Run after ALL tasks complete:

```bash
# 1. Compile all Python files
find rye/rye/.ai/tools/rye -name "*.py" -exec python -m py_compile {} \;

# 2. Check for old signatures
grep -r "kiwi-mcp:validated" rye/rye/.ai/tools/rye/

# 3. Check for old imports
grep -r "kiwi_mcp" rye/rye/.ai/tools/rye/

# 4. Verify structure
find rye/rye/.ai/tools/rye -type d | sort
```

**Checklist:**
- [ ] All .py files compile without syntax errors
- [ ] No `kiwi-mcp:validated` signatures remain (all updated to `rye:validated`)
- [ ] No `kiwi_mcp` imports remain
- [ ] No third-party tools (context7.py, supabase, etc.)
- [ ] system.py has `time` item and requires `USER_SPACE`
- [ ] Extractors in subdirectories: tool/, directive/, knowledge/
- [ ] Capabilities in subdirectories: scopes/, tokens/
- [ ] agent/harness/ contains safety_harness.py and directive_runner.py
- [ ] agent/threads/ does NOT contain capabilities, tokens, or harness files

---

## Final Structure Verification

After all tasks, structure should be:

```
rye/rye/.ai/tools/rye/
├── core/                           # ✅ COMPLETE
│   ├── extractors/
│   │   ├── directive/
│   │   ├── knowledge/
│   │   └── tool/
│   ├── parsers/
│   ├── primitives/
│   ├── protocol/
│   ├── runtimes/
│   ├── sinks/
│   └── system/
├── agent/
│   ├── capabilities/
│   │   ├── scopes/                 # db, fs, git, mcp, net, process
│   │   └── tokens/                 # capability_tokens
│   ├── harness/                    # safety_harness, directive_runner
│   ├── llm/                        # config_schema, pricing
│   ├── telemetry/                  # lib, status, configure, etc.
│   └── threads/                    # spawn, registry, pause, resume, etc.
├── mcp/                            # connect, discover
├── rag/                            # rag tool
├── registry/                       # registry tool
└── telemetry/                      # mcp_logs
```
