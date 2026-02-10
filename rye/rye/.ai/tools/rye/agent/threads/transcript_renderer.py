# rye:validated:2026-02-10T00:42:37Z:718d8bd2d88bf737f640c5635dff5f0e70a819b4823468e9b6bdccbdc19b0add
"""
Transcript Renderer Tool: Standalone markdown renderer for thread transcripts.

Reads JSONL transcript files and renders human-readable markdown with
configurable options for thinking, tool details, and metadata visibility.

Permissions: Requires read capability for thread transcripts.
"""

__tool_type__ = "python"
__version__ = "1.0.0"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads"
__tool_description__ = "Render thread transcript from JSONL to markdown"

import json
from pathlib import Path
from typing import Optional, Dict, Any, List
import logging

logger = logging.getLogger(__name__)


class TranscriptRenderer:
    """
    Renders JSONL transcripts to human-readable markdown.
    
    Options:
    - thinking: Include assistant_reasoning events (default: True)
    - tool_details: Include tool input/output details (default: True)
    - metadata: Include thread metadata header (default: True)
    """
    
    def __init__(
        self,
        thinking: bool = True,
        tool_details: bool = True,
        metadata: bool = True,
    ):
        """
        Initialize renderer.
        
        Args:
            thinking: Include thinking/reasoning blocks
            tool_details: Include full tool input/output
            metadata: Include thread metadata header
        """
        self.thinking = thinking
        self.tool_details = tool_details
        self.metadata = metadata
    
    def render_file(self, jsonl_path: Path) -> str:
        """
        Render a transcript JSONL file to markdown.
        
        Args:
            jsonl_path: Path to transcript.jsonl file
            
        Returns:
            Markdown string
        """
        events = []
        try:
            with open(jsonl_path, "r", encoding="utf-8") as f:
                for line in f:
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        events.append(json.loads(line))
                    except json.JSONDecodeError as e:
                        logger.warning(f"Failed to parse event line: {e}")
                        continue
        except Exception as e:
            logger.error(f"Failed to read transcript file {jsonl_path}: {e}")
            return f"# Error\n\nFailed to read transcript: {e}"
        
        return self.render_events(events)
    
    def render_events(self, events: List[Dict[str, Any]]) -> str:
        """
        Render a list of events to markdown.
        
        Args:
            events: List of event dicts
            
        Returns:
            Markdown string
        """
        if not events:
            return "# No events\n\n"
        
        md_parts = []
        
        # Add metadata header if enabled
        if self.metadata:
            start_event = next((e for e in events if e.get("type") == "thread_start"), None)
            if start_event:
                md_parts.append(self._render_header(start_event))
        
        # Render events
        for event in events:
            chunk = self._render_event(event)
            if chunk:
                md_parts.append(chunk)
        
        return "".join(md_parts)
    
    def _render_header(self, start_event: Dict[str, Any]) -> str:
        """Render thread metadata header."""
        directive = start_event.get("directive", "Thread")
        thread_id = start_event.get("thread_id", "")
        model = start_event.get("model", "unknown")
        mode = start_event.get("thread_mode", "single")
        ts = start_event.get("ts", "")
        
        return (
            f"# {directive}\n\n"
            f"**Thread ID:** `{thread_id}`\n"
            f"**Model:** {model}\n"
            f"**Mode:** {mode}\n"
            f"**Started:** {ts}\n\n---\n\n"
        )
    
    def _render_event(self, event: Dict[str, Any]) -> str:
        """Render a single event to markdown."""
        event_type = event.get("type", "")
        
        # Skip metadata events
        if event_type in ("thread_start",):
            return ""
        
        # User/system messages
        if event_type == "user_message":
            role = event.get("role", "user").title()
            text = event.get("text", "")
            return f"## {role}\n\n{text}\n\n---\n\n"
        
        # Step markers
        if event_type == "step_start":
            turn = event.get("turn_number", "?")
            return f"### Turn {turn}\n\n"
        
        # Assistant response
        if event_type == "assistant_text":
            text = event.get("text", "")
            return f"**Assistant:**\n\n{text}\n\n"
        
        # Thinking/reasoning
        if event_type == "assistant_reasoning":
            if not self.thinking:
                return ""
            text = event.get("text", "")
            return f"_Thinking:_\n\n```\n{text}\n```\n\n"
        
        # Tool calls
        if event_type == "tool_call_start":
            tool = event.get("tool", "unknown")
            call_id = event.get("call_id", "?")
            
            if not self.tool_details:
                return f"**Tool:** `{tool}` (ID: `{call_id}`)\n\n"
            
            input_data = event.get("input", {})
            try:
                input_str = json.dumps(input_data, indent=2)
            except Exception:
                input_str = str(input_data)
            
            return (
                f"**Tool Call:** `{tool}` (ID: `{call_id}`)\n\n"
                f"```json\n{input_str}\n```\n\n"
            )
        
        # Tool results
        if event_type == "tool_call_result":
            call_id = event.get("call_id", "?")
            output = event.get("output", "")
            error = event.get("error")
            duration = event.get("duration_ms", 0)
            
            result = f"**Tool Result** (ID: `{call_id}`, {duration}ms)\n\n"
            if error:
                result += f"**Error:** {error}\n\n"
            elif self.tool_details:
                result += f"```\n{output}\n```\n\n"
            
            return result
        
        # Step completion
        if event_type == "step_finish":
            tokens = event.get("tokens", 0)
            cost = event.get("cost", 0)
            reason = event.get("finish_reason", "unknown")
            return f"_Step finished: {tokens} tokens, ${cost:.6f}, reason: {reason}_\n\n---\n\n"
        
        # Thread completion
        if event_type == "thread_complete":
            cost_dict = event.get("cost", {})
            tokens = cost_dict.get("tokens", 0)
            spend = cost_dict.get("spend", 0)
            return (
                f"## Completed\n\n"
                f"**Total Tokens:** {tokens}\n"
                f"**Total Cost:** ${spend:.6f}\n\n"
            )
        
        # Thread error
        if event_type == "thread_error":
            error_code = event.get("error_code", "unknown")
            detail = event.get("detail", "")
            return f"## Error: {error_code}\n\n{detail}\n\n"
        
        # Unknown event type
        return ""


