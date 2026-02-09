# kiwi-mcp:validated:2026-01-26T04:13:30Z:de139cf324562bfbc28d596b9ff346426a64f5916e7eae9a65662d8968e438f7
# .ai/tools/threads/read_transcript.py
__tool_type__ = "python"
__version__ = "1.0.0"
__executor_id__ = "python_runtime"
__category__ = "threads"

"""
Read Transcript Tool: Read another thread's conversation history.

A data-driven tool for reading thread transcripts from JSONL files.
Used for thread-to-thread intervention and annealing workflows.

Permissions: Requires intervention.read capability to read other threads' transcripts.
"""

import json
from pathlib import Path
from typing import Optional, Dict, Any, List
import logging

logger = logging.getLogger(__name__)


async def execute(thread_id: str, last_n: int = 10, **params) -> Dict[str, Any]:
    """
    Read a thread's transcript.
    
    Args:
        thread_id: Thread identifier to read
        last_n: Number of most recent entries to return (default: 10)
        **params: Additional parameters (project_path, etc.)
        
    Returns:
        Result dict with transcript entries
    """
    # Get project path
    project_path = Path(params.pop("_project_path", Path.cwd()))
    transcript_path = project_path / ".ai" / "threads" / thread_id / "transcript.jsonl"
    
    try:
        # Check if transcript exists
        if not transcript_path.exists():
            return {
                "success": False,
                "error": f"Transcript not found for thread {thread_id}",
                "thread_id": thread_id,
            }
        
        # Read transcript file
        entries = []
        with open(transcript_path, "r", encoding="utf-8") as f:
            lines = f.readlines()
        
        # Parse last N entries
        for line in lines[-last_n:]:
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
                entries.append(entry)
            except json.JSONDecodeError as e:
                logger.warning(f"Failed to parse transcript line: {e}")
                continue
        
        return {
            "success": True,
            "thread_id": thread_id,
            "entries": entries,
            "count": len(entries),
            "total_lines": len([l for l in lines if l.strip()]),
        }
    
    except Exception as e:
        logger.exception(f"Error reading transcript for thread {thread_id}")
        return {
            "success": False,
            "error": str(e),
            "thread_id": thread_id,
        }


# CLI entry point for subprocess execution
if __name__ == "__main__":
    import asyncio
    import argparse
    import sys
    
    parser = argparse.ArgumentParser(description="Read Thread Transcript Tool")
    parser.add_argument("--thread-id", "--thread_id", dest="thread_id", required=True, help="Thread ID")
    parser.add_argument("--last-n", "--last_n", dest="last_n", type=int, default=10, help="Number of recent entries")
    parser.add_argument("--project-path", "--project_path", dest="project_path", help="Project path")
    
    args = parser.parse_args()
    
    params = {}
    if args.project_path:
        params["_project_path"] = Path(args.project_path)
    
    try:
        result = asyncio.run(execute(args.thread_id, args.last_n, **params))
        print(json.dumps(result, indent=2))
        sys.exit(0 if result.get("success") else 1)
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}, indent=2))
        sys.exit(1)
