"""
Phase 4: Thread Channels — Multi-agent turn-based coordination.

Implements round-robin and on-demand turn protocols for coordinating
multiple agent threads in a shared channel with turn-taking logic.

Key concepts:
- Channel: Shared coordination space with multiple member threads
- Turn protocol: How threads take turns (round_robin or on_demand)
- Channel state: Persisted in .ai/threads/{channel_id}/channel.json
"""

import json
import logging
from datetime import datetime, timezone
from pathlib import Path
from typing import Dict, List, Optional, Any

logger = logging.getLogger(__name__)


class ThreadChannelState:
    """Represents the state of a thread channel."""
    
    def __init__(
        self,
        channel_id: str,
        thread_mode: str = "channel",
        members: Optional[List[Dict]] = None,
        turn_protocol: str = "round_robin",
        turn_order: Optional[List[str]] = None,
        current_turn: Optional[str] = None,
        turn_count: int = 0,
    ):
        """
        Initialize channel state.
        
        Args:
            channel_id: Unique channel identifier
            thread_mode: Always "channel" for channels
            members: List of member threads with structure:
                     [{"thread_id": "...", "directive": "..."}]
            turn_protocol: "round_robin" or "on_demand"
            turn_order: List of thread IDs in round-robin order
            current_turn: Which thread has the turn now
            turn_count: Total turns executed so far
        """
        self.channel_id = channel_id
        self.thread_mode = thread_mode
        self.members = members or []
        self.turn_protocol = turn_protocol
        
        # Initialize turn_order from members if not provided
        if turn_order is None:
            self.turn_order = [m["thread_id"] for m in self.members]
        else:
            self.turn_order = turn_order
        
        # Initialize current_turn to first member if not provided
        if current_turn is None and self.turn_order:
            self.current_turn = self.turn_order[0]
        else:
            self.current_turn = current_turn
        
        self.turn_count = turn_count
        self.created_at = datetime.now(timezone.utc).isoformat()
        self.updated_at = self.created_at
    
    def to_dict(self) -> Dict[str, Any]:
        """Serialize channel state to dict."""
        return {
            "channel_id": self.channel_id,
            "thread_mode": self.thread_mode,
            "members": self.members,
            "turn_protocol": self.turn_protocol,
            "turn_order": self.turn_order,
            "current_turn": self.current_turn,
            "turn_count": self.turn_count,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        }
    
    @staticmethod
    def from_dict(data: Dict) -> "ThreadChannelState":
        """Deserialize channel state from dict."""
        state = ThreadChannelState(
            channel_id=data.get("channel_id", ""),
            thread_mode=data.get("thread_mode", "channel"),
            members=data.get("members", []),
            turn_protocol=data.get("turn_protocol", "round_robin"),
            turn_order=data.get("turn_order", []),
            current_turn=data.get("current_turn"),
            turn_count=data.get("turn_count", 0),
        )
        state.created_at = data.get("created_at", state.created_at)
        state.updated_at = data.get("updated_at", state.updated_at)
        return state


def create_channel(
    channel_id: str,
    members: List[Dict],
    project_path: Path,
    turn_protocol: str = "round_robin",
) -> str:
    """
    Create a new thread channel.
    
    File structure:
    ```
    .ai/threads/{channel_id}/
        channel.json        (channel state)
        transcript.jsonl    (shared transcript)
    ```
    
    Args:
        channel_id: Unique channel identifier
        members: List of member threads [{"thread_id": "...", "directive": "..."}]
        project_path: Project root
        turn_protocol: "round_robin" or "on_demand"
        
    Returns:
        channel_id
        
    Raises:
        ValueError: If invalid turn_protocol
        Exception: If write fails
    """
    if turn_protocol not in ("round_robin", "on_demand"):
        raise ValueError(f"Invalid turn_protocol: {turn_protocol}")
    
    if not members:
        raise ValueError("Channel must have at least one member")
    
    # Create channel directory
    channel_dir = project_path / ".ai" / "threads" / channel_id
    channel_dir.mkdir(parents=True, exist_ok=True)
    
    # Initialize turn order
    turn_order = [m["thread_id"] for m in members]
    
    # Create channel state
    channel_state = ThreadChannelState(
        channel_id=channel_id,
        members=members,
        turn_protocol=turn_protocol,
        turn_order=turn_order,
        current_turn=turn_order[0] if turn_order else None,
        turn_count=0,
    )
    
    # Write channel.json
    channel_path = channel_dir / "channel.json"
    tmp_path = channel_path.with_suffix(".json.tmp")
    
    try:
        with open(tmp_path, "w", encoding="utf-8") as f:
            json.dump(channel_state.to_dict(), f, indent=2)
        tmp_path.rename(channel_path)
        logger.info(f"Created channel {channel_id} with protocol {turn_protocol}")
    except Exception as e:
        if tmp_path.exists():
            tmp_path.unlink()
        logger.error(f"Failed to create channel: {e}")
        raise
    
    return channel_id


def get_channel_state(
    channel_id: str,
    project_path: Path,
) -> ThreadChannelState:
    """
    Load channel state from disk.
    
    Args:
        channel_id: Channel identifier
        project_path: Project root
        
    Returns:
        ThreadChannelState instance
        
    Raises:
        FileNotFoundError: If channel.json doesn't exist
        ValueError: If channel.json is malformed
    """
    channel_path = project_path / ".ai" / "threads" / channel_id / "channel.json"
    
    if not channel_path.exists():
        raise FileNotFoundError(f"Channel not found: {channel_path}")
    
    try:
        data = json.loads(channel_path.read_text())
        return ThreadChannelState.from_dict(data)
    except json.JSONDecodeError as e:
        raise ValueError(f"Malformed channel.json: {e}")


