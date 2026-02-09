# kiwi-mcp:validated:2026-01-26T03:02:16Z:b10c107ada970b76c8ff3d280e02bd64159acedebcc4852480a123c3235a1fa4
# .ai/tools/threads/thread_registry.py
__tool_type__ = "python"
__version__ = "1.0.0"
__executor_id__ = "python_runtime"
__category__ = "threads"

"""
Thread Registry: SQLite-based persistence for thread state and events.

A data-driven tool for tracking LLM agent threads, their status, events, and metadata.
Uses SQLite with WAL mode for concurrent access. Includes JSONL transcript writer
for human-readable logs.

This is a privileged harness tool - directives cannot call it directly;
only safety_harness can use it.
"""

import json
import sqlite3
from dataclasses import dataclass, asdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional, Dict, Any, List
import logging

logger = logging.getLogger(__name__)


@dataclass
class ThreadRecord:
    """Thread metadata record."""
    thread_id: str
    directive_id: str
    parent_thread_id: Optional[str]
    status: str  # running, paused, completed, error
    created_at: str
    updated_at: str
    permission_context_json: Optional[str] = None
    cost_budget_json: Optional[str] = None
    total_usage_json: Optional[str] = None


@dataclass
class ThreadEvent:
    """Thread event record (append-only audit log)."""
    thread_id: str
    ts: str
    event_type: str
    payload_json: Optional[str] = None


