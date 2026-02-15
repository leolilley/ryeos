# rye:signed:2026-02-14T00:28:39Z:7e7b5cc34e99f632ef7a8a9a26185e832a24aaef302aae218817d76743bc5760:9hSxrBnXzH9EW6b13hE7yl_HONnhuiK8H1shLloPaDJLmN7sCMcI3oqez45tGKLB9xSLXQOIdtayWHxpArxTBQ==:440443d0858f0199
__version__ = "1.0.0"
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

    DB location: {project_path}/.ai/threads/registry.db
    """

    def __init__(self, project_path: Path):
        self.db_path = project_path / AI_DIR / "threads" / DB_FILE
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
            conn.commit()

    def update_status(self, thread_id: str, status: str) -> None:
        """Update thread status."""
        now = datetime.utcnow().isoformat()
        with sqlite3.connect(self.db_path) as conn:
            extra = ""
            params = [status, now, thread_id]
            if status in ("completed", "error", "cancelled"):
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


_registry_cache: Dict[str, ThreadRegistry] = {}


def get_registry(project_path: Path) -> ThreadRegistry:
    key = str(project_path)
    if key not in _registry_cache:
        _registry_cache[key] = ThreadRegistry(project_path)
    return _registry_cache[key]
