# rye:validated:2026-02-10T00:42:37Z:c986d23c5cd1fde7e2b80ed8e6930e2dbe825cec014e5d87af92427e6e43cff2
"""
Thread Telemetry Tool: Aggregate metrics from thread executions.

Reads thread.json files from .ai/threads/ directory and computes
statistics: thread counts, cost aggregation, grouping by directive, etc.

Permissions: Requires read capability for thread metadata.
"""

__tool_type__ = "python"
__version__ = "1.0.0"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads"
__tool_description__ = "Aggregate telemetry metrics from thread executions"

import json
from pathlib import Path
from typing import Optional, Dict, Any, List
import logging
from datetime import datetime, timezone

logger = logging.getLogger(__name__)


class ThreadTelemetry:
    """
    Aggregate telemetry metrics from thread.json files.
    
    Computes:
    - Total thread counts by status
    - Cost aggregation (tokens, spend)
    - Grouping by directive
    - Time-based filtering
    """
    
    def __init__(self, threads_dir: Path):
        """
        Initialize telemetry aggregator.
        
        Args:
            threads_dir: .ai/threads directory
        """
        self.threads_dir = Path(threads_dir)
    
    def aggregate_all(self) -> Dict[str, Any]:
        """
        Aggregate all threads in the directory.
        
        Returns:
            Aggregated metrics dict
        """
        threads = self._load_all_threads()
        
        return {
            "total_threads": len(threads),
            "by_status": self._group_by_status(threads),
            "by_directive": self._group_by_directive(threads),
            "cost_summary": self._aggregate_costs(threads),
            "threads": threads,
        }
    
    def aggregate_by_directive(self, directive_name: Optional[str] = None) -> Dict[str, Any]:
        """
        Aggregate metrics for a specific directive or all directives.
        
        Args:
            directive_name: If provided, filter to this directive
            
        Returns:
            Aggregated metrics dict
        """
        threads = self._load_all_threads()
        
        if directive_name:
            threads = [t for t in threads if t.get("directive") == directive_name]
        
        return {
            "directive": directive_name,
            "total": len(threads),
            "by_status": self._group_by_status(threads),
            "cost_summary": self._aggregate_costs(threads),
            "threads": threads,
        }
    
    def aggregate_by_status(self, status: str) -> Dict[str, Any]:
        """
        Get all threads with a specific status.
        
        Args:
            status: Status to filter (running, completed, error)
            
        Returns:
            Aggregated metrics dict
        """
        threads = self._load_all_threads()
        threads = [t for t in threads if t.get("status") == status]
        
        return {
            "status": status,
            "total": len(threads),
            "by_directive": self._group_by_directive(threads),
            "cost_summary": self._aggregate_costs(threads),
            "threads": threads,
        }
    
    def _load_all_threads(self) -> List[Dict[str, Any]]:
        """Load all thread.json files from threads directory."""
        threads = []
        
        if not self.threads_dir.exists():
            return threads
        
        for thread_dir in self.threads_dir.iterdir():
            if not thread_dir.is_dir():
                continue
            
            meta_path = thread_dir / "thread.json"
            if not meta_path.exists():
                continue
            
            try:
                with open(meta_path, "r", encoding="utf-8") as f:
                    meta = json.load(f)
                threads.append(meta)
            except json.JSONDecodeError:
                logger.warning(f"Failed to parse {meta_path}")
                continue
            except Exception as e:
                logger.warning(f"Failed to load {meta_path}: {e}")
                continue
        
        return threads
    
    def _group_by_status(self, threads: List[Dict[str, Any]]) -> Dict[str, int]:
        """Group threads by status."""
        groups = {}
        for thread in threads:
            status = thread.get("status", "unknown")
            groups[status] = groups.get(status, 0) + 1
        return groups
    
    def _group_by_directive(self, threads: List[Dict[str, Any]]) -> Dict[str, Dict[str, Any]]:
        """Group threads by directive with counts and costs."""
        groups = {}
        for thread in threads:
            directive = thread.get("directive", "unknown")
            
            if directive not in groups:
                groups[directive] = {
                    "count": 0,
                    "by_status": {},
                    "cost": {
                        "total_tokens": 0,
                        "total_spend": 0.0,
                    },
                }
            
            groups[directive]["count"] += 1
            
            # Track status
            status = thread.get("status", "unknown")
            groups[directive]["by_status"][status] = groups[directive]["by_status"].get(status, 0) + 1
            
            # Aggregate costs
            cost = thread.get("cost", {})
            groups[directive]["cost"]["total_tokens"] += cost.get("tokens", 0)
            groups[directive]["cost"]["total_spend"] += cost.get("spend", 0.0)
        
        return groups
    
    def _aggregate_costs(self, threads: List[Dict[str, Any]]) -> Dict[str, Any]:
        """Aggregate cost metrics across threads."""
        total_tokens = 0
        total_spend = 0.0
        total_turns = 0
        
        for thread in threads:
            cost = thread.get("cost", {})
            total_tokens += cost.get("tokens", 0)
            total_spend += cost.get("spend", 0.0)
            total_turns += cost.get("turns", 0)
        
        return {
            "total_tokens": total_tokens,
            "total_spend": total_spend,
            "total_turns": total_turns,
            "average_tokens_per_thread": total_tokens // len(threads) if threads else 0,
            "average_spend_per_thread": total_spend / len(threads) if threads else 0.0,
        }


