"""
Phase 5: TranscriptWatcher — Incremental transcript polling.

Watches transcript.jsonl files and incrementally reads new entries using file
seek positions. Enables real-time monitoring of running threads without
reading entire file on each poll.

Key design:
- Tracks last read position in file seek position
- Reads only new lines appended since last poll
- Efficient for long-running threads with growing transcripts
"""

import json
import logging
from pathlib import Path
from typing import Dict, List, Optional, Any

logger = logging.getLogger(__name__)


class TranscriptWatcher:
    """Watches a transcript.jsonl file and polls for new events."""
    
    def __init__(
        self,
        thread_id: str,
        project_path: Path,
        initial_position: int = 0,
    ):
        """
        Initialize transcript watcher for a thread.
        
        Args:
            thread_id: Thread ID to watch
            project_path: Project root
            initial_position: Starting file position (0 for beginning)
        """
        self.thread_id = thread_id
        self.project_path = project_path
        self.transcript_path = project_path / ".ai" / "threads" / thread_id / "transcript.jsonl"
        self.position = initial_position
    
    def poll(self) -> List[Dict[str, Any]]:
        """
        Poll for new events in transcript.
        
        Reads all lines since last position and advances position pointer.
        Returns empty list if no new events or transcript doesn't exist.
        
        Returns:
            List of new event dicts since last poll
            
        Raises:
            ValueError: If transcript is malformed (JSON parse error)
        """
        if not self.transcript_path.exists():
            return []
        
        events = []
        try:
            with open(self.transcript_path, "r", encoding="utf-8") as f:
                # Seek to last known position
                f.seek(self.position)
                
                # Read all new lines
                for line in f:
                    if not line.strip():
                        continue
                    
                    try:
                        event = json.loads(line)
                        events.append(event)
                    except json.JSONDecodeError as e:
                        logger.error(f"Malformed event in transcript: {e}")
                        raise ValueError(f"Malformed transcript line: {e}")
                
                # Update position
                self.position = f.tell()
        
        except IOError as e:
            logger.error(f"Failed to read transcript: {e}")
            raise
        
        return events
    
    def reset_position(self) -> None:
        """Reset position to beginning of file."""
        self.position = 0
    
    def seek_to_end(self) -> None:
        """Seek to end of file (useful for following mode)."""
        if self.transcript_path.exists():
            try:
                with open(self.transcript_path, "r", encoding="utf-8") as f:
                    f.seek(0, 2)  # Seek to end
                    self.position = f.tell()
            except IOError as e:
                logger.warning(f"Failed to seek to end: {e}")
    
    def get_position(self) -> int:
        """Get current file position."""
        return self.position


class MultiThreadWatcher:
    """Watches multiple threads' transcripts simultaneously."""
    
    def __init__(self, project_path: Path):
        """
        Initialize multi-thread watcher.
        
        Args:
            project_path: Project root
        """
        self.project_path = project_path
        self.watchers: Dict[str, TranscriptWatcher] = {}
    
    def watch(self, thread_id: str) -> TranscriptWatcher:
        """
        Start watching a thread's transcript.
        
        Args:
            thread_id: Thread to watch
            
        Returns:
            TranscriptWatcher instance (cached if already watching)
        """
        if thread_id not in self.watchers:
            self.watchers[thread_id] = TranscriptWatcher(thread_id, self.project_path)
        return self.watchers[thread_id]
    
    def unwatch(self, thread_id: str) -> None:
        """Stop watching a thread."""
        self.watchers.pop(thread_id, None)
    
    def poll_all(self) -> Dict[str, List[Dict[str, Any]]]:
        """
        Poll all watched threads for new events.
        
        Returns:
            Dict mapping thread_id → list of new events
        """
        results = {}
        for thread_id, watcher in self.watchers.items():
            try:
                results[thread_id] = watcher.poll()
            except Exception as e:
                logger.warning(f"Failed to poll thread {thread_id}: {e}")
                results[thread_id] = []
        return results
    
    def get_latest_events(self, thread_id: str, count: int = 10) -> List[Dict[str, Any]]:
        """
        Get latest N events from a thread without polling.
        
        Reads entire transcript but returns only last N events.
        Useful for status checks.
        
        Args:
            thread_id: Thread ID
            count: Number of events to return
            
        Returns:
            Last N events (fewer if thread has fewer events)
        """
        transcript_path = self.project_path / ".ai" / "threads" / thread_id / "transcript.jsonl"
        
        if not transcript_path.exists():
            return []
        
        events = []
        try:
            with open(transcript_path, "r", encoding="utf-8") as f:
                for line in f:
                    if not line.strip():
                        continue
                    events.append(json.loads(line))
        except Exception as e:
            logger.error(f"Failed to read transcript: {e}")
            return []
        
        return events[-count:] if count else events


# Convenience functions

def watch_thread(
    thread_id: str,
    project_path: Path,
) -> TranscriptWatcher:
    """
    Create a watcher for a single thread.
    
    Args:
        thread_id: Thread to watch
        project_path: Project root
        
    Returns:
        TranscriptWatcher instance
    """
    return TranscriptWatcher(thread_id, project_path)


def get_new_events(
    thread_id: str,
    project_path: Path,
    watcher: Optional[TranscriptWatcher] = None,
) -> List[Dict[str, Any]]:
    """
    Get new events from transcript (single thread).
    
    Creates temporary watcher if not provided.
    
    Args:
        thread_id: Thread ID
        project_path: Project root
        watcher: Optional existing watcher
        
    Returns:
        List of new events since last poll
    """
    if not watcher:
        watcher = TranscriptWatcher(thread_id, project_path)
    return watcher.poll()