def save_channel_state(
    channel_state: ThreadChannelState,
    project_path: Path,
) -> None:
    """
    Persist channel state to disk atomically.
    
    Args:
        channel_state: ThreadChannelState instance
        project_path: Project root
        
    Raises:
        Exception: If write fails
    """
    channel_dir = project_path / ".ai" / "threads" / channel_state.channel_id
    channel_dir.mkdir(parents=True, exist_ok=True)
    
    channel_path = channel_dir / "channel.json"
    tmp_path = channel_path.with_suffix(".json.tmp")
    
    try:
        channel_state.updated_at = datetime.now(timezone.utc).isoformat()
        with open(tmp_path, "w", encoding="utf-8") as f:
            json.dump(channel_state.to_dict(), f, indent=2)
        tmp_path.rename(channel_path)
        logger.debug(f"Saved channel state for {channel_state.channel_id}")
    except Exception as e:
        if tmp_path.exists():
            tmp_path.unlink()
        logger.error(f"Failed to save channel state: {e}")
        raise


def advance_turn_round_robin(
    channel_state: ThreadChannelState,
) -> str:
    """
    Advance to next thread in round-robin protocol.
    
    Args:
        channel_state: Channel state to mutate
        
    Returns:
        thread_id of the next turn
    """
    if not channel_state.turn_order:
        raise ValueError("Channel has no turn_order")
    
    # Find current index
    try:
        idx = channel_state.turn_order.index(channel_state.current_turn)
    except ValueError:
        idx = -1
    
    # Advance to next
    next_idx = (idx + 1) % len(channel_state.turn_order)
    channel_state.current_turn = channel_state.turn_order[next_idx]
    channel_state.turn_count += 1
    
    return channel_state.current_turn


def can_write_to_channel(
    origin_thread_id: str,
    channel_state: ThreadChannelState,
) -> bool:
    """
    Check if a thread can write to the channel.
    
    Turn protocol rules:
    - round_robin: Only current_turn thread can write
    - on_demand: Any member can write (turn-less)
    
    Args:
        origin_thread_id: Thread attempting to write
        channel_state: Channel state
        
    Returns:
        True if allowed to write
    """
    if channel_state.turn_protocol == "on_demand":
        # Any member can write
        return any(m["thread_id"] == origin_thread_id for m in channel_state.members)
    
    elif channel_state.turn_protocol == "round_robin":
        # Only current turn holder can write
        return origin_thread_id == channel_state.current_turn
    
    return False


def write_to_channel(
    channel_id: str,
    origin_thread_id: str,
    message: Dict[str, Any],
    project_path: Path,
    auto_advance: bool = True,
) -> Dict[str, Any]:
    """
    Write a message to the channel transcript.
    
    For round_robin protocol: advances turn after successful write.
    For on_demand protocol: no turn advancement.
    
    Args:
        channel_id: Channel identifier
        origin_thread_id: Thread ID writing to channel
        message: Message dict to write
        project_path: Project root
        auto_advance: Auto-advance turn for round_robin (default True)
        
    Returns:
        Updated channel state
        
    Raises:
        ValueError: If thread doesn't have write permission
        Exception: If write fails
    """
    # Load channel state
    channel_state = get_channel_state(channel_id, project_path)
    
    # Check permissions
    if not can_write_to_channel(origin_thread_id, channel_state):
        raise ValueError(
            f"Thread {origin_thread_id} cannot write to channel {channel_id}. "
            f"Current turn: {channel_state.current_turn}"
        )
    
    # Append to transcript
    transcript_path = project_path / ".ai" / "threads" / channel_id / "transcript.jsonl"
    transcript_path.parent.mkdir(parents=True, exist_ok=True)
    
    try:
        message_with_meta = {
            **message,
            "origin_thread": origin_thread_id,
            "timestamp": datetime.now(timezone.utc).isoformat(),
        }
        with open(transcript_path, "a", encoding="utf-8") as f:
            f.write(json.dumps(message_with_meta) + "\n")
    except Exception as e:
        logger.error(f"Failed to write to channel transcript: {e}")
        raise
    
    # Advance turn if round_robin and auto_advance
    if auto_advance and channel_state.turn_protocol == "round_robin":
        advance_turn_round_robin(channel_state)
    
    # Save updated state
    save_channel_state(channel_state, project_path)
    
    return channel_state.to_dict()


def read_channel_transcript(
    channel_id: str,
    project_path: Path,
    limit: Optional[int] = None,
) -> List[Dict]:
    """
    Read messages from channel transcript.
    
    Args:
        channel_id: Channel identifier
        project_path: Project root
        limit: Max messages to return (None for all)
        
    Returns:
        List of message dicts
        
    Raises:
        FileNotFoundError: If transcript doesn't exist
    """
    transcript_path = project_path / ".ai" / "threads" / channel_id / "transcript.jsonl"
    
    if not transcript_path.exists():
        return []
    
    messages = []
    try:
        with open(transcript_path, "r", encoding="utf-8") as f:
            for line in f:
                if not line.strip():
                    continue
                messages.append(json.loads(line))
        
        if limit:
            messages = messages[-limit:]
    
    except json.JSONDecodeError as e:
        logger.error(f"Malformed transcript: {e}")
        raise
    
    return messages
