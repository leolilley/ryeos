# rye:signed:2026-02-26T05:02:40Z:da555d46a7568e3acdaee7e3de17362ec9ea0360125ac5dfc297c5e3fb08e774:ij6vY8YSXAcsCNriPgf433wZ2wszTuaASplTczYizejfPgaBbNhFVmQGCTxS-5LRIapsvbNgj0TuHN57dN1OCw==:4b987fd4e40303ac
__version__ = "1.1.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "Budget ledger for thread cost tracking"

import sqlite3
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, Optional

from rye.constants import AI_DIR
from ..errors import BudgetNotRegistered, InsufficientBudget, BudgetOverspend, BudgetLedgerLocked

DB_FILE = "budget_ledger.db"

TERMINAL_STATUSES = frozenset({"completed", "cancelled", "error"})


class BudgetLedger:
    """SQLite-backed hierarchical budget tracking.

    DB location: {project_path}/.ai/agent/threads/budget_ledger.db

    Key invariant: reserve() uses BEGIN IMMEDIATE to prevent concurrent
    over-reservation. Two threads trying to reserve from the same parent
    are serialized at the transaction level.
    """

    def __init__(self, project_path: Path):
        self.db_path = project_path / AI_DIR / "agent" / "threads" / DB_FILE
        self._ensure_schema()

    def _connect(self) -> sqlite3.Connection:
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(str(self.db_path), timeout=5.0)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA foreign_keys = ON")
        return conn

    def _ensure_schema(self):
        """Create table if not exists with WAL mode."""
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

    # --- Registration ---

    def register(self, thread_id: str, max_spend: Optional[float] = None,
                 parent_thread_id: Optional[str] = None) -> None:
        """Register a thread's budget. Called before runner.run().

        Root threads (no parent) trigger cleanup of all terminal entries
        from previous runs so the ledger doesn't accumulate stale data.
        """
        now = datetime.now(timezone.utc).isoformat()
        with self._connect() as conn:
            if not parent_thread_id:
                conn.execute(
                    "DELETE FROM budget_ledger WHERE status IN ('completed', 'error', 'cancelled')"
                )
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

            conn.execute("""
                INSERT INTO budget_ledger
                    (thread_id, parent_thread_id, reserved_spend, max_spend,
                     status, created_at, updated_at)
                VALUES (?, ?, ?, ?, 'active', ?, ?)
                ON CONFLICT(thread_id) DO UPDATE SET
                    reserved_spend = excluded.reserved_spend,
                    max_spend = excluded.max_spend,
                    status = 'active',
                    updated_at = excluded.updated_at
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

    def can_spawn(self, parent_thread_id: str, requested_budget: float) -> Dict:
        """Pre-check whether a spawn is affordable. Does not reserve.

        Returns {affordable: bool, remaining: float, requested: float}.
        Raises BudgetNotRegistered if parent has no ledger entry.
        """
        remaining = self.get_remaining(parent_thread_id)
        return {
            "affordable": remaining >= requested_budget,
            "remaining": remaining,
            "requested": requested_budget,
        }

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

    # --- Cascade (kept from v1.0.0) ---

    def cascade_spend(self, child_thread_id: str, parent_thread_id: str, amount: float) -> None:
        """Add child's actual spend to parent's actual_spend."""
        now = datetime.now(timezone.utc).isoformat()
        with self._connect() as conn:
            conn.execute("""
                UPDATE budget_ledger
                SET actual_spend = actual_spend + ?, updated_at = ?
                WHERE thread_id = ? AND status = 'active'
            """, (amount, now, parent_thread_id))

    def get_status(self, thread_id: str) -> Optional[Dict[str, Any]]:
        """Get full budget status for a thread."""
        with self._connect() as conn:
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
