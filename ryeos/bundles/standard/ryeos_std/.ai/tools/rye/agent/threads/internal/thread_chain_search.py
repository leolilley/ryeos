# rye:signed:2026-02-26T03:49:32Z:497deaa35aa7a9ee21303ff89e4e86fa94f27f05f4d4b71966e6178fca0e95d0:EMFztw1P4l6x0iB2P7FWWOmYL-oCRC1SFbgQuT6AwioaR7LtGBLE5IIA2Z7Z9g3gtwXGS_Km_pZTEZxtrfb6Dw==:9fbfabe975fa5a7f
# internal/thread_chain_search.py
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Search across all threads in a continuation chain"

import json
import re
from pathlib import Path
from typing import Dict

from rye.constants import AI_DIR

from module_loader import load_module

_ANCHOR = Path(__file__).parent.parent

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
    thread_registry = load_module("persistence/thread_registry", anchor=_ANCHOR)

    thread_id = params["thread_id"]
    query = params["query"]
    search_type = params.get("search_type", "text")
    include_events = set(params.get("include_events", [
        "cognition_in", "cognition_out", "tool_call_start", "tool_call_result"
    ]))
    max_results = params.get("max_results", 50)

    proj_path = Path(project_path)
    registry = thread_registry.get_registry(proj_path)

    # Get the full chain
    chain = registry.get_chain(thread_id)
    if not chain:
        return {"success": False, "error": f"No chain found for thread {thread_id}"}

    results = []
    pattern = re.compile(query, re.IGNORECASE) if search_type == "regex" else None

    for thread in chain:
        tid = thread["thread_id"]
        transcript_path = proj_path / AI_DIR / "agent" / "threads" / tid / "transcript.jsonl"

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
