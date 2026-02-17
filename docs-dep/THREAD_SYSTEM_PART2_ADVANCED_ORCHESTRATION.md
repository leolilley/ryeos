# Thread System v2 — Part 2: Advanced Orchestration

> **Extends:** [`THREAD_SYSTEM_IMPLEMENTATION_PLAN.md`](THREAD_SYSTEM_IMPLEMENTATION_PLAN.md) (Part 1)
> **Target directory:** `rye/rye/.ai/tools/rye/agent/threads/`
> **Architecture:** Same data-driven patterns — YAML config, 4 primary tools × 3 item types, ToolDispatcher, condition_evaluator, interpolation engine

Part 1 covers the core single-thread loop: runner, safety_harness, ToolDispatcher, config loaders, internal tools. This document extends that architecture with hierarchical budgets, streaming tool-call parsing, crash recovery, context window management, and thread continuation chains.

---

## Table of Contents

1. [Scope and Relationship to Part 1](#1-scope-and-relationship-to-part-1)
2. [Extended Module Dependency Graph](#2-extended-module-dependency-graph)
3. [Thread Registry Extensions](#3-thread-registry-extensions)
4. [Hierarchical Budget Enforcement](#4-hierarchical-budget-enforcement)
5. [Provider Streaming and Tool-Call Parsing](#5-provider-streaming-and-tool-call-parsing)
6. [State Persistence and Crash Recovery](#6-state-persistence-and-crash-recovery)
7. [Context Limit and Continuation](#7-context-limit-and-continuation)
8. [Extended Configuration](#8-extended-configuration)
9. [Testing Strategy](#9-testing-strategy)
10. [Migration Checklist](#10-migration-checklist)

---

## 1. Scope and Relationship to Part 1

### What Part 1 Establishes (Already Defined — Do Not Redefine)

| Component                                                         | Part 1 Reference | Status                  |
| ----------------------------------------------------------------- | ---------------- | ----------------------- |
| `config_loader.py` — YAML + extends + merge-by-id                 | §6.1             | Defined                 |
| `condition_evaluator.py` — `path`/`op`/`value` + combinators      | §7.3             | Defined                 |
| `interpolation.py` — `${...}` template engine                     | §7.1             | Defined                 |
| `tool_dispatcher.py` — action dicts → core tool `handle()` kwargs | §6.7             | Defined                 |
| `safety_harness.py` — limits, hook evaluation, cancellation       | §6.10            | Defined                 |
| `runner.py` — single-thread LLM loop                              | §6.9             | Defined                 |
| `event_emitter.py` — criticality routing from config              | §6.6             | Defined                 |
| All config loaders (events, error, hooks, resilience)             | §6.2–§6.5        | Defined                 |
| All internal tools (control, emitter, classifier, etc.)           | §5               | Defined                 |
| `orchestrator.py` — basic wait/cancel/status                      | §6.11            | Defined (extended here) |
| `budgets.py` — basic reserve/report/release                       | §6.14            | Defined (extended here) |
| `thread_registry.py` — basic register/status                      | §6.15            | Defined (extended here) |
| `state_store.py` — atomic JSON persistence                        | §6.13            | Defined (extended here) |

### What Part 2 Adds

| Capability                                    | New/Extended Module                                                        | Section |
| --------------------------------------------- | -------------------------------------------------------------------------- | ------- |
| Cost columns, hierarchy queries, PID tracking | `persistence/thread_registry.py` (extended)                                | §3      |
| Continuation chain columns and queries        | `persistence/thread_registry.py` (extended)                                | §3      |
| Hierarchical budget with IMMEDIATE isolation  | `persistence/budgets.py` (extended)                                        | §4      |
| Concurrent spend tracking, fail-loud errors   | `persistence/budgets.py` (extended)                                        | §4      |
| Streaming tool-call parsing (Anthropic)       | `events/streaming_tool_parser.py` (rewritten)                              | §5      |
| Provider streaming integration                | `adapters/provider_adapter.py` (extended)                                  | §5      |
| Crash recovery and orphan detection           | `persistence/state_store.py` (extended), new `internal/orphan_detector.py` | §6      |
| Context limit and thread continuation         | `runner.py` (extended), new `internal/continuation_handoff.py`             | §7      |
| Chain transcript search                       | new `internal/thread_chain_search.py`                                      | §7      |
| Coordination and streaming configs            | `config/coordination.yaml`, `config/streaming.yaml` (new)                  | §8      |

### Deferred Items (Not in Part 2)

| Item                               | Reason                                                                               |
| ---------------------------------- | ------------------------------------------------------------------------------------ |
| **Wave-based orchestration**       | Convenience over manual spawn+wait. Implement after basic thread spawning is proven. |
| **Cross-process registry polling** | Single-process is the only current runtime. Columns added now, polling deferred.     |
| **Thread-to-thread artifacts**     | Orchestrator LLM passes data between waves via inputs. No separate store needed.     |
| **Managed subprocess**             | Requires platform-specific process group management.                                 |
| **Distributed coordination**       | Future scope — `hostname` column in registry is a placeholder.                       |
| **Per-wave git snapshots**         | Requires git integration outside thread system.                                      |

### Design Invariants (Carried from Part 1)

1. **4 primary actions × 3 item types** — all hook actions and coordination actions use the same `{primary, item_type, item_id, params}` format dispatched through `ToolDispatcher`
2. **Config-driven** — all thresholds, timeouts, policies from YAML, overridable per-project via `extends` + merge-by-id
3. **`condition_evaluator`** — same `path`/`op`/`value` + `any`/`all`/`not` combinators for all matching
4. **`${...}` interpolation** — same engine for hook params and continuation config
5. **Canonical limit names** — `turns`, `tokens`, `spend`, `spawns`, `duration_seconds` (no `max_` prefix)
6. **`actions` not `steps`** — `directive.get("actions")` everywhere
7. **`body` vs `content`** — `body` = user prompt, `content` = raw XML reference
8. **3-tier space** — project > user > system for all discovery
9. **Fail loud, never silently degrade** — missing state is an error, not a default. No clamping, no silent fallbacks. Typed exceptions for every failure mode. See §2.1 Error Types.

---

## 2. Extended Module Dependency Graph

Part 2 additions shown with `[P2]` markers:

```
rye/rye/.ai/tools/rye/agent/threads/
│
├── thread_directive.py         # Entry point (Part 1 §6.8)
├── runner.py                   # Core LLM loop (Part 1 §6.9, extended §5/§7)
├── orchestrator.py             # Thread coordination (Part 1 §6.11, extended §7)
├── safety_harness.py           # Thin facade (Part 1 §6.10)
│
├── loaders/                    # Config loaders (Part 1 §6.1–§6.5)
│   ├── config_loader.py        # Base loader (Part 1)
│   ├── condition_evaluator.py  # Shared evaluator (Part 1)
│   ├── interpolation.py        # Template engine (Part 1)
│   ├── events_loader.py        # (Part 1)
│   ├── error_loader.py         # (Part 1)
│   ├── hooks_loader.py         # (Part 1)
│   ├── resilience_loader.py    # (Part 1)
│   ├── coordination_loader.py  # [P2] Load coordination.yaml
│   └── streaming_loader.py     # [P2] Load streaming.yaml
│
├── config/                     # System default YAML configs
│   ├── events.yaml             # (Part 1)
│   ├── error_classification.yaml  # (Part 1)
│   ├── hook_conditions.yaml    # (Part 1)
│   ├── resilience.yaml         # (Part 1)
│   ├── budget_ledger_schema.yaml  # (Part 1)
│   ├── coordination.yaml       # [P2] Continuation + orphan config
│   └── streaming.yaml          # [P2] Streaming + parser config
│
├── adapters/
│   ├── provider_adapter.py     # LLM provider (Part 1 §6.12, extended §5)
│   └── tool_dispatcher.py      # Action translation (Part 1 §6.7)
│
├── persistence/
│   ├── state_store.py          # Atomic state (Part 1 §6.13, extended §6)
│   ├── budgets.py              # Budget ledger (Part 1 §6.14, extended §4)
│   └── thread_registry.py      # Thread registry (Part 1 §6.15, extended §3)
│
├── events/
│   ├── event_emitter.py        # Emit events (Part 1 §6.6)
│   └── streaming_tool_parser.py  # [P2] Rewritten: parse structured events
│
├── security/
│   └── security.py             # Capability tokens (Part 1)
│
└── internal/
    ├── control.py              # (Part 1 §5.1)
    ├── emitter.py              # (Part 1 §5.2)
    ├── classifier.py           # (Part 1 §5.3)
    ├── limit_checker.py        # (Part 1 §5.4)
    ├── state_persister.py      # (Part 1)
    ├── cancel_checker.py       # (Part 1)
    ├── budget_ops.py           # (Part 1 §5.5, extended §4)
    ├── cost_tracker.py         # (Part 1)
    ├── orphan_detector.py      # [P2] Scan for orphaned threads
    ├── continuation_handoff.py # [P2] Handle context limit via continuation
    └── thread_chain_search.py  # [P2] Search across continuation chain transcripts
```

### 2.1 Error Types

All Part 2 modules raise typed exceptions instead of returning None/False/empty dicts. These are classified by the existing `error_classification.yaml` condition evaluator.

```python
# errors.py — Typed exceptions for the thread system
# NOTE: ThreadSystemError, TranscriptCorrupt, ResumeImpossible, ThreadNotFound,
# CheckpointFailed already exist. Part 2 adds the remaining exceptions.

class ThreadSystemError(Exception):
    """Base for all thread system errors."""

class BudgetNotRegistered(ThreadSystemError):
    """Thread has no budget ledger entry. Register before operations."""
    def __init__(self, thread_id: str):
        self.thread_id = thread_id
        super().__init__(f"No budget ledger entry for thread: {thread_id}")

class InsufficientBudget(ThreadSystemError):
    """Parent cannot afford requested reservation."""
    def __init__(self, parent_id: str, remaining: float, requested: float):
        self.parent_id = parent_id
        self.remaining = remaining
        self.requested = requested
        super().__init__(f"Insufficient budget: parent={parent_id} remaining={remaining} requested={requested}")

class BudgetOverspend(ThreadSystemError):
    """Actual spend exceeded reserved amount. Invariant violation."""
    def __init__(self, thread_id: str, reserved: float, actual: float):
        self.thread_id = thread_id
        self.reserved = reserved
        self.actual = actual
        super().__init__(f"Overspend: thread={thread_id} reserved={reserved} actual={actual}")

class BudgetLedgerLocked(ThreadSystemError):
    """SQLite write lock contention. Classified as transient for retry."""
    def __init__(self, operation: str):
        self.operation = operation
        super().__init__(f"Budget ledger locked during: {operation}")

class ThreadWaitTimeout(ThreadSystemError):
    """Timed out waiting for thread completion."""
    def __init__(self, thread_ids: list, timeout: float):
        self.thread_ids = thread_ids
        self.timeout = timeout
        super().__init__(f"Timeout ({timeout}s) waiting for threads: {thread_ids}")

class CompletionEventMissing(ThreadSystemError):
    """In-process thread has no asyncio.Event. Invariant violation."""
    def __init__(self, thread_id: str):
        self.thread_id = thread_id
        super().__init__(f"Missing completion event for in-process thread: {thread_id}")

class ToolInputParseError(ThreadSystemError):
    """Streaming tool input JSON could not be parsed."""
    def __init__(self, tool_id: str, raw: str):
        self.tool_id = tool_id
        self.raw = raw[:200]
        super().__init__(f"Failed to parse tool input for {tool_id}")

class PidCheckUnknown(ThreadSystemError):
    """Cannot determine if PID is alive (permission denied)."""
    def __init__(self, pid: int):
        self.pid = pid
        super().__init__(f"Cannot check PID {pid}: permission denied")

class ContinuationFailed(ThreadSystemError):
    """Failed to spawn continuation thread."""
    def __init__(self, thread_id: str, reason: str):
        self.thread_id = thread_id
        self.reason = reason
        super().__init__(f"Continuation failed for {thread_id}: {reason}")

class ChainResolutionError(ThreadSystemError):
    """Cycle or break in continuation chain during resolution."""
    def __init__(self, thread_id: str, chain_issue: str):
        self.thread_id = thread_id
        self.chain_issue = chain_issue
        super().__init__(f"Chain resolution error at {thread_id}: {chain_issue}")

# NOTE: TranscriptCorrupt, ResumeImpossible, ThreadNotFound, CheckpointFailed
# already exist in the current errors.py — keep them unchanged.
```

These error types are registered in `error_classification.yaml` as patterns:

```yaml
# Additional patterns for thread system errors (appended to error_classification.yaml)
- id: "budget_ledger_locked"
  category: "transient"
  retryable: true
  match:
    path: "error.type"
    op: "eq"
    value: "BudgetLedgerLocked"
  retry_policy:
    type: "fixed"
    delay: 0.1

- id: "budget_insufficient"
  category: "permanent"
  retryable: false
  match:
    path: "error.type"
    op: "eq"
    value: "InsufficientBudget"

- id: "checkpoint_failed"
  category: "permanent"
  retryable: false
  match:
    path: "error.type"
    op: "eq"
    value: "CheckpointFailed"

- id: "continuation_failed"
  category: "permanent"
  retryable: false
  match:
    path: "error.type"
    op: "eq"
    value: "ContinuationFailed"

- id: "chain_resolution_error"
  category: "permanent"
  retryable: false
  match:
    path: "error.type"
    op: "eq"
    value: "ChainResolutionError"
```

---

## 3. Thread Registry Extensions

### Problem

Part 1's registry has a minimal schema: `thread_id`, `directive`, `parent_id`, `status`, timestamps, `result`. It lacks:

1. **Cost tracking** — no way to query how much a thread has spent without going to the budget ledger
2. **Hierarchy queries** — no recursive CTE for subtree/ancestor traversal
3. **Process tracking** — no PID column for orphan detection after crashes
4. **Continuation chain** — no columns to link continued threads

### Extended Registry Schema

```sql
-- Extended thread_registry schema (migrate from Part 1)
CREATE TABLE IF NOT EXISTS threads (
    thread_id       TEXT PRIMARY KEY,
    parent_id       TEXT,
    directive       TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'created',  -- created|running|completed|error|suspended|cancelled|continued
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    completed_at    TEXT,
    result          TEXT,

    -- Cost snapshot (updated post-turn and on completion)
    turns           INTEGER DEFAULT 0,
    input_tokens    INTEGER DEFAULT 0,
    output_tokens   INTEGER DEFAULT 0,
    spend           REAL DEFAULT 0.0,
    spawn_count     INTEGER DEFAULT 0,

    -- Process tracking (for orphan detection)
    pid             INTEGER,          -- OS process ID that owns this thread

    -- Model
    model           TEXT,

    -- Continuation chain (§7)
    continuation_of       TEXT,       -- Thread this continues from
    continuation_thread_id TEXT,      -- Thread that continues this one
    chain_root_id         TEXT,       -- Root of the continuation chain

    FOREIGN KEY (parent_id) REFERENCES threads(thread_id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_threads_parent ON threads(parent_id);
CREATE INDEX IF NOT EXISTS idx_threads_status ON threads(status);
CREATE INDEX IF NOT EXISTS idx_threads_pid ON threads(pid);
```

### Extended Registry Operations

```python
# persistence/thread_registry.py — Extended for Part 2

class ThreadRegistry:
    """SQLite-backed thread lifecycle registry.

    DB location: {project_path}/.ai/threads/registry.db
    WAL mode for concurrent readers with single writer.
    """

    def __init__(self, project_path: Path):
        self.db_path = project_path / ".ai" / "threads" / "registry.db"
        self._ensure_schema()

    def _ensure_schema(self):
        import sqlite3
        with sqlite3.connect(str(self.db_path)) as conn:
            conn.execute("PRAGMA journal_mode=WAL")
            conn.executescript(SCHEMA_SQL)

    # --- Part 1 operations (unchanged) ---
    def register(self, thread_id, directive, parent_id=None): ...
    def update_status(self, thread_id, status): ...
    def get_status(self, thread_id) -> Optional[str]: ...
    def get_thread(self, thread_id) -> Optional[Dict]: ...
    def list_active(self) -> List[Dict]: ...
    def list_children(self, parent_id) -> List[Dict]: ...
    def set_result(self, thread_id, result): ...

    # --- Part 2: Hierarchy Queries ---

    def get_children(self, parent_id: str, recursive: bool = False) -> List[Dict]:
        """Get immediate children, or full subtree if recursive=True."""
        if not recursive:
            return self._query("SELECT * FROM threads WHERE parent_id = ?", (parent_id,))

        # Recursive CTE for full subtree
        return self._query("""
            WITH RECURSIVE subtree AS (
                SELECT * FROM threads WHERE parent_id = ?
                UNION ALL
                SELECT t.* FROM threads t
                JOIN subtree s ON t.parent_id = s.thread_id
            )
            SELECT * FROM subtree
        """, (parent_id,))

    def get_ancestors(self, thread_id: str) -> List[Dict]:
        """Get all ancestors up to root (for budget chain traversal)."""
        return self._query("""
            WITH RECURSIVE ancestors AS (
                SELECT * FROM threads WHERE thread_id = (
                    SELECT parent_id FROM threads WHERE thread_id = ?
                )
                UNION ALL
                SELECT t.* FROM threads t
                JOIN ancestors a ON t.thread_id = a.parent_id
            )
            SELECT * FROM ancestors
        """, (thread_id,))

    # --- Part 2: Cost Aggregation ---

    def aggregate_cost(self, thread_id: str) -> Dict:
        """Aggregate cost across entire subtree (thread + all descendants)."""
        row = self._query_one("""
            WITH RECURSIVE subtree AS (
                SELECT * FROM threads WHERE thread_id = ?
                UNION ALL
                SELECT t.* FROM threads t
                JOIN subtree s ON t.parent_id = s.thread_id
            )
            SELECT
                COUNT(*) as thread_count,
                SUM(turns) as total_turns,
                SUM(input_tokens) as total_input_tokens,
                SUM(output_tokens) as total_output_tokens,
                SUM(spend) as total_spend,
                SUM(spawn_count) as total_spawns,
                SUM(CASE WHEN status = 'completed' THEN 1 ELSE 0 END) as completed,
                SUM(CASE WHEN status = 'error' THEN 1 ELSE 0 END) as errored,
                SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END) as running
            FROM subtree
        """, (thread_id,))
        return dict(row) if row else {}

    def update_cost_snapshot(self, thread_id: str, cost: Dict) -> None:
        """Update cost columns from runner's cost dict (called post-turn)."""
        self._execute("""
            UPDATE threads SET
                turns = ?, input_tokens = ?, output_tokens = ?,
                spend = ?, updated_at = datetime('now')
            WHERE thread_id = ?
        """, (cost.get("turns", 0), cost.get("input_tokens", 0),
              cost.get("output_tokens", 0), cost.get("spend", 0.0), thread_id))

    # --- Part 2: Orphan Detection ---

    def find_orphans(self, current_pid: int) -> Dict[str, List[Dict]]:
        """Find threads marked 'running' whose PID is not alive.

        Returns {confirmed: [...], uncertain: [...]} — never silently
        assumes a thread is orphaned when PID status is unknown.
        """
        candidates = self._query(
            "SELECT * FROM threads WHERE status = 'running' AND pid != ?",
            (current_pid,),
        )
        confirmed = []
        uncertain = []
        for thread in candidates:
            status = _check_pid_status(thread["pid"])
            if status == "dead":
                confirmed.append(thread)
            elif status == "unknown":
                uncertain.append(thread)
            # status == "alive" → not orphaned, skip
        return {"confirmed": confirmed, "uncertain": uncertain}

    def mark_orphan(self, thread_id: str) -> None:
        """Mark an orphaned thread as suspended for recovery."""
        self._execute("""
            UPDATE threads SET status = 'suspended', updated_at = datetime('now')
            WHERE thread_id = ? AND status = 'running'
        """, (thread_id,))

    # --- Part 2: Continuation Chain ---

    def set_continuation(self, thread_id: str, continuation_thread_id: str) -> None:
        """Mark thread as continued with forward pointer."""
        self._execute("""
            UPDATE threads SET
                continuation_thread_id = ?,
                status = 'continued',
                updated_at = datetime('now')
            WHERE thread_id = ?
        """, (continuation_thread_id, thread_id))

    def set_chain_info(self, thread_id: str, chain_root_id: str,
                       continuation_of: str) -> None:
        """Set chain metadata for a continuation thread."""
        self._execute("""
            UPDATE threads SET
                chain_root_id = ?,
                continuation_of = ?,
                updated_at = datetime('now')
            WHERE thread_id = ?
        """, (chain_root_id, continuation_of, thread_id))

    def get_chain(self, thread_id: str) -> List[Dict]:
        """Get full continuation chain containing this thread.

        Walks backward to root via continuation_of, then forward
        via continuation_thread_id to collect the full chain in order.
        """
        # Find root first
        root_id = thread_id
        visited = set()
        while True:
            if root_id in visited:
                raise ChainResolutionError(thread_id, f"Cycle at {root_id}")
            visited.add(root_id)
            thread = self.get_thread(root_id)
            if not thread:
                break
            prev = thread.get("continuation_of")
            if not prev:
                break
            root_id = prev

        # Walk forward from root
        chain = []
        current = root_id
        visited.clear()
        while current:
            if current in visited:
                raise ChainResolutionError(thread_id, f"Cycle at {current}")
            visited.add(current)
            thread = self.get_thread(current)
            if not thread:
                break
            chain.append(thread)
            current = thread.get("continuation_thread_id")

        return chain


def _check_pid_status(pid: int) -> str:
    """Check process status. Returns 'alive', 'dead', or 'unknown'.

    'unknown' means we lack permission to check — do NOT treat as dead.
    """
    import os
    if pid is None:
        return "dead"
    try:
        os.kill(pid, 0)
        return "alive"
    except ProcessLookupError:
        return "dead"
    except PermissionError:
        return "unknown"  # Cannot determine — do NOT assume dead
```

---

## 4. Hierarchical Budget Enforcement

### Problem

Part 1's `budgets.py` has correctness issues that must be fixed:

1. **No transaction isolation** — `reserve()` reads remaining budget and inserts in separate steps, allowing concurrent over-reservation
2. **Silent failure** — `reserve()` returns `False` instead of raising, callers must check return values
3. **Clamping** — `report_actual()` clamps to reserved amount instead of raising on overspend
4. **No `can_spawn`** — LLM has no way to pre-check if a spawn is affordable
5. **No tree queries** — no way to see total spend across a thread subtree

### Design: SQLite IMMEDIATE Isolation + Fail-Loud

All budget mutations use `BEGIN IMMEDIATE` transactions. SQLite IMMEDIATE acquires a write lock at transaction start (not at first write), preventing concurrent transactions from reading stale remaining-budget values.

All error conditions raise typed exceptions — no silent returns.

### Extended Budget Ledger

```python
# persistence/budgets.py — Extended for Part 2

import sqlite3
import os
from pathlib import Path
from typing import Dict, Optional
from datetime import datetime, timezone

DB_FILE = "budget_ledger.db"

TERMINAL_STATUSES = frozenset({"completed", "cancelled", "error"})

class BudgetLedger:
    """SQLite-backed hierarchical budget tracking.

    DB location: {project_path}/.ai/threads/budget_ledger.db

    Key invariant: reserve() uses BEGIN IMMEDIATE to prevent concurrent
    over-reservation. Two threads trying to reserve from the same parent
    are serialized at the transaction level.
    """

    def __init__(self, project_path: Path):
        self.db_path = project_path / ".ai" / "threads" / DB_FILE
        self._ensure_schema()

    def _ensure_schema(self):
        with self._connect() as conn:
            conn.execute("PRAGMA journal_mode=WAL")
            conn.executescript("""
                CREATE TABLE IF NOT EXISTS budget_ledger (
                    thread_id        TEXT PRIMARY KEY,
                    parent_thread_id TEXT,
                    reserved_spend   REAL NOT NULL DEFAULT 0.0,
                    actual_spend     REAL NOT NULL DEFAULT 0.0,
                    max_spend        REAL,
                    status           TEXT NOT NULL DEFAULT 'active',
                    created_at       TEXT NOT NULL,
                    updated_at       TEXT NOT NULL,
                    FOREIGN KEY (parent_thread_id)
                        REFERENCES budget_ledger(thread_id) ON DELETE RESTRICT
                );
                CREATE INDEX IF NOT EXISTS idx_budget_parent
                    ON budget_ledger(parent_thread_id);
                CREATE INDEX IF NOT EXISTS idx_budget_status
                    ON budget_ledger(status);
            """)

    def _connect(self) -> sqlite3.Connection:
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(str(self.db_path), timeout=5.0)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA foreign_keys = ON")
        return conn

    # --- Registration ---

    def register(self, thread_id: str, max_spend: Optional[float] = None,
                 parent_thread_id: Optional[str] = None) -> None:
        """Register a thread's budget. Called before runner.run()."""
        now = datetime.now(timezone.utc).isoformat()
        with self._connect() as conn:
            conn.execute("""
                INSERT OR IGNORE INTO budget_ledger
                    (thread_id, parent_thread_id, max_spend, status, created_at, updated_at)
                VALUES (?, ?, ?, 'active', ?, ?)
            """, (thread_id, parent_thread_id, max_spend, now, now))

    # --- Reservation (atomic) ---

    def reserve(self, child_thread_id: str, amount: float,
                parent_thread_id: str, child_max_spend: Optional[float] = None) -> None:
        """Atomically reserve budget from parent for child.

        Uses BEGIN IMMEDIATE to prevent concurrent over-reservation.
        Raises InsufficientBudget if parent has insufficient remaining.
        Raises BudgetNotRegistered if parent has no ledger entry.

        Remaining = max_spend - actual_spend - sum(children.reserved_spend where active)
        """
        now = datetime.now(timezone.utc).isoformat()
        conn = self._connect()
        try:
            conn.execute("BEGIN IMMEDIATE")

            # Calculate parent's remaining budget
            row = conn.execute("""
                SELECT
                    COALESCE(bl.max_spend, 0) - bl.actual_spend
                    - COALESCE((
                        SELECT SUM(c.reserved_spend)
                        FROM budget_ledger c
                        WHERE c.parent_thread_id = bl.thread_id
                          AND c.status = 'active'
                    ), 0) as remaining
                FROM budget_ledger bl
                WHERE bl.thread_id = ?
            """, (parent_thread_id,)).fetchone()

            if row is None:
                conn.rollback()
                raise BudgetNotRegistered(parent_thread_id)

            remaining = row["remaining"]
            if remaining is None or remaining < amount:
                conn.rollback()
                raise InsufficientBudget(parent_thread_id, remaining or 0.0, amount)

            # Insert child reservation
            conn.execute("""
                INSERT INTO budget_ledger
                    (thread_id, parent_thread_id, reserved_spend, max_spend,
                     status, created_at, updated_at)
                VALUES (?, ?, ?, ?, 'active', ?, ?)
            """, (child_thread_id, parent_thread_id, amount,
                  child_max_spend or amount, now, now))

            conn.commit()

        except sqlite3.OperationalError as e:
            conn.rollback()
            if "database is locked" in str(e):
                raise BudgetLedgerLocked("reserve") from e
            raise
        except Exception:
            conn.rollback()
            raise
        finally:
            conn.close()

    # --- Spend Reporting ---

    def report_actual(self, thread_id: str, amount: float) -> None:
        """Report actual spend. Raises BudgetOverspend if amount exceeds reserved."""
        now = datetime.now(timezone.utc).isoformat()
        with self._connect() as conn:
            row = conn.execute(
                "SELECT reserved_spend FROM budget_ledger WHERE thread_id = ?",
                (thread_id,),
            ).fetchone()
            if row is None:
                raise BudgetNotRegistered(thread_id)
            if amount > row["reserved_spend"]:
                raise BudgetOverspend(thread_id, row["reserved_spend"], amount)
            conn.execute("""
                UPDATE budget_ledger SET actual_spend = ?, updated_at = ?
                WHERE thread_id = ?
            """, (amount, now, thread_id))

    def increment_actual(self, thread_id: str, delta: float) -> None:
        """Increment actual spend by delta. Raises BudgetOverspend on overspend."""
        now = datetime.now(timezone.utc).isoformat()
        with self._connect() as conn:
            row = conn.execute(
                "SELECT actual_spend, reserved_spend FROM budget_ledger WHERE thread_id = ?",
                (thread_id,),
            ).fetchone()
            if row is None:
                raise BudgetNotRegistered(thread_id)
            new_actual = row["actual_spend"] + delta
            if new_actual > row["reserved_spend"]:
                raise BudgetOverspend(thread_id, row["reserved_spend"], new_actual)
            conn.execute("""
                UPDATE budget_ledger SET actual_spend = ?, updated_at = ?
                WHERE thread_id = ?
            """, (new_actual, now, thread_id))

    # --- Release ---

    def release(self, thread_id: str, final_status: str = "completed") -> None:
        """Release reservation on thread completion/error/cancel.

        Sets reserved_spend = actual_spend (frees unused reservation).
        Parent's remaining budget increases by (old_reserved - actual_spend).
        """
        now = datetime.now(timezone.utc).isoformat()
        with self._connect() as conn:
            conn.execute("""
                UPDATE budget_ledger SET
                    reserved_spend = actual_spend,
                    status = ?,
                    updated_at = ?
                WHERE thread_id = ?
            """, (final_status, now, thread_id))

    # --- Queries ---

    def get_remaining(self, thread_id: str) -> float:
        """Get remaining budget for a thread. Raises BudgetNotRegistered if missing."""
        with self._connect() as conn:
            row = conn.execute("""
                SELECT
                    COALESCE(bl.max_spend, 0) - bl.actual_spend
                    - COALESCE((
                        SELECT SUM(c.reserved_spend)
                        FROM budget_ledger c
                        WHERE c.parent_thread_id = bl.thread_id
                          AND c.status = 'active'
                    ), 0) as remaining
                FROM budget_ledger bl
                WHERE bl.thread_id = ?
            """, (thread_id,)).fetchone()
            if row is None:
                raise BudgetNotRegistered(thread_id)
            return row["remaining"]

    def get_tree_spend(self, thread_id: str) -> Dict:
        """Get total actual spend across entire subtree."""
        with self._connect() as conn:
            row = conn.execute("""
                WITH RECURSIVE subtree AS (
                    SELECT thread_id, actual_spend, reserved_spend, status
                    FROM budget_ledger WHERE thread_id = ?
                    UNION ALL
                    SELECT bl.thread_id, bl.actual_spend, bl.reserved_spend, bl.status
                    FROM budget_ledger bl
                    JOIN subtree s ON bl.parent_thread_id = s.thread_id
                )
                SELECT
                    SUM(actual_spend) as total_actual,
                    SUM(reserved_spend) as total_reserved,
                    COUNT(*) as thread_count,
                    SUM(CASE WHEN status = 'active' THEN 1 ELSE 0 END) as active_count
                FROM subtree
            """, (thread_id,)).fetchone()
            return dict(row) if row else {}

    def can_spawn(self, parent_thread_id: str, requested_budget: float) -> Dict:
        """Pre-check whether a spawn is affordable. Does not reserve.

        Returns {affordable: bool, remaining: float, requested: float}.
        Raises BudgetNotRegistered if parent has no ledger entry.
        """
        remaining = self.get_remaining(parent_thread_id)  # Raises if not registered
        return {
            "affordable": remaining >= requested_budget,
            "remaining": remaining,
            "requested": requested_budget,
        }
```

### Budget Flow Diagram

```
Parent (max_spend=$3.00)
│
├── own actual_spend: $0.08 (orchestrator LLM turns)
│
├── reserve(child_A, $0.80) → remaining: $3.00 - $0.08 - $0.80 = $2.12
├── reserve(child_B, $0.80) → remaining: $2.12 - $0.80 = $1.32
│
│   child_A completes: actual=$0.45
│   └── release(child_A) → reserved becomes $0.45
│       → parent remaining: $3.00 - $0.08 - $0.45 - $0.80 = $1.67
│
├── reserve(child_C, $0.80) → remaining: $1.67 - $0.80 = $0.87 ✓
│
│   child_B completes: actual=$0.72
│   └── release(child_B) → reserved becomes $0.72
│       → parent remaining: $3.00 - $0.08 - $0.45 - $0.72 - $0.80 = $0.95
│
├── reserve(child_D, $1.00) → remaining: $0.95 < $1.00 ✗ RAISES InsufficientBudget
│   └── hook fires: limit event with limit_code "hierarchical_budget_exceeded"
```

### Integration with Safety Harness

The harness's `check_limits()` (Part 1 §6.10) is extended with a budget check:

```python
# In safety_harness.py — extended check_limits()

def check_limits(self, cost: Dict) -> Optional[Dict]:
    """Check all limits against current cost. Returns limit event or None."""
    # Part 1 checks: turns, tokens, spend (local)
    result = self._check_local_limits(cost)
    if result:
        return result

    # Part 2: hierarchical budget check
    if self._budget_ledger and self.thread_id:
        remaining = self._budget_ledger.get_remaining(self.thread_id)
        if remaining <= 0:
            return {
                "limit_code": "hierarchical_budget_exceeded",
                "current_value": cost.get("spend", 0.0),
                "current_max": remaining,
            }

    return None
```

### Extended budget_ops.py

Part 1 §5.5 defines basic operations. Part 2 adds `can_spawn`, `increment_actual`, and `get_tree_spend`:

```python
# internal/budget_ops.py — Extended for Part 2

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {
            "type": "string",
            "enum": [
                "reserve", "report_actual", "release", "check_remaining",
                "can_spawn", "increment_actual", "get_tree_spend",
            ],
        },
        "thread_id": {"type": "string"},
        "parent_thread_id": {"type": "string"},
        "amount": {"type": "number"},
        "final_status": {"type": "string"},
    },
    "required": ["operation", "thread_id"],
}

def execute(params: Dict, project_path: str) -> Dict:
    from ..persistence import budgets
    ledger = budgets.get_ledger(Path(project_path))
    operation = params["operation"]

    # Part 1 operations — now fail-loud (reserve raises, not returns False)

    if operation == "reserve":
        parent_id = params.get("parent_thread_id")
        amount = params.get("amount", 0.0)
        ledger.reserve(params["thread_id"], amount, parent_id)
        return {"success": True, "reserved": amount}

    if operation == "report_actual":
        ledger.report_actual(params["thread_id"], params.get("amount", 0.0))
        return {"success": True}

    if operation == "release":
        ledger.release(params["thread_id"], params.get("final_status", "completed"))
        return {"success": True}

    if operation == "check_remaining":
        remaining = ledger.get_remaining(params["thread_id"])
        return {"success": True, "remaining": remaining}

    # Part 2 additions

    if operation == "can_spawn":
        return ledger.can_spawn(params["thread_id"], params.get("amount", 0.0))

    if operation == "increment_actual":
        ledger.increment_actual(params["thread_id"], params.get("amount", 0.0))
        return {"success": True}

    if operation == "get_tree_spend":
        return {"success": True, **ledger.get_tree_spend(params["thread_id"])}

    return {"success": False, "error": f"Unknown operation: {operation}"}
```

---

## 5. Provider Streaming and Tool-Call Parsing

### Problem

Part 1's runner uses batch-after-response: the full LLM response arrives, then tool calls execute sequentially. This wastes time — tools could start executing as soon as their definition appears in the stream.

The current `streaming_tool_parser.py` is an SSE text parser (splits on `\n\n`, parses `data:` lines). Part 2 replaces it with a structured event parser that consumes pre-parsed provider events.

### Design: Streaming Pipeline

```
LLM SSE Stream ──► ProviderAdapter.create_streaming_completion()
                     │ (parses raw SSE into event dicts)
                     ▼
                StreamingToolParser.feed_event(event_dict)
                     │
               ┌─────┴──────────────────────┐
               │                            │
          text chunks                 tool definitions
               │                            │
       cognition_out_delta          pending_tools buffer
       (droppable event)                    │
                                   batch dispatch trigger:
                                     • 5 tools ready, OR
                                     • stream ends
                                            │
                                   ToolDispatcher.dispatch_parallel()
                                            │
                                   tool_call_start + tool_call_result
                                   (critical events)
```

### StreamingToolParser (Rewrite)

```python
# events/streaming_tool_parser.py — Rewritten for Part 2

from typing import Any, Dict, Iterator, List, Tuple

class StreamingToolParser:
    """Parse tool calls from structured LLM response events.

    Consumes pre-parsed event dicts from ProviderAdapter (not raw SSE text).
    Supports Anthropic content_block_delta format.

    Yields events as tool definitions complete mid-stream:
        ('text', str)           — text delta
        ('reasoning', str)      — thinking/reasoning delta
        ('tool_complete', Dict) — complete tool call ready to execute
        ('tool_delta', Dict)    — partial tool input (for progress tracking)
        ('stream_end', Dict)    — stream finished with usage stats
    """

    def __init__(self, max_tool_input_size: int = 1048576, max_text_buffer: int = 10485760):
        self._index_to_tool_id: Dict[int, str] = {}  # content_block index → tool_id
        self._partial_tools: Dict[str, Dict] = {}     # tool_id → partial tool
        self._partial_inputs: Dict[str, str] = {}     # tool_id → accumulated JSON string
        self._text_buffer: List[str] = []
        self._reasoning_buffer: List[str] = []
        self._full_text: str = ""
        self._usage: Dict[str, int] = {}
        self._max_tool_input_size = max_tool_input_size
        self._max_text_buffer = max_text_buffer

    def feed_event(self, event: Dict) -> Iterator[Tuple[str, Any]]:
        """Process a parsed SSE event dict.

        The provider adapter parses raw SSE lines into event dicts.
        This parser extracts tool calls and text from those events.
        """
        event_type = event.get("type", "")

        if event_type == "content_block_start":
            content_block = event.get("content_block", {})
            index = event.get("index")
            if content_block.get("type") == "tool_use":
                tool_id = content_block.get("id", "")
                if index is not None:
                    self._index_to_tool_id[index] = tool_id
                self._partial_tools[tool_id] = {
                    "id": tool_id,
                    "name": content_block.get("name", ""),
                    "input": {},
                }
                self._partial_inputs[tool_id] = ""

        elif event_type == "content_block_delta":
            delta = event.get("delta", {})
            delta_type = delta.get("type", "")

            if delta_type == "text_delta":
                text = delta.get("text", "")
                self._text_buffer.append(text)
                self._full_text += text
                if len(self._full_text) > self._max_text_buffer:
                    raise ToolInputParseError("text_buffer", f"Text exceeds {self._max_text_buffer} bytes")
                yield ("text", text)

            elif delta_type == "thinking_delta":
                text = delta.get("thinking", "")
                self._reasoning_buffer.append(text)
                yield ("reasoning", text)

            elif delta_type == "input_json_delta":
                partial_json = delta.get("partial_json", "")
                # Attribute to correct tool via content_block index
                index = event.get("index")
                tool_id = self._index_to_tool_id.get(index) if index is not None else None
                if tool_id is None:
                    raise ToolInputParseError("unknown", f"No tool for index {index}")
                if tool_id not in self._partial_inputs:
                    raise ToolInputParseError(tool_id, "Received delta for completed tool")
                self._partial_inputs[tool_id] += partial_json
                # Enforce size limit
                if len(self._partial_inputs[tool_id]) > self._max_tool_input_size:
                    raise ToolInputParseError(tool_id, f"Tool input exceeds {self._max_tool_input_size} bytes")
                yield ("tool_delta", {"id": tool_id, "partial": partial_json})

        elif event_type == "content_block_stop":
            # Identify the completed tool by index — never guess
            index = event.get("index")
            tool_id = self._index_to_tool_id.get(index) if index is not None else None
            if tool_id and tool_id in self._partial_tools:
                partial = self._partial_tools[tool_id]
                raw_input = self._partial_inputs.get(tool_id, "")
                try:
                    import json
                    partial["input"] = json.loads(raw_input) if raw_input else {}
                except (json.JSONDecodeError, ValueError) as e:
                    raise ToolInputParseError(tool_id, raw_input[:200]) from e
                self._partial_inputs.pop(tool_id, None)
                del self._partial_tools[tool_id]
                self._index_to_tool_id.pop(index, None)
                yield ("tool_complete", partial)

        elif event_type == "message_delta":
            # Usage stats at end of stream
            usage = event.get("usage", {})
            if usage:
                self._usage.update(usage)

        elif event_type == "message_stop":
            yield ("stream_end", {
                "usage": self._usage,
                "full_text": self._full_text,
                "has_reasoning": bool(self._reasoning_buffer),
            })

    def get_full_text(self) -> str:
        return self._full_text

    def get_reasoning(self) -> str:
        return "".join(self._reasoning_buffer)

    def has_pending_tools(self) -> bool:
        return bool(self._partial_tools)
```

### Streaming Runner Extension

Part 1's `runner.py` (§6.9) defines the batch loop. Part 2 adds an alternative streaming path, selected when the provider config enables streaming:

```python
# runner.py — Extended with streaming mode

async def run(
    thread_id: str,
    system_prompt: str,
    harness: "SafetyHarness",
    provider: "ProviderAdapter",
    dispatcher: "ToolDispatcher",
    emitter: "EventEmitter",
    transcript: Any,
    project_path: Path,
) -> Dict:
    """Execute the LLM loop.

    Delegates to _run_batch or _run_streaming based on provider config.
    """
    if provider.supports_streaming:
        return await _run_streaming(
            thread_id, system_prompt, harness, provider,
            dispatcher, emitter, transcript, project_path,
        )
    else:
        return await _run_batch(...)  # Part 1 loop unchanged


async def _run_streaming(
    thread_id, system_prompt, harness, provider,
    dispatcher, emitter, transcript, project_path,
) -> Dict:
    """Streaming LLM loop with inline tool execution."""
    from .events.streaming_tool_parser import StreamingToolParser

    messages = [{"role": "system", "content": system_prompt}]
    cost = {"turns": 0, "input_tokens": 0, "output_tokens": 0, "spend": 0.0}

    emitter.emit(thread_id, "thread_started", {
        "directive": harness.directive_name,
        "model": provider.model,
        "limits": harness.limits,
        "streaming": True,
    }, transcript)

    while True:
        # Pre-turn checks (same as batch — Part 1 §6.9)
        limit_result = harness.check_limits(cost)
        if limit_result:
            hook_result = await harness.run_hooks("limit", limit_result, dispatcher)
            if hook_result:
                return _finalize(thread_id, cost, hook_result, emitter, transcript)

        if harness.is_cancelled():
            return _finalize(thread_id, cost, {"success": False, "status": "cancelled"},
                             emitter, transcript)

        cost["turns"] += 1
        parser = StreamingToolParser()  # Fresh parser per turn
        pending_tools = []
        completed_tool_results = []

        try:
            async for event in provider.create_streaming_completion(messages, harness.available_tools):
                for event_type, data in parser.feed_event(event):

                    if event_type == "text":
                        emitter.emit_droppable(thread_id, "cognition_out_delta",
                                               {"text": data}, transcript)

                    elif event_type == "reasoning":
                        emitter.emit_droppable(thread_id, "cognition_reasoning",
                                               {"text": data}, transcript)

                    elif event_type == "tool_complete":
                        pending_tools.append(data)
                        emitter.emit(thread_id, "tool_call_start", {
                            "tool": data["name"], "call_id": data["id"],
                            "input": data["input"],
                        }, transcript)

                        # Batch dispatch: execute when 5 tools ready
                        if len(pending_tools) >= 5:
                            results = await dispatcher.dispatch_parallel([
                                {"primary": "execute", "item_type": "tool",
                                 "item_id": t["name"], "params": t["input"]}
                                for t in pending_tools
                            ])
                            for tool, result in zip(pending_tools, results):
                                emitter.emit(thread_id, "tool_call_result", {
                                    "call_id": tool["id"],
                                    "output": str(result)[:1000] if not isinstance(result, Exception) else None,
                                    "error": str(result) if isinstance(result, Exception) else None,
                                }, transcript)
                                completed_tool_results.append({
                                    "call_id": tool["id"], "result": result,
                                })
                            pending_tools.clear()

                    elif event_type == "stream_end":
                        usage = data.get("usage", {})
                        cost["input_tokens"] += usage.get("input_tokens", 0)
                        cost["output_tokens"] += usage.get("output_tokens", 0)
                        cost["spend"] += _calculate_spend(usage, provider.model)

            # Emit complete cognition (critical, always)
            emitter.emit(thread_id, "cognition_out", {
                "text": parser.get_full_text(),
                "model": provider.model,
                "is_partial": False,
            }, transcript)

        except Exception as e:
            # Emit partial cognition on error
            partial_text = parser.get_full_text()
            emitter.emit(thread_id, "cognition_out", {
                "text": partial_text,
                "is_partial": True,
                "error": str(e),
            }, transcript)

            # Classify and handle error (same as batch — Part 1 §6.9)
            classification = error_loader.classify(project_path, _error_to_context(e))
            hook_result = await harness.run_hooks("error",
                {"error": e, "classification": classification}, dispatcher)
            if hook_result:
                if hook_result.get("action") == "retry":
                    delay = error_loader.get_error_loader().calculate_retry_delay(
                        project_path, classification.get("retry_policy", {}), cost["turns"])
                    await asyncio.sleep(delay)
                    continue
                return _finalize(thread_id, cost, hook_result, emitter, transcript)
            return _finalize(thread_id, cost, {"success": False, "error": str(e)},
                             emitter, transcript)

        # Execute remaining pending tools after stream ends
        if pending_tools:
            results = await dispatcher.dispatch_parallel([
                {"primary": "execute", "item_type": "tool",
                 "item_id": t["name"], "params": t["input"]}
                for t in pending_tools
            ])
            for tool, result in zip(pending_tools, results):
                emitter.emit(thread_id, "tool_call_result", {
                    "call_id": tool["id"],
                    "output": str(result)[:1000] if not isinstance(result, Exception) else None,
                    "error": str(result) if isinstance(result, Exception) else None,
                }, transcript)
                completed_tool_results.append({"call_id": tool["id"], "result": result})
            pending_tools.clear()

        # No tool calls = LLM is done
        if not completed_tool_results:
            return _finalize(thread_id, cost,
                {"success": True, "result": parser.get_full_text()}, emitter, transcript)

        # Add tool results to messages for next turn
        messages.append({"role": "assistant", "content": parser.get_full_text()})
        for tr in completed_tool_results:
            result = tr["result"]
            content = str(result) if not isinstance(result, Exception) else f"Error: {result}"
            messages.append({"role": "tool", "tool_call_id": tr["call_id"], "content": content})

        # Post-turn hooks
        await harness.run_hooks("after_step", {"cost": cost}, dispatcher)
```

### Provider Adapter Streaming Extension

```python
# adapters/provider_adapter.py — Extended for Part 2

class ProviderAdapter:
    """LLM provider abstraction. Part 1 defines create_completion().
    Part 2 adds streaming support."""

    def __init__(self, model: str, provider_config: Dict):
        self.model = model
        self.config = provider_config
        self.supports_streaming = provider_config.get("stream", {}).get("enabled", False)

    async def create_completion(self, messages, tools) -> Dict:
        """Batch completion (Part 1 — unchanged)."""
        raise NotImplementedError

    async def create_streaming_completion(self, messages, tools):
        """Stream completion — yields parsed SSE event dicts.

        Each yielded dict is a parsed SSE event (e.g., content_block_delta,
        content_block_stop, message_delta, message_stop).

        The StreamingToolParser consumes these events.
        """
        raise NotImplementedError
```

---

## 6. State Persistence and Crash Recovery

### Problem

Part 1's `state_store.py` defines atomic save/load for `state.json`. But:

1. No checkpoint triggers are wired into the runner
2. No mechanism to detect orphaned threads after a crash
3. No replay logic to reconstruct messages from `transcript.jsonl`

### Design: Checkpoint Triggers + Orphan Detection + Replay

### Runner Integration

The runner saves state at configurable checkpoints (triggers from `resilience.yaml`):

```python
# In runner.py — state persistence integration

async def _save_checkpoint(
    state_store: "StateStore",
    thread_id: str,
    harness: "SafetyHarness",
    cost: Dict,
    messages: List[Dict],
    trigger: str,
    project_path: Path,
) -> None:
    """Save state checkpoint. Raises CheckpointFailed on write error.

    Checkpoint failure is fatal by default — crash recovery depends on
    checkpoints existing. Policy is config-driven via resilience.yaml checkpoint.on_failure.
    """
    from .loaders import resilience_loader
    config = resilience_loader.load(project_path)
    triggers = config.get("checkpoint", {}).get("triggers", {})

    if not triggers.get(trigger, False):
        return

    state = {
        "thread_id": thread_id,
        "directive": harness.directive_name,
        "version": "1.0.0",
        "saved_at": datetime.now(timezone.utc).isoformat(),
        "cost": cost,
        "limits": harness.limits,
        "status": "running",
        "turn_number": cost.get("turns", 0),
        "messages": messages,
        "hooks": [h for h in harness.hooks if h.get("layer") == 1],
    }

    try:
        state_store.save(state)
    except Exception as e:
        on_failure = config.get("checkpoint", {}).get("on_failure", "fail")
        if on_failure == "fail":
            raise CheckpointFailed(thread_id, trigger, e) from e
        import logging
        logging.getLogger(__name__).warning(f"Checkpoint failed ({trigger}): {e}")
```

Checkpoint calls in the runner loop:

```python
# In runner._run_batch / _run_streaming:

while True:
    # Pre-turn checkpoint
    await _save_checkpoint(state_store, thread_id, harness, cost, messages,
                           "pre_turn", project_path)

    # ... LLM call ...

    # Post-LLM checkpoint (cost updated)
    cost["input_tokens"] += response.get("input_tokens", 0)
    cost["output_tokens"] += response.get("output_tokens", 0)
    cost["spend"] += response.get("spend", 0.0)
    await _save_checkpoint(state_store, thread_id, harness, cost, messages,
                           "post_llm", project_path)

    # ... tool execution ...

    # Post-tools checkpoint
    await _save_checkpoint(state_store, thread_id, harness, cost, messages,
                           "post_tools", project_path)
```

### Orphan Detection

An internal tool that scans for threads stuck in "running" whose owner process has exited:

```python
# internal/orphan_detector.py
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Detect and recover orphaned threads"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {"type": "string", "enum": ["scan", "recover"]},
        "thread_id": {"type": "string"},
    },
    "required": ["operation"],
}

def execute(params: Dict, project_path: str) -> Dict:
    """Detect and optionally recover orphaned threads."""
    from ..persistence.thread_registry import ThreadRegistry

    registry = ThreadRegistry(Path(project_path))
    operation = params["operation"]

    if operation == "scan":
        import os
        orphans = registry.find_orphans(os.getpid())
        return {
            "success": True,
            "confirmed": [
                {
                    "thread_id": o["thread_id"],
                    "directive": o["directive"],
                    "pid": o["pid"],
                    "has_state": _has_state_file(o["thread_id"], project_path),
                    "has_transcript": _has_transcript(o["thread_id"], project_path),
                }
                for o in orphans["confirmed"]
            ],
            "uncertain": [
                {
                    "thread_id": o["thread_id"],
                    "pid": o["pid"],
                }
                for o in orphans["uncertain"]
            ],
            "confirmed_count": len(orphans["confirmed"]),
            "uncertain_count": len(orphans["uncertain"]),
        }

    if operation == "recover":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required for recover"}

        # Mark as suspended
        registry.mark_orphan(thread_id)

        # Check what state is available
        state_path = Path(project_path) / ".ai" / "threads" / thread_id / "state.json"
        transcript_path = Path(project_path) / ".ai" / "threads" / thread_id / "transcript.jsonl"

        if state_path.exists():
            return {
                "success": True,
                "thread_id": thread_id,
                "status": "suspended",
                "recovery": "state_available",
                "resume_command": f"resume_thread(thread_id='{thread_id}')",
            }
        elif transcript_path.exists():
            return {
                "success": True,
                "thread_id": thread_id,
                "status": "suspended",
                "recovery": "transcript_only",
                "message": "State must be reconstructed from transcript before resume",
            }
        else:
            registry.update_status(thread_id, "error")
            return {
                "success": True,
                "thread_id": thread_id,
                "status": "error",
                "recovery": "no_state",
                "message": "No recovery data available",
            }


def _has_state_file(thread_id: str, project_path: str) -> bool:
    return (Path(project_path) / ".ai" / "threads" / thread_id / "state.json").exists()

def _has_transcript(thread_id: str, project_path: str) -> bool:
    return (Path(project_path) / ".ai" / "threads" / thread_id / "transcript.jsonl").exists()
```

### Transcript Replay for Message Reconstruction

When resuming from a crash, messages are reconstructed from the transcript JSONL:

```python
# persistence/state_store.py — Extended for Part 2

class StateStore:
    # ... Part 1 methods unchanged ...

    def reconstruct_messages(self) -> Optional[List[Dict]]:
        """Reconstruct conversation messages from transcript.jsonl.

        Used when state.json is missing or corrupt but transcript exists.
        Handles interleaved streaming events (§5) by using cognition_out
        (complete text) and ignoring cognition_out_delta (partial).

        Raises TranscriptCorrupt on unparseable lines.
        """
        transcript_path = self.state_dir / "transcript.jsonl"
        if not transcript_path.exists():
            return None

        import json
        messages = []
        pending_tool_calls = {}

        with open(transcript_path) as f:
            for line_no, line in enumerate(f, 1):
                line = line.strip()
                if not line:
                    continue
                try:
                    event = json.loads(line)
                except json.JSONDecodeError as e:
                    raise TranscriptCorrupt(str(transcript_path), line_no, line[:100]) from e

                event_type = event.get("event_type", "")
                payload = event.get("payload", {})

                if event_type == "cognition_in":
                    messages.append({
                        "role": payload.get("role", "user"),
                        "content": payload.get("text", ""),
                    })

                elif event_type == "cognition_out":
                    # Use complete text, not deltas
                    messages.append({
                        "role": "assistant",
                        "content": payload.get("text", ""),
                    })

                elif event_type == "tool_call_start":
                    call_id = payload.get("call_id", "")
                    pending_tool_calls[call_id] = {
                        "name": payload.get("tool", ""),
                        "input": payload.get("input", {}),
                    }

                elif event_type == "tool_call_result":
                    call_id = payload.get("call_id", "")
                    if call_id in pending_tool_calls:
                        output = payload.get("output", "")
                        error = payload.get("error")
                        messages.append({
                            "role": "tool",
                            "tool_call_id": call_id,
                            "content": error or output,
                        })
                        del pending_tool_calls[call_id]

        return messages if messages else None

    def resume_state(self) -> Optional[Dict]:
        """Load state for resume, preferring state.json, falling back to transcript.

        Returns state dict suitable for SafetyHarness.from_dict() + runner restart.
        Raises ResumeImpossible if no recovery data available.
        """
        # Try state.json first
        state = self.load()
        if state:
            # Reconstruct messages from transcript (state.json may have stale messages)
            messages = self.reconstruct_messages()
            if messages:
                state["messages"] = messages
            return state

        # Fall back to transcript-only reconstruction
        messages = self.reconstruct_messages()
        if not messages:
            raise ResumeImpossible(self.state_dir.name, "no state.json and no transcript")

        # Attempt to recover directive from registry
        from .thread_registry import ThreadRegistry
        thread_id = self.state_dir.name
        project_path = self.state_dir.parent.parent  # .ai/threads/{id} → .ai/threads → .ai → project
        try:
            registry = ThreadRegistry(project_path.parent)
            thread_info = registry.get_thread(thread_id)
            directive = thread_info.get("directive") if thread_info else None
        except Exception:
            directive = None

        if not directive:
            raise ResumeImpossible(thread_id, "directive unknown — no state.json and not in registry")

        return {
            "thread_id": thread_id,
            "directive": directive,
            "version": "1.0.0",
            "saved_at": datetime.now(timezone.utc).isoformat(),
            "messages": messages,
            "status": "suspended",
            "suspend_reason": "crash_recovery",
            "cost": {"turns": 0, "input_tokens": 0, "output_tokens": 0, "spend": 0.0},
            "limits": {},
        }
```

### Recovery Matrix

| Scenario                          | State Available                   | Recovery                                                         |
| --------------------------------- | --------------------------------- | ---------------------------------------------------------------- |
| Process crash with checkpoints    | `state.json` + `transcript.jsonl` | Load state, reconstruct messages, resume from last turn          |
| Process crash without checkpoints | `transcript.jsonl` only           | Reconstruct messages from transcript, resume with estimated cost |
| Process crash, no data            | Nothing                           | Mark as error, manual intervention                               |
| Suspension (limit/error)          | `state.json` (saved on suspend)   | Resume directly with optional limit bump                         |
| Cancellation                      | `state.json` (saved on cancel)    | State preserved, optionally resume later                         |

---

## 7. Context Limit and Continuation

### Problem

Long-running threads accumulate messages and tool results that approach the provider's context window limit. Without management, the LLM call fails with a context length error.

### Design: Thread Continuation (Not Compaction)

Instead of in-place compaction, the system spawns a **continuation thread** that chains from the current thread. The original thread ends with status `continued`, and the caller automatically resolves to the continuation thread's result.

**Key insight:** The thread is already running with a provider. The continuation hook uses the existing provider from `_thread_context` to generate the summary — no separate provider config needed.

### Continuation Flow

```
Thread A (context at 90%)
    │
    ├── 1. context_limit_reached event fires
    │
    ├── 2. Hook action: execute continuation_handoff tool
    │      └── Tool receives _thread_context: {provider, harness, transcript, thread_id, ...}
    │
    ├── 3. continuation_handoff.execute():
    │      │
    │      ├── Reconstruct conversation from transcript
    │      │
    │      ├── Run summary using existing provider (async)
    │      │   └── Produces: {summary: "..."}
    │      │
    │      ├── Extract recent turns (within recent_turns_tokens budget)
    │      │
    │      ├── Spawn Thread B with messages:
    │      │   [0] user: [Continuation from Thread A]
    │      │            Summary of prior work: {summary text}
    │      │   [1+] <recent messages from A>
    │      │
    │      └── Return {success: true, continuation_thread_id: "B", ...}
    │
    ├── 4. Thread A finalized:
    │      status = "continued"
    │      continuation_thread_id = "B"
    │
    └── 5. Caller's wait_threads resolves to Thread B's result
```

### Thread Chaining Structure

```
Thread A (continued) ←── Thread B (continued) ←── Thread C (active/completed)
    │                        │                        │
    └── transcript           └── transcript           └── transcript
    └── continuation_thread_id = B
                             └── continuation_of = A
                             └── continuation_thread_id = C
                                                      └── continuation_of = B
```

No maximum chain length — threads can continue indefinitely.

### Limit Detection

The runner calculates `usage_ratio` each turn:

```python
# In runner.py — context limit detection

def _check_context_limit(
    messages: List[Dict],
    provider: "ProviderAdapter",
    harness: "SafetyHarness",
    project_path: Path,
) -> Optional[Dict]:
    """Check if context window is approaching capacity.

    Returns event dict if threshold crossed, else None.
    Uses hysteresis: only fires when ratio crosses threshold, not every turn.
    """
    tokens_used = _estimate_message_tokens(messages, provider.model)
    context_limit = provider.config.get("context_window", 200000)
    usage_ratio = tokens_used / context_limit if context_limit > 0 else 0.0

    from .loaders import coordination_loader
    config = coordination_loader.load(project_path)
    threshold = config.get("continuation", {}).get("trigger_threshold", 0.9)
    last_ratio = getattr(harness, "_last_usage_ratio", 0.0)
    harness._last_usage_ratio = usage_ratio

    if usage_ratio >= threshold and last_ratio < threshold:
        return {
            "usage_ratio": usage_ratio,
            "tokens_used": tokens_used,
            "tokens_limit": context_limit,
        }

    return None


def _estimate_message_tokens(messages: List[Dict], model: str) -> int:
    """Rough token estimate: ~4 chars per token for English text."""
    total_chars = sum(len(m.get("content", "")) for m in messages)
    return total_chars // 4
```

### Hook Flow

When `context_limit_reached` fires, hooks run in layer order:

1. **Layer 1 (directive)** — User-defined hooks in the directive run first
2. **Layer 2 (builtin)** — `default_continuation` hook runs, executes `continuation_handoff` tool
3. **Layer 3 (infra)** — Infrastructure hooks (if any)

```yaml
# In hook_conditions.yaml (builtin, layer 2):
- id: "default_continuation"
  event: "context_limit_reached"
  layer: 2
  action:
    primary: "execute"
    item_type: "tool"
    item_id: "rye/agent/threads/internal/continuation_handoff"
    params:
      usage_ratio: "${event.usage_ratio}"
      tokens_used: "${event.tokens_used}"
      tokens_limit: "${event.tokens_limit}"
```

### continuation_handoff.py

```python
# internal/continuation_handoff.py
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Handle context limit by spawning continuation thread"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "usage_ratio": {"type": "number"},
        "tokens_used": {"type": "integer"},
        "tokens_limit": {"type": "integer"},
    },
    "required": ["usage_ratio"],
}

async def execute(params: Dict, project_path: str) -> Dict:
    """Spawn continuation thread when context limit is reached.

    Uses the existing provider from _thread_context to generate summary.
    Reconstructs conversation from transcript, extracts recent turns,
    and spawns a new thread with summary + recent messages.
    """
    from ..persistence.thread_registry import ThreadRegistry
    from ..persistence.state_store import StateStore
    from ..loaders import coordination_loader

    ctx = params.get("_thread_context", {})
    provider = ctx.get("provider")
    harness = ctx.get("harness")
    thread_id = ctx.get("thread_id")

    if not provider or not harness or not thread_id:
        return {
            "success": False,
            "error": "Missing thread context — cannot continue",
            "action": "fail",
        }

    proj_path = Path(project_path)
    config = coordination_loader.load(proj_path)
    cont_config = config.get("continuation", {})
    summary_max_tokens = cont_config.get("summary_max_tokens", 2000)
    recent_turns_tokens = cont_config.get("recent_turns_tokens", 10000)

    # 1. Reconstruct messages from transcript
    state_store = StateStore(proj_path, thread_id)
    messages = state_store.reconstruct_messages()

    if not messages:
        return {
            "success": False,
            "error": "Cannot reconstruct conversation from transcript",
            "action": "fail",
        }

    # 2. Generate summary using existing provider (async)
    summary = await _generate_summary(
        provider=provider,
        messages=messages,
        max_tokens=summary_max_tokens,
    )

    # 3. Extract recent turns within token budget
    recent_messages = _extract_recent_messages(messages, recent_turns_tokens)

    # 4. Build chain info
    registry = ThreadRegistry(proj_path)
    thread_info = registry.get_thread(thread_id)
    chain_root = thread_info.get("chain_root_id", thread_id) if thread_info else thread_id

    # 5. Register continuation thread
    import uuid
    continuation_id = f"thread-{uuid.uuid4().hex[:12]}"

    registry.register(continuation_id, harness.directive_name, parent_id=None)
    registry.set_chain_info(continuation_id, chain_root_id=chain_root,
                            continuation_of=thread_id)

    # 6. Update current thread
    registry.set_continuation(thread_id, continuation_id)

    # 7. Build continuation context
    chain_msg = f"""[Continuation from Thread {thread_id}]

Thread Chain: {chain_root} → ... → {thread_id} → (this thread)

Summary of prior work:
{summary}"""

    return {
        "success": True,
        "action": "continue",
        "continuation_thread_id": continuation_id,
        "continuation_messages": [{"role": "user", "content": chain_msg}] + recent_messages,
        "summary_generated": bool(summary),
        "recent_messages_count": len(recent_messages),
    }


async def _generate_summary(provider, messages: List[Dict], max_tokens: int) -> str:
    """Use existing provider to generate conversation summary."""
    summary_prompt = f"""Summarize the conversation so far in under {max_tokens} tokens.

Include:
- Key decisions made
- Work completed
- Important context for continuation
- Any pending tasks

Be concise but comprehensive."""

    try:
        summary_messages = messages + [{"role": "user", "content": summary_prompt}]
        response = await provider.create_completion(summary_messages, [])
        return response.get("text", "")
    except Exception:
        return ""


def _extract_recent_messages(messages: List[Dict], token_budget: int) -> List[Dict]:
    """Extract recent messages within token budget."""
    recent = []
    tokens = 0

    for msg in reversed(messages):
        msg_tokens = len(msg.get("content", "")) // 4
        if tokens + msg_tokens > token_budget:
            break
        recent.insert(0, msg)
        tokens += msg_tokens

    return recent
```

### Auto-Resolve Chain

When `wait_threads` is called on a thread that has been continued, the orchestrator follows the chain:

```python
# In orchestrator.py — chain resolution

def resolve_thread_chain(thread_id: str, project_path: Path) -> str:
    """Follow continuation chain to terminal thread.

    Returns the thread_id of the terminal thread (completed/error/running).
    Raises ChainResolutionError on cycles.
    """
    from .persistence.thread_registry import ThreadRegistry
    registry = ThreadRegistry(project_path)

    current = thread_id
    visited = set()

    while True:
        if current in visited:
            raise ChainResolutionError(thread_id, f"Cycle at {current}")
        visited.add(current)

        thread = registry.get_thread(current)
        if not thread:
            raise ThreadNotFound(current, "in continuation chain")

        status = thread.get("status")
        if status != "continued":
            return current

        continuation_id = thread.get("continuation_thread_id")
        if not continuation_id:
            return current
        current = continuation_id


async def wait_threads(thread_ids: List[str], timeout: float,
                       project_path: Path, ...) -> Dict:
    """Wait for threads, resolving continuation chains."""
    resolved_ids = [resolve_thread_chain(tid, project_path) for tid in thread_ids]
    # ... continue with existing wait logic using resolved_ids
```

### Runner Integration

```python
# In runner._run_batch / _run_streaming — after post-turn hooks:

# Context limit check
limit_info = _check_context_limit(messages, provider, harness, project_path)
if limit_info:
    thread_ctx = {"provider": provider, "harness": harness, "thread_id": thread_id}
    hook_result = await harness.run_hooks("context_limit_reached", limit_info,
                                          dispatcher, thread_ctx)
    if hook_result and hook_result.get("action") == "continue":
        continuation_id = hook_result.get("continuation_thread_id")
        continuation_messages = hook_result.get("continuation_messages", [])
        emitter.emit(thread_id, "thread_continued", {
            "continuation_thread_id": continuation_id,
            "tokens_used": limit_info["tokens_used"],
        }, transcript)
        # Spawn the continuation thread with pre-built messages
        await _spawn_continuation_task(continuation_id, continuation_messages,
                                       harness, provider, dispatcher, emitter,
                                       project_path)
        return _finalize(thread_id, cost, {
            "success": True,
            "status": "continued",
            "continuation_thread_id": continuation_id,
        }, emitter, transcript)
```

### Events

New event type in `events.yaml`:

```yaml
thread_continued:
  category: lifecycle
  criticality: critical
  description: "Thread continued to new thread due to context limit"
  payload_schema:
    type: object
    required: [continuation_thread_id]
    properties:
      continuation_thread_id: { type: string }
      tokens_used: { type: integer }
      summary_generated: { type: boolean }
```

### Chain Search Tool

The `thread_chain_search` tool allows searching across all transcripts in a continuation chain:

```python
# internal/thread_chain_search.py
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Search across all threads in a continuation chain"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "thread_id": {"type": "string", "description": "Any thread in the chain"},
        "query": {"type": "string", "description": "Search pattern (regex or text)"},
        "search_type": {"type": "string", "enum": ["regex", "text"], "default": "text"},
        "include_events": {
            "type": "array",
            "items": {"type": "string"},
            "default": ["cognition_in", "cognition_out", "tool_call_start", "tool_call_result"],
            "description": "Event types to search",
        },
        "max_results": {"type": "integer", "default": 50},
    },
    "required": ["thread_id", "query"],
}

def execute(params: Dict, project_path: str) -> Dict:
    """Search across all threads in a continuation chain.

    Collects the full chain from root to current, then searches
    each thread's transcript for the query.
    """
    from ..persistence.thread_registry import ThreadRegistry
    import json
    import re

    thread_id = params["thread_id"]
    query = params["query"]
    search_type = params.get("search_type", "text")
    include_events = set(params.get("include_events", [
        "cognition_in", "cognition_out", "tool_call_start", "tool_call_result"
    ]))
    max_results = params.get("max_results", 50)

    proj_path = Path(project_path)
    registry = ThreadRegistry(proj_path)

    # Get the full chain
    chain = registry.get_chain(thread_id)
    if not chain:
        return {"success": False, "error": f"No chain found for thread {thread_id}"}

    results = []
    pattern = re.compile(query, re.IGNORECASE) if search_type == "regex" else None

    for thread in chain:
        tid = thread["thread_id"]
        transcript_path = proj_path / ".ai" / "threads" / tid / "transcript.jsonl"

        if not transcript_path.exists():
            continue

        with open(transcript_path) as f:
            for line_no, line in enumerate(f, 1):
                line = line.strip()
                if not line:
                    continue
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue

                event_type = event.get("event_type", "")
                if event_type not in include_events:
                    continue

                payload_str = json.dumps(event.get("payload", {}))

                if search_type == "regex":
                    matches = pattern.findall(payload_str)
                else:
                    matches = [query] if query.lower() in payload_str.lower() else []

                if matches:
                    results.append({
                        "thread_id": tid,
                        "event_type": event_type,
                        "line_no": line_no,
                        "snippet": payload_str[:500],
                        "matches": matches[:5],
                    })

                    if len(results) >= max_results:
                        return {
                            "success": True,
                            "chain_length": len(chain),
                            "results": results,
                            "truncated": True,
                        }

    return {
        "success": True,
        "chain_length": len(chain),
        "chain_threads": [t["thread_id"] for t in chain],
        "results": results,
        "truncated": False,
    }
```

**LLM usage example:**

```
// Search for earlier decisions about database schema
thread_chain_search(
    thread_id="thread-abc123",
    query="database schema decision",
    search_type="text"
)
```

### Config (in coordination.yaml)

```yaml
continuation:
  trigger_threshold: 0.9 # Fire at 90% of context limit
  summary_max_tokens: 2000 # Max tokens for summary text
  recent_turns_tokens: 10000 # Token budget for carried-over messages
```

---

## 8. Extended Configuration

### 8.1 coordination.yaml

New config file for continuation, orphan detection, and wait timeouts:

```yaml
# config/coordination.yaml
schema_version: "1.0.0"

coordination:
  # Wait configuration
  wait_threads:
    default_timeout: 600 # seconds
    max_timeout: 3600 # 1 hour hard limit

  # Context limit and continuation
  continuation:
    trigger_threshold: 0.9 # Fire at 90% of context limit
    summary_max_tokens: 2000 # Max tokens for summary text
    recent_turns_tokens: 10000 # Token budget for carried-over messages

  # Orphan detection
  orphan_detection:
    enabled: true
    scan_on_startup: true
    stale_threshold_minutes: 60 # Thread running > 60 min with no cost update
```

### 8.2 streaming.yaml

New config file for streaming pipeline:

```yaml
# config/streaming.yaml
schema_version: "1.0.0"

streaming:
  # Tool batching
  batch:
    size_threshold: 5 # Execute when N tools ready
    max_pending: 50 # Hard cap on pending tools

  # Parser limits
  parser:
    max_tool_input_size: 1048576 # 1MB per tool input JSON
    max_text_buffer: 10485760 # 10MB accumulated text

  # Error handling
  on_interruption:
    emit_partial_cognition: true
    preserve_completed_tools: true
```

### 8.3 Loaders for New Configs

```python
# loaders/coordination_loader.py
from .config_loader import ConfigLoader
from pathlib import Path
from typing import Dict, Any, Optional

class CoordinationLoader(ConfigLoader):
    def __init__(self):
        super().__init__("coordination.yaml")

    def get_wait_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("coordination", {}).get("wait_threads", {})

    def get_continuation_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("coordination", {}).get("continuation", {})

    def get_orphan_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("coordination", {}).get("orphan_detection", {})

_loader: Optional[CoordinationLoader] = None

def get_coordination_loader() -> CoordinationLoader:
    global _loader
    if _loader is None:
        _loader = CoordinationLoader()
    return _loader

def load(project_path: Path) -> Dict[str, Any]:
    return get_coordination_loader().load(project_path)
```

```python
# loaders/streaming_loader.py
from .config_loader import ConfigLoader
from pathlib import Path
from typing import Dict, Any, Optional

class StreamingLoader(ConfigLoader):
    def __init__(self):
        super().__init__("streaming.yaml")

    def get_batch_config(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("streaming", {}).get("batch", {})

    def get_parser_limits(self, project_path: Path) -> Dict:
        config = self.load(project_path)
        return config.get("streaming", {}).get("parser", {})

_loader: Optional[StreamingLoader] = None

def get_streaming_loader() -> StreamingLoader:
    global _loader
    if _loader is None:
        _loader = StreamingLoader()
    return _loader

def load(project_path: Path) -> Dict[str, Any]:
    return get_streaming_loader().load(project_path)
```

---

## 9. Testing Strategy

### Part 2-Specific Tests

| Module                       | Key Test Cases                                                                                                                                                                                                                                                                                                                                                  |
| ---------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `errors`                     | All typed exceptions instantiate with correct fields, `str()` produces useful messages                                                                                                                                                                                                                                                                          |
| `thread_registry` (extended) | `get_children` recursive CTE, `get_ancestors`, `aggregate_cost` across subtree, `find_orphans` with dead PID, continuation chain columns and queries, `get_chain` follows full chain, cycle detection                                                                                                                                                           |
| `budgets` (extended)         | Concurrent `reserve()` with IMMEDIATE isolation (two threads, same parent), `reserve` raises `InsufficientBudget` (not returns False), `increment_actual` raises `BudgetOverspend` on overspend (no clamping), `BudgetLedgerLocked` on contention, `release` frees reservation, `get_tree_spend` recursive, `can_spawn` raises `BudgetNotRegistered` if missing |
| `streaming_tool_parser`      | `feed_event` with Anthropic content_block_delta sequence, index-based tool attribution (not heuristic), `ToolInputParseError` on malformed JSON, size limit enforcement, text between tools, `tool_complete` yields correct input, `stream_end` with usage                                                                                                      |
| `runner` streaming mode      | Tool executes during stream, batch dispatch at size threshold, partial cognition on error, cognition_out always emitted, interleaved transcript events                                                                                                                                                                                                          |
| `state_store` (extended)     | `reconstruct_messages` raises `TranscriptCorrupt` on bad JSON (no silent skip), `resume_state` prefers state.json, falls back to registry for directive, raises `ResumeImpossible` if unrecoverable                                                                                                                                                             |
| `orphan_detector`            | Scan returns `{confirmed, uncertain}` — never marks uncertain PIDs as orphaned, recover marks confirmed as suspended, no state → marks error                                                                                                                                                                                                                    |
| Context continuation         | Threshold from config (not hardcoded), hysteresis fires only on threshold crossing, continuation thread spawned with summary + recent messages, chain resolution follows to terminal thread                                                                                                                                                                     |
| `continuation_handoff`       | Uses existing provider from `_thread_context` (async call), reconstructs messages from transcript, generates summary via provider, extracts recent turns within token budget, registers continuation thread, updates registry with chain info                                                                                                                   |
| `thread_chain_search`        | Returns chain from registry, searches all transcripts in chain, regex and text search modes, respects max_results limit, includes event type filtering, returns thread_id + line_no for each match                                                                                                                                                              |
| `checkpoint`                 | `CheckpointFailed` raised on write error (default), `on_failure: "warn"` opt-in for lenient mode                                                                                                                                                                                                                                                                |
| `coordination_loader`        | Load coordination.yaml, project override, get continuation config                                                                                                                                                                                                                                                                                               |
| `streaming_loader`           | Load streaming.yaml, get parser limits, batch config                                                                                                                                                                                                                                                                                                            |

### Integration Tests

| Scenario               | What It Validates                                                                                                                                                                  |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Crash recovery         | Start thread, kill process, orphan scan, resume from state.json + transcript                                                                                                       |
| Budget exhaustion      | Parent $1.00, spawn two $0.60 children, second reservation raises InsufficientBudget, hook fires                                                                                   |
| Streaming end-to-end   | Provider streams SSE → parser yields tools → tools execute during stream → transcript interleaved                                                                                  |
| Fail-loud verification | `BudgetNotRegistered` on unregistered thread, `ThreadWaitTimeout` as exception (not data), `TranscriptCorrupt` on bad lines, `ToolInputParseError` on bad JSON                     |
| Continuation chain     | Context limit reached → thread spawns continuation → original thread status "continued" → `wait_threads` resolves chain → continuation runs with summary + recent messages         |
| Chain search           | 3-thread continuation chain → `thread_chain_search` finds matches across all 3 transcripts → regex mode matches patterns → max_results truncates → chain_threads returned in order |
| Checkpoints            | `CheckpointFailed` raised by default, `on_failure: "warn"` opt-in works                                                                                                            |

---

## 10. Migration Checklist

### Phase 0: Error Types (Before all Part 2 phases)

- [ ] Extend `errors.py` — add remaining typed exceptions (`BudgetNotRegistered`, `InsufficientBudget`, `BudgetOverspend`, `BudgetLedgerLocked`, `ThreadWaitTimeout`, `CompletionEventMissing`, `ToolInputParseError`, `PidCheckUnknown`, `ContinuationFailed`, `ChainResolutionError`)
- [ ] Add thread system error patterns to `error_classification.yaml` (BudgetLedgerLocked as transient, InsufficientBudget/CheckpointFailed/ContinuationFailed as permanent)

### Phase A: Extended Persistence (After Part 1 Phases 1–5)

- [ ] Extend `persistence/thread_registry.py` — add cost columns, PID column, continuation chain columns (`continuation_of`, `continuation_thread_id`, `chain_root_id`), recursive CTE queries (`get_children`, `get_ancestors`, `aggregate_cost`), orphan detection with tri-state PID check, `set_continuation()`, `set_chain_info()`, `get_chain()`
- [ ] Extend `persistence/budgets.py` — IMMEDIATE isolation, `BudgetLedgerLocked` on contention, `increment_actual` with overspend check (raises `BudgetOverspend`), `can_spawn` (raises `BudgetNotRegistered`), `get_tree_spend` recursive, `release` with status, `ON DELETE RESTRICT` (not CASCADE), `reserve()` raises instead of returning False
- [ ] Create `internal/orphan_detector.py` — scan returns `{confirmed, uncertain}`, recover marks confirmed as suspended

### Phase B: Streaming Pipeline (After Part 1 Phase 5)

- [ ] Rewrite `events/streaming_tool_parser.py` — Anthropic `feed_event` format, index-based tool attribution, size limit enforcement, raises `ToolInputParseError` on malformed JSON
- [ ] Extend `adapters/provider_adapter.py` — add `create_streaming_completion()`, `supports_streaming`
- [ ] Extend `runner.py` — add `_run_streaming()` path, partial cognition on error, batch dispatch
- [ ] Create `config/streaming.yaml`
- [ ] Create `loaders/streaming_loader.py`

### Phase C: Recovery and Continuation (After Phase B)

- [ ] Extend `runner.py` — wire `_save_checkpoint` (raises `CheckpointFailed` by default), `_check_context_limit` with config-driven threshold
- [ ] Extend `persistence/state_store.py` — `reconstruct_messages()` raises `TranscriptCorrupt`, `resume_state()` raises `ResumeImpossible`
- [ ] Extend `safety_harness.py` — hierarchical budget check in `check_limits()` (missing ledger is a hard error)
- [ ] Wire orphan scan on startup with tri-state PID check (controlled by `coordination.yaml` config)
- [ ] Create `internal/continuation_handoff.py` — async, uses `_thread_context` provider, reconstructs from transcript, generates summary, spawns continuation
- [ ] Create `internal/thread_chain_search.py` — searches all transcripts in continuation chain, regex and text modes
- [ ] Extend `orchestrator.py` — add `resolve_thread_chain()` for auto-resolving continuation chains in `wait_threads`
- [ ] Create `config/coordination.yaml`
- [ ] Create `loaders/coordination_loader.py`
- [ ] Extend `internal/budget_ops.py` — add `can_spawn`, `increment_actual`, `get_tree_spend` operations

### Phase D: Verification

- [ ] Concurrent `reserve()` — two threads, same parent: one raises `InsufficientBudget`, SQLite IMMEDIATE serializes correctly
- [ ] `StreamingToolParser` — Anthropic SSE sequence: index-based attribution, `ToolInputParseError` on bad JSON, size limits enforced
- [ ] Crash recovery — kill mid-turn, orphan scan returns `{confirmed, uncertain}`, resume from checkpoint, `ResumeImpossible` when unrecoverable
- [ ] Context continuation — threshold from config, hysteresis fires only on threshold crossing, continuation thread spawned, chain resolution works
- [ ] Chain search — 3-thread chain, search finds results in all threads, regex mode works, max_results truncates, event type filtering works
- [ ] Checkpoints — `CheckpointFailed` raised by default, `on_failure: "warn"` opt-in works
- [ ] All new configs load with project overrides (extends + merge-by-id)
- [ ] New loaders follow Part 1 pattern (singleton, cache per project, `clear_cache()`)
- [ ] New internal tools follow standard contract (`__version__`, `__category__`, signable)
- [ ] Every `Optional[X]` return eliminated — typed exceptions replace silent None/False returns throughout

---

## Appendix: Changes from Previous Version

| Item                       | Previous Design                                               | Current Design                                          | Reason                                                                                                                           |
| -------------------------- | ------------------------------------------------------------- | ------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| Thread-to-thread artifacts | `persistence/artifacts.py` + `internal/artifact_ops.py`       | **Removed**                                             | Orchestrator LLM passes data between waves via inputs. Separate artifact store adds complexity for a problem that doesn't exist. |
| Wave-based orchestration   | `execute_wave` in orchestrator                                | **Deferred**                                            | Convenience over manual spawn+wait. LLM can already do this. Implement after basic thread spawning is proven.                    |
| Cross-process polling      | Registry polling for `spawn_mode='cross_process'`             | **Deferred** (columns added)                            | Single-process is the only runtime. Schema supports it, implementation deferred.                                                 |
| `continuation_handoff`     | Sync `execute()` calling async `provider.create_completion()` | **Fixed**: `async def execute()`                        | Provider methods are async; handoff must be async too.                                                                           |
| `_spawn_continuation()`    | Had `# TODO: Actually spawn the thread`                       | **Fixed**: Returns continuation messages, runner spawns | Handoff returns pre-built messages, runner handles actual spawn.                                                                 |
| `chain_root_id` tracking   | Separate column, set at spawn time                            | **Kept** but derivable                                  | Could be computed via recursive query; column is a denormalization for fast lookups.                                             |
| Context management         | In-place compaction with summary + prune                      | Thread continuation chain                               | Continuation preserves full transcript history, no data loss                                                                     |
