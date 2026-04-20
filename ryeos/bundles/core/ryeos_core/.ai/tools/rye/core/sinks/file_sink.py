# rye:signed:2026-04-19T09:49:53Z:458d6ee36ee86753a3804728aa333e7c71da0cb66d49c60443424d44009eb512:kK3KLztcQ/1mRNRTdkjC/YuGtiMuLj+ryVu6wv3bdwxDqv9JMP0D/d857huIdMYhCIyM2cuZrnDStV5PENocDQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
__tool_type__ = "runtime"
__version__ = "1.0.0"
__executor_id__ = "python"
__category__ = "rye/core/sinks"
__tool_description__ = (
    "File sink - append streaming events to file in JSONL or plain text format"
)

import json
import io
from pathlib import Path
from typing import Optional


class FileSink:
    """Append streaming events to file."""

    def __init__(self, path: str, format: str = "jsonl", flush_every: int = 10):
        self.path = Path(path)
        self.format = format
        self.flush_every = flush_every
        self.event_count = 0
        self.file_handle: Optional[io.TextIOWrapper] = None

        # Ensure parent directory exists
        self.path.parent.mkdir(parents=True, exist_ok=True)

    async def write(self, event: str) -> None:
        """Write event to file."""
        if not self.file_handle:
            self.file_handle = open(self.path, "a", encoding="utf-8")

        if self.format == "jsonl":
            # Parse SSE event and write as JSONL
            try:
                data = json.loads(event)
                self.file_handle.write(json.dumps(data) + "\n")
            except json.JSONDecodeError:
                # Write raw if not valid JSON
                self.file_handle.write(event + "\n")
        else:
            # Raw format
            self.file_handle.write(event + "\n")

        self.event_count += 1

        # Periodic flush for safety
        if self.event_count % self.flush_every == 0:
            self.file_handle.flush()

    async def close(self) -> None:
        """Close file handle."""
        if self.file_handle:
            self.file_handle.flush()
            self.file_handle.close()
            self.file_handle = None
