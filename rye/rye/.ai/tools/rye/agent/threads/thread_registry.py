# rye:validated:2026-02-10T00:42:36Z:5cc8741fde67c583f18947f860d48992b4a924a0a937b34f74588a9cd80031e8
"""
Thread Registry: SQLite-based persistence for thread state and events.

A data-driven tool for tracking LLM agent threads, their status, events, and metadata.
Uses SQLite with WAL mode for concurrent access. Includes JSONL transcript writer
for human-readable logs.

This is a privileged harness tool - directives cannot call it directly;
only safety_harness can use it.
"""

__tool_type__ = "python"
__version__ = "1.0.0"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads"
__tool_description__ = "SQLite-based thread state and event persistence"

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
    JSONL transcript writer for human-readable thread logs with optional auto-markdown.
    
    Append-only file per thread: .ai/threads/{thread_id}/transcript.jsonl
    If auto_markdown enabled: also writes .ai/threads/{thread_id}/transcript.md
    
    Event envelope contract:
    - `ts`: ISO 8601 timestamp (UTC), auto-set by TranscriptWriter
    - `type`: event type string
    - `thread_id`: thread identifier, auto-set by TranscriptWriter
    - `directive`: directive name, MUST be provided via data or default_directive
    """
    
    def __init__(
        self,
        transcript_dir: Path,
        auto_markdown: bool = True,
        default_directive: Optional[str] = None,
    ):
        """
        Initialize transcript writer.
        
        Args:
            transcript_dir: Base directory for transcripts
            auto_markdown: If True, auto-generate transcript.md alongside transcript.jsonl
            default_directive: Default directive name if not in event data
        """
        self.transcript_dir = Path(transcript_dir)
        self.transcript_dir.mkdir(parents=True, exist_ok=True)
        self.auto_markdown = auto_markdown
        self.default_directive = default_directive
    
    def write_event(
        self,
        thread_id: str,
        event_type: str,
        data: Dict[str, Any],
    ) -> None:
        """
        Write an event to the transcript file.
        
        Event envelope contract: All events include ts, type, thread_id, directive.
        
        Args:
            thread_id: Thread identifier
            event_type: Event type (thread_start, user_message, tool_call, etc.)
            data: Event data. Must contain 'directive' or TranscriptWriter must have default_directive.
            
        Raises:
            ValueError: If 'directive' is missing from data and no default_directive set
        """
        transcript_path = self.transcript_dir / thread_id / "transcript.jsonl"
        transcript_path.parent.mkdir(parents=True, exist_ok=True)
        
        # Extract or use default directive
        directive = data.get("directive") or self.default_directive
        if not directive:
            raise ValueError(
                f"Event '{event_type}' missing 'directive'. "
                f"Provide it in data or set default_directive on TranscriptWriter."
            )
        
        # Build event envelope: ts, type, thread_id, directive are guaranteed
        event = {
            "ts": datetime.now(timezone.utc).isoformat(),
            "type": event_type,
            "thread_id": thread_id,
            "directive": directive,
            # Merge payload, filtering out envelope fields if they exist
            **{k: v for k, v in data.items() if k not in ("ts", "type", "thread_id", "directive")},
        }
        
        # Write to JSONL
        with open(transcript_path, "a", encoding="utf-8") as f:
            f.write(json.dumps(event) + "\n")
        
        # Auto-generate markdown if enabled
        if self.auto_markdown:
            md_path = self.transcript_dir / thread_id / "transcript.md"
            md_chunk = self._render_event_to_markdown(event)
            if md_chunk:
                with open(md_path, "a", encoding="utf-8") as f:
                    f.write(md_chunk)
    
    def _render_event_to_markdown(self, event: Dict[str, Any]) -> str:
        """
        Render a single event to a markdown fragment for transcript.md.
        
        Args:
            event: Event dict with at least 'type' field
            
        Returns:
            Markdown string fragment (may be empty for some event types)
        """
        event_type = event.get("type", "")
        
        if event_type == "thread_start":
            return (
                f"# {event.get('directive', 'Thread')}\n\n"
                f"**Thread ID:** `{event.get('thread_id', '')}`\n"
                f"**Model:** {event.get('model', 'unknown')}\n"
                f"**Provider:** {event.get('provider', 'unknown')}\n"
                f"**Mode:** {event.get('thread_mode', 'single')}\n"
                f"**Started:** {event.get('ts', '')}\n\n---\n\n"
            )
        
        elif event_type == "user_message":
            role = event.get("role", "user").title()
            text = event.get("text", "")
            return f"## {role}\n\n{text}\n\n---\n\n"
        
        elif event_type == "step_start":
            turn = event.get("turn_number", "?")
            return f"### Turn {turn}\n\n"
        
        elif event_type == "assistant_text":
            text = event.get("text", "")
            return f"**Assistant:**\n\n{text}\n\n"
        
        elif event_type == "assistant_reasoning":
            text = event.get("text", "")
            return f"_Thinking:_\n\n```\n{text}\n```\n\n"
        
        elif event_type == "tool_call_start":
            tool = event.get("tool", "unknown")
            call_id = event.get("call_id", "?")
            input_data = event.get("input", {})
            return (
                f"**Tool Call:** `{tool}` (ID: `{call_id}`)\n\n"
                f"```json\n{json.dumps(input_data, indent=2)}\n```\n\n"
            )
        
        elif event_type == "tool_call_result":
            call_id = event.get("call_id", "?")
            output = event.get("output", "")
            error = event.get("error")
            duration = event.get("duration_ms", 0)
            
            result_str = f"**Tool Result** (ID: `{call_id}`, {duration}ms)\n\n"
            if error:
                result_str += f"**Error:** {error}\n\n"
            else:
                result_str += f"```\n{output}\n```\n\n"
            return result_str
        
        elif event_type == "step_finish":
            tokens = event.get("tokens", 0)
            cost = event.get("cost", 0)
            finish_reason = event.get("finish_reason", "unknown")
            return f"_Step finished: {tokens} tokens, ${cost:.6f}, reason: {finish_reason}_\n\n---\n\n"
        
        elif event_type == "thread_complete":
            cost_dict = event.get("cost", {})
            tokens = cost_dict.get("tokens", 0)
            spend = cost_dict.get("spend", 0)
            return (
                f"## Completed\n\n"
                f"**Total Tokens:** {tokens}\n"
                f"**Total Cost:** ${spend:.6f}\n\n"
            )
        
        elif event_type == "thread_error":
            error_code = event.get("error_code", "unknown")
            detail = event.get("detail", "")
            return f"## Error: {error_code}\n\n{detail}\n\n"
        
        # Default: no markdown for unknown event types
        return ""


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