class ThreadRegistry:
    """
    SQLite-based thread registry for tracking thread state and events.
    
    Features:
    - WAL mode for concurrent access
    - Thread registration and status updates
    - Event logging (append-only)
    - Query operations (by status, directive, time range)
    - Automatic schema initialization
    """
    
    def __init__(self, db_path: Path):
        """
        Initialize thread registry.
        
        Args:
            db_path: Path to SQLite database file
        """
        self.db_path = Path(db_path)
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._init_schema()
    
    def _get_connection(self) -> sqlite3.Connection:
        """Get database connection with WAL mode enabled."""
        conn = sqlite3.connect(str(self.db_path), timeout=30.0)
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA foreign_keys=ON")
        conn.row_factory = sqlite3.Row
        return conn
    
    def _init_schema(self) -> None:
        """Initialize database schema if it doesn't exist."""
        conn = self._get_connection()
        try:
            # threads table
            conn.execute("""
                CREATE TABLE IF NOT EXISTS threads (
                    thread_id TEXT PRIMARY KEY,
                    directive_id TEXT NOT NULL,
                    parent_thread_id TEXT,
                    status TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    permission_context_json TEXT,
                    cost_budget_json TEXT,
                    total_usage_json TEXT,
                    FOREIGN KEY (parent_thread_id) REFERENCES threads(thread_id)
                )
            """)
            
            # thread_events table (append-only audit log)
            conn.execute("""
                CREATE TABLE IF NOT EXISTS thread_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    thread_id TEXT NOT NULL,
                    ts TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    payload_json TEXT,
                    FOREIGN KEY (thread_id) REFERENCES threads(thread_id)
                )
            """)
            
            # Indexes
            conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_events_thread 
                ON thread_events(thread_id, ts)
            """)
            
            conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_threads_directive 
                ON threads(directive_id, created_at)
            """)
            
            conn.commit()
            logger.debug(f"Thread registry schema initialized at {self.db_path}")
        finally:
            conn.close()
    
    def register(
        self,
        thread_id: str,
        directive_id: str,
        parent_thread_id: Optional[str] = None,
        permission_context: Optional[Dict[str, Any]] = None,
        cost_budget: Optional[Dict[str, Any]] = None,
    ) -> None:
        """
        Register a new thread.
        
        Args:
            thread_id: Unique thread identifier
            directive_id: Directive name that spawned this thread
            parent_thread_id: Optional parent thread ID (for nested threads)
            permission_context: Permission context JSON (capability tokens, etc.)
            cost_budget: Cost budget JSON (max_turns, max_tokens, max_usd, max_context)
        """
        now = datetime.now(timezone.utc).isoformat()
        
        conn = self._get_connection()
        try:
            conn.execute("""
                INSERT INTO threads (
                    thread_id, directive_id, parent_thread_id, status,
                    created_at, updated_at,
                    permission_context_json, cost_budget_json, total_usage_json
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            """, (
                thread_id,
                directive_id,
                parent_thread_id,
                "running",
                now,
                now,
                json.dumps(permission_context) if permission_context else None,
                json.dumps(cost_budget) if cost_budget else None,
                json.dumps({})  # Initialize empty usage
            ))
            conn.commit()
            logger.info(f"Registered thread {thread_id} (directive: {directive_id})")
        except sqlite3.IntegrityError as e:
            if "UNIQUE constraint failed" in str(e):
                raise ValueError(f"Thread {thread_id} already exists")
            raise
        finally:
            conn.close()
    
    def update_status(
        self,
        thread_id: str,
        status: str,
        metadata: Optional[Dict[str, Any]] = None,
    ) -> None:
        """
        Update thread status.
        
        Args:
            thread_id: Thread identifier
            status: New status (running, paused, completed, error)
            metadata: Optional metadata to update (usage, etc.)
        """
        now = datetime.now(timezone.utc).isoformat()
        
        conn = self._get_connection()
        try:
            # Update status and updated_at
            conn.execute("""
                UPDATE threads 
                SET status = ?, updated_at = ?
                WHERE thread_id = ?
            """, (status, now, thread_id))
            
            # Update metadata if provided
            if metadata:
                if "usage" in metadata:
                    conn.execute("""
                        UPDATE threads 
                        SET total_usage_json = ?
                        WHERE thread_id = ?
                    """, (json.dumps(metadata["usage"]), thread_id))
            
            conn.commit()
            logger.debug(f"Updated thread {thread_id} status to {status}")
        finally:
            conn.close()
    
    def get_status(self, thread_id: str) -> Optional[Dict[str, Any]]:
        """
        Get current status of a thread.
        
        Args:
            thread_id: Thread identifier
            
        Returns:
            Thread status dict or None if not found
        """
        conn = self._get_connection()
        try:
            row = conn.execute("""
                SELECT * FROM threads WHERE thread_id = ?
            """, (thread_id,)).fetchone()
            
            if not row:
                return None
            
            result = dict(row)
            # Parse JSON fields
            if result.get("permission_context_json"):
                result["permission_context"] = json.loads(result["permission_context_json"])
            if result.get("cost_budget_json"):
                result["cost_budget"] = json.loads(result["cost_budget_json"])
            if result.get("total_usage_json"):
                result["total_usage"] = json.loads(result["total_usage_json"])
            
            return result
        finally:
            conn.close()
    
    def query(
        self,
        directive_id: Optional[str] = None,
        status: Optional[str] = None,
        created_after: Optional[str] = None,
        created_before: Optional[str] = None,
        limit: int = 100,
        order_by: str = "created_at DESC",
    ) -> List[Dict[str, Any]]:
        """
        Query threads by criteria.
        
        Args:
            directive_id: Filter by directive
            status: Filter by status
            created_after: Filter by creation time (ISO format)
            created_before: Filter by creation time (ISO format)
            limit: Maximum results
            order_by: Sort order (SQL fragment)
            
        Returns:
            List of thread records
        """
        conn = self._get_connection()
        try:
            conditions = []
            params = []
            
            if directive_id:
                conditions.append("directive_id = ?")
                params.append(directive_id)
            
            if status:
                conditions.append("status = ?")
                params.append(status)
            
            if created_after:
                conditions.append("created_at >= ?")
                params.append(created_after)
            
            if created_before:
                conditions.append("created_at <= ?")
                params.append(created_before)
            
            where_clause = " AND ".join(conditions) if conditions else "1=1"
            
            query = f"""
                SELECT * FROM threads 
                WHERE {where_clause}
                ORDER BY {order_by}
                LIMIT ?
            """
            params.append(limit)
            
            rows = conn.execute(query, params).fetchall()
            
            results = []
            for row in rows:
                result = dict(row)
                # Parse JSON fields
                if result.get("permission_context_json"):
                    result["permission_context"] = json.loads(result["permission_context_json"])
                if result.get("cost_budget_json"):
                    result["cost_budget"] = json.loads(result["cost_budget_json"])
                if result.get("total_usage_json"):
                    result["total_usage"] = json.loads(result["total_usage_json"])
                results.append(result)
            
            return results
        finally:
            conn.close()
    
    def log_event(
        self,
        thread_id: str,
        event_type: str,
        payload: Optional[Dict[str, Any]] = None,
    ) -> None:
        """
        Log an event for a thread (append-only audit log).
        
        Args:
            thread_id: Thread identifier
            event_type: Event type (tool_call, error, warning, etc.)
            payload: Optional event payload
        """
        now = datetime.now(timezone.utc).isoformat()
        
        conn = self._get_connection()
        try:
            conn.execute("""
                INSERT INTO thread_events (thread_id, ts, event_type, payload_json)
                VALUES (?, ?, ?, ?)
            """, (
                thread_id,
                now,
                event_type,
                json.dumps(payload) if payload else None,
            ))
            conn.commit()
            logger.debug(f"Logged event {event_type} for thread {thread_id}")
        finally:
            conn.close()
    
    def get_events(
        self,
        thread_id: str,
        limit: int = 100,
        event_type: Optional[str] = None,
    ) -> List[Dict[str, Any]]:
        """
        Get events for a thread.
        
        Args:
            thread_id: Thread identifier
            limit: Maximum results
            event_type: Optional filter by event type
            
        Returns:
            List of event records
        """
        conn = self._get_connection()
        try:
            if event_type:
                rows = conn.execute("""
                    SELECT * FROM thread_events 
                    WHERE thread_id = ? AND event_type = ?
                    ORDER BY ts DESC
                    LIMIT ?
                """, (thread_id, event_type, limit)).fetchall()
            else:
                rows = conn.execute("""
                    SELECT * FROM thread_events 
                    WHERE thread_id = ?
                    ORDER BY ts DESC
                    LIMIT ?
                """, (thread_id, limit)).fetchall()
            
            results = []
            for row in rows:
                result = dict(row)
                if result.get("payload_json"):
                    result["payload"] = json.loads(result["payload_json"])
                results.append(result)
            
            return results
        finally:
            conn.close()


