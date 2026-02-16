# rye:signed:2026-02-16T05:32:26Z:f36897dac5d892d8ca924c54d930897dfb67a6856f7dd1b9987a96b9442df898:2AzrNls6gAnyNCudn_UVTRaN5wPJ1SdcHXltGiI9rq3O03mfCfzkXMn9hBFY2B3GZLAHo5O_yYdIY9A0IBjEAg==:440443d0858f0199
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
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

    thread_dir = Path(project_path) / AI_DIR / "threads" / thread_id
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