async def execute(
    thread_id: str,
    thinking: bool = True,
    tool_details: bool = True,
    metadata: bool = True,
    output_path: Optional[str] = None,
    **params
) -> Dict[str, Any]:
    """
    Render a thread transcript to markdown.
    
    Args:
        thread_id: Thread identifier
        thinking: Include thinking blocks (default: True)
        tool_details: Include tool details (default: True)
        metadata: Include metadata header (default: True)
        output_path: If provided, write to this file path
        **params: Additional parameters (project_path, etc.)
        
    Returns:
        Result dict with rendered markdown
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
        
        # Render transcript
        renderer = TranscriptRenderer(
            thinking=thinking,
            tool_details=tool_details,
            metadata=metadata,
        )
        markdown = renderer.render_file(transcript_path)
        
        # Optionally write to output file
        if output_path:
            output_file = Path(output_path)
            output_file.parent.mkdir(parents=True, exist_ok=True)
            with open(output_file, "w", encoding="utf-8") as f:
                f.write(markdown)
        
        return {
            "success": True,
            "thread_id": thread_id,
            "markdown": markdown,
            "output_path": str(output_path) if output_path else None,
        }
    
    except Exception as e:
        logger.exception(f"Error rendering transcript for thread {thread_id}")
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
    
    parser = argparse.ArgumentParser(description="Transcript Renderer Tool")
    parser.add_argument("--thread-id", "--thread_id", dest="thread_id", required=True, help="Thread ID")
    parser.add_argument("--thinking", type=bool, default=True, help="Include thinking blocks")
    parser.add_argument("--tool-details", "--tool_details", dest="tool_details", type=bool, default=True, help="Include tool details")
    parser.add_argument("--metadata", type=bool, default=True, help="Include metadata header")
    parser.add_argument("--output-path", "--output_path", dest="output_path", help="Output file path")
    parser.add_argument("--project-path", "--project_path", dest="project_path", help="Project path")
    
    args = parser.parse_args()
    
    params = {}
    if args.project_path:
        params["_project_path"] = Path(args.project_path)
    
    try:
        result = asyncio.run(execute(
            thread_id=args.thread_id,
            thinking=args.thinking,
            tool_details=args.tool_details,
            metadata=args.metadata,
            output_path=args.output_path,
            **params,
        ))
        print(json.dumps(result, indent=2))
        sys.exit(0 if result.get("success") else 1)
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}, indent=2))
        sys.exit(1)