class TranscriptWriter:
    """
    JSONL transcript writer for human-readable thread logs.
    
    Append-only file per thread: .ai/threads/{thread_id}/transcript.jsonl
    """
    
    def __init__(self, transcript_dir: Path):
        """
        Initialize transcript writer.
        
        Args:
            transcript_dir: Base directory for transcripts
        """
        self.transcript_dir = Path(transcript_dir)
        self.transcript_dir.mkdir(parents=True, exist_ok=True)
    
    def write_event(
        self,
        thread_id: str,
        event_type: str,
        data: Dict[str, Any],
    ) -> None:
        """
        Write an event to the transcript file.
        
        Args:
            thread_id: Thread identifier
            event_type: Event type (turn_start, user_message, tool_call, etc.)
            data: Event data
        """
        transcript_path = self.transcript_dir / thread_id / "transcript.jsonl"
        transcript_path.parent.mkdir(parents=True, exist_ok=True)
        
        event = {
            "ts": datetime.now(timezone.utc).isoformat(),
            "type": event_type,
            **data,
        }
        
        with open(transcript_path, "a", encoding="utf-8") as f:
            f.write(json.dumps(event) + "\n")


# Tool entry point function
async def execute(action: str, **params) -> Dict[str, Any]:
    """
    Tool entry point for thread registry operations.
    
    Actions:
    - register: Register a new thread
    - update_status: Update thread status
    - get_status: Get thread status
    - query: Query threads by criteria
    - log_event: Log an event for a thread
    - get_events: Get events for a thread
    
    Args:
        action: Action to perform
        **params: Action-specific parameters
        
    Returns:
        Result dict
    """
    # Get config from params (passed by executor)
    project_path = Path(params.pop("_project_path", Path.cwd()))
    db_path = params.pop("db_path", project_path / ".ai" / "threads" / "registry.db")
    transcript_dir = params.pop("transcript_dir", project_path / ".ai" / "threads")
    
    # Parse complex parameters (handles both dicts and JSON strings)
    def parse_complex_param(value):
        """Parse complex parameter from various formats."""
        if value is None:
            return None
        if isinstance(value, dict):
            return value  # Already a dict
        if isinstance(value, str):
            # Try JSON first
            try:
                return json.loads(value)
            except json.JSONDecodeError:
                # Try Python dict literal (from str(dict))
                try:
                    import ast
                    return ast.literal_eval(value)
                except (ValueError, SyntaxError):
                    # Not parseable, return as-is
                    return value
        return value
    
    # Parse complex parameters
    if "permission_context" in params:
        params["permission_context"] = parse_complex_param(params["permission_context"])
    if "cost_budget" in params:
        params["cost_budget"] = parse_complex_param(params["cost_budget"])
    if "metadata" in params:
        params["metadata"] = parse_complex_param(params["metadata"])
    if "payload" in params:
        params["payload"] = parse_complex_param(params["payload"])
    
    registry = ThreadRegistry(Path(db_path))
    transcript_writer = TranscriptWriter(Path(transcript_dir))
    
    try:
        if action == "register":
            registry.register(
                thread_id=params["thread_id"],
                directive_id=params["directive_id"],
                parent_thread_id=params.get("parent_thread_id"),
                permission_context=params.get("permission_context"),
                cost_budget=params.get("cost_budget"),
            )
            return {"success": True, "thread_id": params["thread_id"]}
        
        elif action == "update_status":
            registry.update_status(
                thread_id=params["thread_id"],
                status=params["status"],
                metadata=params.get("metadata"),
            )
            return {"success": True, "thread_id": params["thread_id"], "status": params["status"]}
        
        elif action == "get_status":
            status = registry.get_status(params["thread_id"])
            if not status:
                return {"success": False, "error": f"Thread {params['thread_id']} not found"}
            return {"success": True, "status": status}
        
        elif action == "query":
            results = registry.query(
                directive_id=params.get("directive_id"),
                status=params.get("status"),
                created_after=params.get("created_after"),
                created_before=params.get("created_before"),
                limit=params.get("limit", 100),
                order_by=params.get("order_by", "created_at DESC"),
            )
            return {"success": True, "threads": results, "count": len(results)}
        
        elif action == "log_event":
            # Write to transcript first (always succeeds, even if DB fails)
            transcript_written = False
            if params.get("write_transcript", True):
                try:
                    transcript_writer.write_event(
                        thread_id=params["thread_id"],
                        event_type=params["event_type"],
                        data=params.get("payload", {}),
                    )
                    transcript_written = True
                except Exception as e:
                    logger.warning(f"Failed to write transcript: {e}")
            
            # Then write to registry (may fail if thread doesn't exist)
            try:
                registry.log_event(
                    thread_id=params["thread_id"],
                    event_type=params["event_type"],
                    payload=params.get("payload"),
                )
            except sqlite3.IntegrityError as e:
                if "FOREIGN KEY constraint failed" in str(e):
                    # Thread doesn't exist in registry, but transcript was written
                    return {
                        "success": True,
                        "thread_id": params["thread_id"],
                        "event_type": params["event_type"],
                        "warning": "Thread not found in registry, but transcript written",
                    }
                raise
            
            return {"success": True, "thread_id": params["thread_id"], "event_type": params["event_type"]}
        
        elif action == "get_events":
            events = registry.get_events(
                thread_id=params["thread_id"],
                limit=params.get("limit", 100),
                event_type=params.get("event_type"),
            )
            return {"success": True, "events": events, "count": len(events)}
        
        else:
            return {"success": False, "error": f"Unknown action: {action}"}
    
    except Exception as e:
        logger.exception(f"Error executing thread_registry action {action}")
        return {"success": False, "error": str(e)}


