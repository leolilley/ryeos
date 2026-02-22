# rye:signed:2026-02-22T09:00:56Z:5be2282842d6e826076874cec73b6e751fcf7dbf4402931bdb9d811fc8bdc313:hWicNSwKDH2Y3UIUaMk82xX-UHWdaKk8JVbCVP9ReoFb43SzMkxCPBgZcVlonGlabK1s1Ic8vjGdGFr5-ldKAw==:9fbfabe975fa5a7f
__version__ = "1.2.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "Thread registry for tracking active threads"

import sqlite3
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import AI_DIR

DB_FILE = "registry.db"


class ThreadRegistry:
    """Track thread lifecycle in SQLite.

    DB location: {project_path}/.ai/agent/threads/registry.db
    """

    def __init__(self, project_path: Path):
        self.db_path = project_path / AI_DIR / "agent" / "threads" / DB_FILE
        self._ensure_schema()

    def _ensure_schema(self):
        """Create table if not exists."""
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        with sqlite3.connect(self.db_path) as conn:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS threads (
                    thread_id TEXT PRIMARY KEY,
                    directive TEXT NOT NULL,
                    parent_id TEXT,
                    status TEXT DEFAULT 'created',
                    created_at TEXT,
                    updated_at TEXT,
                    completed_at TEXT,
                    result TEXT
                )
            """)
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_threads_parent ON threads(parent_id)"
            )
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_threads_status ON threads(status)"
            )
            conn.commit()

            # Part 2: migrate schema — add columns if missing
            existing = {row[1] for row in conn.execute("PRAGMA table_info(threads)").fetchall()}
            migrations = [
                ("turns", "INTEGER DEFAULT 0"),
                ("input_tokens", "INTEGER DEFAULT 0"),
                ("output_tokens", "INTEGER DEFAULT 0"),
                ("spend", "REAL DEFAULT 0.0"),
                ("spawn_count", "INTEGER DEFAULT 0"),
                ("pid", "INTEGER"),
                ("model", "TEXT"),
                ("continuation_of", "TEXT"),
                ("continuation_thread_id", "TEXT"),
                ("chain_root_id", "TEXT"),
            ]
            for col_name, col_type in migrations:
                if col_name not in existing:
                    conn.execute(f"ALTER TABLE threads ADD COLUMN {col_name} {col_type}")
            conn.commit()

    def register(self, thread_id: str, directive: str, parent_id: str = None) -> None:
        """Register a new thread."""
        now = datetime.utcnow().isoformat()
        with sqlite3.connect(self.db_path) as conn:
            conn.execute(
                """
                INSERT INTO threads (thread_id, directive, parent_id, status, created_at, updated_at)
                VALUES (?, ?, ?, 'created', ?, ?)
            """,
                (thread_id, directive, parent_id, now, now),
            )
            import os
            conn.execute(
                "UPDATE threads SET pid = ? WHERE thread_id = ?",
                (os.getpid(), thread_id),
            )
            conn.commit()

    def update_status(self, thread_id: str, status: str) -> None:
        """Update thread status."""
        now = datetime.utcnow().isoformat()
        with sqlite3.connect(self.db_path) as conn:
            extra = ""
            params = [status, now, thread_id]
            if status in ("completed", "error", "cancelled", "continued"):
                extra = ", completed_at = ?"
                params.insert(2, now)
            conn.execute(
                f"""
                UPDATE threads SET status = ?, updated_at = ?{extra} WHERE thread_id = ?
            """,
                params,
            )
            conn.commit()

    def get_status(self, thread_id: str) -> Optional[str]:
        """Get thread status."""
        with sqlite3.connect(self.db_path) as conn:
            cursor = conn.execute(
                "SELECT status FROM threads WHERE thread_id = ?", (thread_id,)
            )
            row = cursor.fetchone()
            return row[0] if row else None

    def list_active(self) -> List[Dict[str, Any]]:
        """List all active threads."""
        with sqlite3.connect(self.db_path) as conn:
            conn.row_factory = sqlite3.Row
            cursor = conn.execute("""
                SELECT * FROM threads
                WHERE status NOT IN ('completed', 'error', 'cancelled', 'released')
                ORDER BY created_at DESC
            """)
            return [dict(row) for row in cursor.fetchall()]

    def list_children(self, parent_id: str) -> List[Dict[str, Any]]:
        """List all children of a thread."""
        with sqlite3.connect(self.db_path) as conn:
            conn.row_factory = sqlite3.Row
            cursor = conn.execute(
                "SELECT * FROM threads WHERE parent_id = ? ORDER BY created_at",
                (parent_id,),
            )
            return [dict(row) for row in cursor.fetchall()]

    def get_thread(self, thread_id: str) -> Optional[Dict[str, Any]]:
        """Get full thread record."""
        with sqlite3.connect(self.db_path) as conn:
            conn.row_factory = sqlite3.Row
            cursor = conn.execute(
                "SELECT * FROM threads WHERE thread_id = ?", (thread_id,)
            )
            row = cursor.fetchone()
            return dict(row) if row else None

    def set_result(self, thread_id: str, result: Any) -> None:
        """Store thread result."""
        import json

        now = datetime.utcnow().isoformat()
        with sqlite3.connect(self.db_path) as conn:
            conn.execute(
                """
                UPDATE threads SET result = ?, updated_at = ? WHERE thread_id = ?
            """,
                (json.dumps(result, default=str), now, thread_id),
            )
            conn.commit()

    def update_cost_snapshot(self, thread_id: str, cost: Dict[str, Any]) -> None:
        """Update cost columns from runner's cost dict (called post-turn)."""
        now = datetime.utcnow().isoformat()
        with sqlite3.connect(self.db_path) as conn:
            conn.execute("""
                UPDATE threads SET
                    turns = ?, input_tokens = ?, output_tokens = ?,
                    spend = ?, updated_at = ?
                WHERE thread_id = ?
            """, (cost.get("turns", 0), cost.get("input_tokens", 0),
                  cost.get("output_tokens", 0), cost.get("spend", 0.0),
                  now, thread_id))
            conn.commit()

    def set_continuation(self, thread_id: str, continuation_thread_id: str) -> None:
        """Mark thread as continued with forward pointer."""
        now = datetime.utcnow().isoformat()
        with sqlite3.connect(self.db_path) as conn:
            conn.execute("""
                UPDATE threads SET
                    continuation_thread_id = ?,
                    status = 'continued',
                    updated_at = ?
                WHERE thread_id = ?
            """, (continuation_thread_id, now, thread_id))
            conn.commit()

    def set_chain_info(self, thread_id: str, chain_root_id: str,
                       continuation_of: str) -> None:
        """Set chain metadata for a continuation thread."""
        now = datetime.utcnow().isoformat()
        with sqlite3.connect(self.db_path) as conn:
            conn.execute("""
                UPDATE threads SET
                    chain_root_id = ?,
                    continuation_of = ?,
                    updated_at = ?
                WHERE thread_id = ?
            """, (chain_root_id, continuation_of, now, thread_id))
            conn.commit()

    def get_chain(self, thread_id: str) -> List[Dict[str, Any]]:
        """Get the full continuation chain containing this thread.

        Walks backward to find root, then forward to build ordered chain.
        Returns list of thread dicts from root to terminal.
        """
        # Walk backward to find root
        root_id = thread_id
        visited = set()
        while True:
            if root_id in visited:
                break  # cycle
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
                break  # cycle
            visited.add(current)
            thread = self.get_thread(current)
            if not thread:
                break
            chain.append(thread)
            current = thread.get("continuation_thread_id")

        return chain


_registry_cache: Dict[str, ThreadRegistry] = {}


def get_registry(project_path: Path) -> ThreadRegistry:
    key = str(project_path)
    if key not in _registry_cache:
        _registry_cache[key] = ThreadRegistry(project_path)
    return _registry_cache[key]
