# rye:signed:2026-02-25T00:02:14Z:53100e67c6058928571264c36a18257d468d7ddfa98ee2efa5a3098d974a5037:ekTvNb_dzaEanWVcaaCEh_FSJG_lqrt6bq9CBlk4ECx9EtvEdY2UL9RmMgiAH8wYGkS4-IAv42hPAr6zwDSFDA==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Emit transcript events"

import json
import time
from pathlib import Path
from typing import Dict

from rye.constants import AI_DIR

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "event_type": {"type": "string"},
        "payload": {"type": "object", "default": {}},
        "thread_id": {"type": "string"},
    },
    "required": ["event_type", "thread_id"],
}


def execute(params: Dict, project_path: str) -> Dict:
    """Emit an event to the transcript files."""
    event_type = params.get("event_type")
    payload = params.get("payload", {})
    thread_id = params.get("thread_id", "unknown")

    thread_dir = Path(project_path) / AI_DIR / "agent" / "threads" / thread_id
    thread_dir.mkdir(parents=True, exist_ok=True)

    entry = {
        "timestamp": time.time(),
        "thread_id": thread_id,
        "event_type": event_type,
        "payload": payload,
    }

    # Append to JSONL
    jsonl_path = thread_dir / "transcript.jsonl"
    with open(jsonl_path, "a") as f:
        f.write(json.dumps(entry, default=str) + "\n")
        f.flush()

    return {"success": True, "event_type": event_type, "emitted": True}