async def execute(
    mode: str = "all",
    directive: Optional[str] = None,
    status: Optional[str] = None,
    **params
) -> Dict[str, Any]:
    """
    Aggregate thread telemetry metrics.
    
    Modes:
    - all: Aggregate all threads
    - directive: Group by directive (optionally filter to specific directive)
    - status: Filter by status (completed, error, running)
    
    Args:
        mode: Aggregation mode (all, directive, status)
        directive: Optional directive name filter
        status: Optional status filter (running, completed, error)
        **params: Additional parameters (project_path, etc.)
        
    Returns:
        Result dict with aggregated metrics
    """
    # Get project path
    project_path = Path(params.pop("_project_path", Path.cwd()))
    threads_dir = project_path / ".ai" / "threads"
    
    try:
        telemetry = ThreadTelemetry(threads_dir)
        
        if mode == "all":
            result = telemetry.aggregate_all()
        elif mode == "directive":
            result = telemetry.aggregate_by_directive(directive)
        elif mode == "status":
            if not status:
                return {
                    "success": False,
                    "error": "status parameter required for mode='status'",
                }
            result = telemetry.aggregate_by_status(status)
        else:
            return {
                "success": False,
                "error": f"Unknown mode: {mode}. Use 'all', 'directive', or 'status'",
            }
        
        return {
            "success": True,
            "mode": mode,
            **result,
        }
    
    except Exception as e:
        logger.exception(f"Error aggregating thread telemetry")
        return {
            "success": False,
            "error": str(e),
        }


# CLI entry point for subprocess execution
if __name__ == "__main__":
    import asyncio
    import argparse
    import sys
    
    parser = argparse.ArgumentParser(description="Thread Telemetry Tool")
    parser.add_argument("--mode", default="all", help="Aggregation mode: all, directive, status")
    parser.add_argument("--directive", help="Filter by directive name")
    parser.add_argument("--status", help="Filter by status: running, completed, error")
    parser.add_argument("--project-path", "--project_path", dest="project_path", help="Project path")
    
    args = parser.parse_args()
    
    params = {}
    if args.project_path:
        params["_project_path"] = Path(args.project_path)
    
    try:
        result = asyncio.run(execute(
            mode=args.mode,
            directive=args.directive,
            status=args.status,
            **params,
        ))
        print(json.dumps(result, indent=2))
        sys.exit(0 if result.get("success") else 1)
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}, indent=2))
        sys.exit(1)
