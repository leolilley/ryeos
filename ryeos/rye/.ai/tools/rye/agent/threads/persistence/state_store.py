# rye:signed:2026-02-23T00:42:51Z:b91d4c24e8884f9abe3c4df4407a5b61c3534fa30f2c133e9c92751d82b42c40:1GIeDBPjyza1gHBMvz4UHlP2KK2UGhMbOZ2eYCnuWk6I6OlnWEbOzSQ1OBMa5tcNnLtahsvJTYZ7Sj4xe3FBDQ==:9fbfabe975fa5a7f
"""
persistence/state_store.py: Atomic thread state persistence

Persists thread state to state.json in .ai/agent/threads/
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "Thread state persistence store"

import json
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, Optional

from rye.constants import AI_DIR


class StateStore:
    """Atomic thread state persistence."""

    def __init__(self, project_path: Path):
        self.project_path = Path(project_path)
        self.state_dir = self.project_path / AI_DIR / "agent" / "threads"
        self.state_dir.mkdir(parents=True, exist_ok=True)

    def save_state(self, thread_id: str, state: Dict[str, Any]):
        """Save thread state atomically.

        Writes to .ai/agent/threads/{thread_id}/state.json
        """
        thread_dir = self.state_dir / thread_id
        thread_dir.mkdir(parents=True, exist_ok=True)

        state_file = thread_dir / "state.json"
        tmp_file = thread_dir / "state.json.tmp"

        # Write to temp file
        with open(tmp_file, "w") as f:
            json.dump(
                {
                    **state,
                    "saved_at": datetime.utcnow().isoformat(),
                },
                f,
                indent=2,
            )

        # Atomic rename
        tmp_file.replace(state_file)

    def load_state(self, thread_id: str) -> Optional[Dict[str, Any]]:
        """Load thread state."""
        state_file = self.state_dir / thread_id / "state.json"
        if not state_file.exists():
            return None

        with open(state_file) as f:
            return json.load(f)

    def save_transcript(self, thread_id: str, transcript: list):
        """Save event transcript."""
        thread_dir = self.state_dir / thread_id
        thread_dir.mkdir(parents=True, exist_ok=True)

        transcript_file = thread_dir / "transcript.json"
        with open(transcript_file, "w") as f:
            json.dump(transcript, f, indent=2)

    def load_transcript(self, thread_id: str) -> list:
        """Load event transcript."""
        transcript_file = self.state_dir / thread_id / "transcript.json"
        if not transcript_file.exists():
            return []

        with open(transcript_file) as f:
            return json.load(f)

    def request_cancel(self, thread_id: str):
        """Request thread cancellation."""
        thread_dir = self.state_dir / thread_id
        thread_dir.mkdir(parents=True, exist_ok=True)

        cancel_file = thread_dir / ".cancel_requested"
        cancel_file.write_text("")

    def check_cancel_requested(self, thread_id: str) -> bool:
        """Check if cancellation requested."""
        cancel_file = self.state_dir / thread_id / ".cancel_requested"
        return cancel_file.exists()

    def clear_cancel_request(self, thread_id: str):
        """Clear cancellation request."""
        cancel_file = self.state_dir / thread_id / ".cancel_requested"
        if cancel_file.exists():
            cancel_file.unlink()

    def request_suspend(self, thread_id: str):
        """Request thread suspension."""
        thread_dir = self.state_dir / thread_id
        thread_dir.mkdir(parents=True, exist_ok=True)

        suspend_file = thread_dir / ".suspend_requested"
        suspend_file.write_text("")

    def check_suspend_requested(self, thread_id: str) -> bool:
        """Check if suspension requested."""
        suspend_file = self.state_dir / thread_id / ".suspend_requested"
        return suspend_file.exists()

    def clear_suspend_request(self, thread_id: str):
        """Clear suspension request."""
        suspend_file = self.state_dir / thread_id / ".suspend_requested"
        if suspend_file.exists():
            suspend_file.unlink()