# CLI entry point for subprocess execution
if __name__ == "__main__":
    import asyncio
    import argparse
    import sys
    
    parser = argparse.ArgumentParser(description="Thread Registry Tool")
    parser.add_argument("--action", required=True, help="Action to perform")
    
    # Accept both --thread-id and --thread_id (executor may use underscores)
    parser.add_argument("--thread-id", "--thread_id", dest="thread_id", help="Thread ID")
    parser.add_argument("--directive-id", "--directive_id", dest="directive_id", help="Directive ID")
    parser.add_argument("--status", help="Status")
    parser.add_argument("--db-path", "--db_path", dest="db_path", help="Database path")
    parser.add_argument("--transcript-dir", "--transcript_dir", dest="transcript_dir", help="Transcript directory")
    parser.add_argument("--parent-thread-id", "--parent_thread_id", dest="parent_thread_id", help="Parent thread ID")
    parser.add_argument("--project-path", "--project_path", dest="project_path", help="Project path")
    
    # For complex parameters, accept JSON strings
    parser.add_argument("--permission-context", "--permission_context", dest="permission_context", help="Permission context (JSON)")
    parser.add_argument("--cost-budget", "--cost_budget", dest="cost_budget", help="Cost budget (JSON)")
    parser.add_argument("--metadata", help="Metadata (JSON)")
    parser.add_argument("--payload", help="Event payload (JSON)")
    parser.add_argument("--limit", type=int, help="Limit for queries")
    parser.add_argument("--event-type", "--event_type", dest="event_type", help="Event type")
    parser.add_argument("--created-after", "--created_after", dest="created_after", help="Created after (ISO format)")
    parser.add_argument("--created-before", "--created_before", dest="created_before", help="Created before (ISO format)")
    parser.add_argument("--order-by", "--order_by", dest="order_by", help="Order by clause")
    
    args = parser.parse_args()
    
    params = {}
    if args.thread_id:
        params["thread_id"] = args.thread_id
    if args.directive_id:
        params["directive_id"] = args.directive_id
    if args.status:
        params["status"] = args.status
    if args.db_path:
        params["db_path"] = args.db_path
    if args.transcript_dir:
        params["transcript_dir"] = args.transcript_dir
    if args.parent_thread_id:
        params["parent_thread_id"] = args.parent_thread_id
    if args.project_path:
        params["_project_path"] = Path(args.project_path)
    # Complex parameters come as JSON strings from executor
    # The execute function will parse them, so just pass through as strings
    if args.permission_context:
        params["permission_context"] = args.permission_context
    if args.cost_budget:
        params["cost_budget"] = args.cost_budget
    if args.metadata:
        params["metadata"] = args.metadata
    if args.payload:
        params["payload"] = args.payload
    if args.limit:
        params["limit"] = args.limit
    if args.event_type:
        params["event_type"] = args.event_type
    if args.created_after:
        params["created_after"] = args.created_after
    if args.created_before:
        params["created_before"] = args.created_before
    if args.order_by:
        params["order_by"] = args.order_by
    
    try:
        result = asyncio.run(execute(args.action, **params))
        print(json.dumps(result, indent=2))
        sys.exit(0 if result.get("success") else 1)
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}, indent=2))
        sys.exit(1)
