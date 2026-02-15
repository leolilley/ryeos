# rye:signed:2026-02-14T00:28:39Z:9c3668a8d59a0815cf82e5476f02f85d8ca5cdd6c4aea52b18b082e24666cbfa:k65MwQ0_NRwqiLFzgnB6LBL3S75sxjc4WOKDwEc3QeLk2exC92wXPeniZbhMaffV-K5WzbeP_BZ2BUV4iBFEAQ==:440443d0858f0199
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "Budget ledger for thread cost tracking"

import sqlite3
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, Optional

from rye.constants import AI_DIR

DB_FILE = "budget_ledger.db"


class BudgetLedger:
    """SQLite-backed budget tracking.

    DB location: {project_path}/.ai/threads/budget_ledger.db
    Schema loaded from config/budget_ledger_schema.yaml.
    """

    def __init__(self, project_path: Path):
        self.db_path = project_path / AI_DIR / "threads" / DB_FILE
        self._ensure_schema()

    def _ensure_schema(self):
        """Create table if not exists."""
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        with sqlite3.connect(self.db_path) as conn:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS budget_ledger (
                    thread_id TEXT PRIMARY KEY,
                    parent_thread_id TEXT,
                    reserved_spend REAL DEFAULT 0.0,
                    actual_spend REAL DEFAULT 0.0,
                    max_spend REAL,
                    status TEXT DEFAULT 'active',
                    created_at TEXT,
                    updated_at TEXT
                )
            """)
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_budget_parent ON budget_ledger(parent_thread_id)"
            )
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_budget_status ON budget_ledger(status)"
            )
            conn.commit()

    def reserve(
        self, thread_id: str, amount: float, parent_thread_id: str = None
    ) -> bool:
        """Reserve budget. Returns False if parent has insufficient remaining."""
        now = datetime.utcnow().isoformat()
        with sqlite3.connect(self.db_path) as conn:
            if parent_thread_id:
                remaining = self.get_remaining(parent_thread_id)
                if remaining is not None and remaining < amount:
                    return False

            conn.execute(
                """
                INSERT OR REPLACE INTO budget_ledger
                (thread_id, parent_thread_id, reserved_spend, max_spend, status, created_at, updated_at)
                VALUES (?, ?, ?, ?, 'active', ?, ?)
            """,
                (thread_id, parent_thread_id, amount, amount, now, now),
            )
            conn.commit()
        return True

    def report_actual(self, thread_id: str, amount: float) -> None:
        """Report actual spend by adding to existing actual_spend.

        Adds (not overwrites) so cascaded child spend is preserved.
        Total is clamped to reserved amount.
        """
        now = datetime.utcnow().isoformat()
        with sqlite3.connect(self.db_path) as conn:
            cursor = conn.execute(
                "SELECT reserved_spend, actual_spend FROM budget_ledger WHERE thread_id = ?",
                (thread_id,),
            )
            row = cursor.fetchone()
            if row:
                new_total = min(row[1] + amount, row[0])
                conn.execute(
                    """
                    UPDATE budget_ledger
                    SET actual_spend = ?, updated_at = ?
                    WHERE thread_id = ?
                """,
                    (new_total, now, thread_id),
                )
                conn.commit()

    def release(self, thread_id: str) -> None:
        """Release remaining reservation on thread completion/error."""
        now = datetime.utcnow().isoformat()
        with sqlite3.connect(self.db_path) as conn:
            conn.execute(
                """
                UPDATE budget_ledger
                SET status = 'released', updated_at = ?
                WHERE thread_id = ?
            """,
                (now, thread_id),
            )
            conn.commit()

    def get_remaining(self, thread_id: str) -> Optional[float]:
        """Get remaining budget (reserved - actual)."""
        with sqlite3.connect(self.db_path) as conn:
            cursor = conn.execute(
                """
                SELECT reserved_spend, actual_spend
                FROM budget_ledger
                WHERE thread_id = ? AND status = 'active'
            """,
                (thread_id,),
            )
            row = cursor.fetchone()
            if row:
                return row[0] - row[1]
        return None

    def cascade_spend(self, child_thread_id: str, parent_thread_id: str, amount: float) -> None:
        """Add child's actual spend to parent's actual_spend."""
        now = datetime.utcnow().isoformat()
        with sqlite3.connect(self.db_path) as conn:
            conn.execute(
                """
                UPDATE budget_ledger
                SET actual_spend = actual_spend + ?, updated_at = ?
                WHERE thread_id = ? AND status = 'active'
            """,
                (amount, now, parent_thread_id),
            )
            conn.commit()

    def get_status(self, thread_id: str) -> Optional[Dict[str, Any]]:
        """Get full budget status for a thread."""
        with sqlite3.connect(self.db_path) as conn:
            conn.row_factory = sqlite3.Row
            cursor = conn.execute(
                "SELECT * FROM budget_ledger WHERE thread_id = ?", (thread_id,)
            )
            row = cursor.fetchone()
            if row:
                return dict(row)
        return None


_ledger_cache: Dict[str, BudgetLedger] = {}


def get_ledger(project_path: Path) -> BudgetLedger:
    key = str(project_path)
    if key not in _ledger_cache:
        _ledger_cache[key] = BudgetLedger(project_path)
    return _ledger_cache[key]
