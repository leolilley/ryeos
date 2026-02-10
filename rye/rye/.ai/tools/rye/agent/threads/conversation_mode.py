"""
Phase 2: Conversation Mode - Multi-turn thread support.

Implements continue_thread function for pausing/resuming threads with:
- Conversation history reconstruction
- Turn-based cost tracking
- Harness state persistence
- Message appending to transcript
"""

import json
import logging
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, Optional

from core_helpers import run_llm_loop, save_state, rebuild_conversation_from_transcript

logger = logging.getLogger(__name__)


async def continue_thread(
    thread_id: str,
    message: str,
    project_path: Path,
    role: str = "user",
) -> Dict[str, Any]:
    """
    Continue a paused conversation thread with a new message.
    
    Thread lifecycle for conversation mode:
    ```
    start → running → paused → running → paused → ... → completed
    ```
    
    This function:
    1. Loads thread metadata and validates it's conversation mode
    2. Changes status from "paused" to "running"
    3. Appends new message to transcript
    4. Reconstructs full conversation history from transcript
    5. Restores harness state (cost tracking continues)
    6. Runs LLM loop with existing conversation context
    7. Persists updated harness state
    8. Updates thread metadata (turn_count, cumulative cost)
    9. Changes status back to "paused" (awaiting next message)
    
    Args:
        thread_id: Thread identifier (e.g., "planner-1739012630")
        message: New message text to send to LLM
        project_path: Project root path
        role: Message role ("user" by default)
        
    Returns:
        Dict with keys:
        - success: bool
        - status: "completed" | "paused" | "error"
        - turn_count: Total turns executed so far
        - cost: Cumulative cost across all turns
        - text: Final assistant response text
        - error: Error message if failed
        
    Raises:
        ValueError: If thread not found, not conversation mode, or invalid status
        FileNotFoundError: If state.json missing
        Exception: If harness state corrupted
    """
    threads_dir = project_path / ".ai" / "threads"
    thread_dir = threads_dir / thread_id
    meta_path = thread_dir / "thread.json"
    state_path = thread_dir / "state.json"
    
    # Validate thread exists
    if not meta_path.exists():
        raise ValueError(f"Thread not found: {thread_id}")
    
    # Load metadata
    try:
        meta = json.loads(meta_path.read_text())
    except json.JSONDecodeError as e:
        raise ValueError(f"Corrupted thread.json: {e}")
    
    # Validate thread is conversation mode
    thread_mode = meta.get("thread_mode", "single")
    if thread_mode != "conversation":
        raise ValueError(
            f"Cannot continue thread {thread_id}: thread_mode is '{thread_mode}', not 'conversation'. "
            f"Only conversation threads can be paused and resumed."
        )
    
    # Validate status allows continuation
    current_status = meta.get("status")
    if current_status not in ("paused", "completed"):
        raise ValueError(
            f"Cannot continue thread {thread_id}: status is '{current_status}'. "
            f"Can only continue paused or completed threads. "
            f"(running threads must finish their turn first)"
        )
    
    # Validate state.json exists
    if not state_path.exists():
        raise FileNotFoundError(f"Thread state not found: {state_path}")
    
    # Update status to running
    meta["status"] = "running"
    meta["awaiting"] = None
    
    try:
        meta_path.write_text(json.dumps(meta, indent=2))
    except Exception as e:
        logger.error(f"Failed to update thread status to running: {e}")
        raise
    
    # Import required modules
    from thread_registry import TranscriptWriter
    from safety_harness import SafetyHarness
    
    directive_name = meta.get("directive")
    if not directive_name:
        raise ValueError(f"Thread metadata missing 'directive' field")
    
    # Append new user message to transcript
    transcript = TranscriptWriter(threads_dir)
    try:
        transcript.write_event(thread_id, "user_message", {
            "text": message,
            "role": role,
            "directive": directive_name,
        })
    except Exception as e:
        logger.error(f"Failed to append user message to transcript: {e}")
        raise
    
    # Load provider config to reconstruct conversation
    model_config = meta.get("model", {})
    if isinstance(model_config, str):
        # Single model ID, need to resolve full config
        model_id = model_config
        # Load default provider config
        provider_id = meta.get("provider", "rye/agent/providers/anthropic_messages")
    else:
        model_id = model_config.get("id", meta.get("model_id", "claude-3-5-haiku-20241022"))
        provider_id = model_config.get("provider", "rye/agent/providers/anthropic_messages")
    
    # Load provider config
    def _load_provider_config(provider_id: str, project_path: Path) -> Dict:
        """Load provider YAML config."""
        import yaml
        
        if "/" in provider_id:
            parts = provider_id.split("/")
            # Build path from all parts except last: .ai/tools/rye/agent/providers/
            # Then add last part as filename: anthropic_messages.yaml
            dir_parts = parts[:-1]
            config_path = project_path / ".ai" / "tools" / Path(*dir_parts) / f"{parts[-1]}.yaml"
        else:
            config_path = project_path / ".ai" / "tools" / "agent" / "providers" / f"{provider_id}.yaml"
        
        if not config_path.exists():
            # Fall back to system provider config
            import importlib.util
            from pathlib import Path as PathLib
            system_path = PathLib(__file__).parent / "configs" / f"{provider_id}.yaml"
            if system_path.exists():
                config_path = system_path
            else:
                raise ValueError(f"Provider config not found: {provider_id}")
        
        try:
            return yaml.safe_load(config_path.read_text())
        except Exception as e:
            raise ValueError(f"Failed to load provider config {provider_id}: {e}")
    
    try:
        provider_config = _load_provider_config(provider_id, project_path)
    except Exception as e:
        logger.error(f"Failed to load provider config: {e}")
        raise
    
    # Reconstruct conversation from transcript
    try:
        messages = rebuild_conversation_from_transcript(thread_id, project_path, provider_config)
    except Exception as e:
        logger.error(f"Failed to reconstruct conversation: {e}")
        raise
    
    # Restore harness state
    if not state_path.exists():
        raise FileNotFoundError(f"Harness state file not found: {state_path}")
    
    try:
        harness_state = json.loads(state_path.read_text())
        harness = SafetyHarness.from_state_dict(harness_state, project_path)
    except Exception as e:
        logger.error(f"Failed to restore harness state: {e}")
        raise
    
    # Get tool definitions from metadata
    tool_defs = meta.get("tools", [])
    if not tool_defs:
        # Empty tool list is valid (thread may have no tools)
        tool_defs = []
    
    # Build tool map
    tool_map = {}
    for tool_def in tool_defs:
        if isinstance(tool_def, dict):
            tool_map[tool_def.get("name", "")] = tool_def.get("item_id", "")
    
    # Get max_tokens from limits
    limits = harness.limits if hasattr(harness, "limits") else {}
    max_tokens = limits.get("max_tokens", 1024)
    
    # Run LLM loop with reconstructed conversation
    try:
        llm_result = await run_llm_loop(
            project_path=project_path,
            model_id=model_id,
            provider_id=provider_id,
            provider_config=provider_config,
            tool_defs=tool_defs,
            tool_map=tool_map,
            harness=harness,
            messages=messages,
            max_tokens=max_tokens,
            directive_name=directive_name,
            thread_id=thread_id,
            transcript=transcript,
        )
    except Exception as e:
        logger.error(f"LLM loop failed: {e}")
        # Update status to error
        meta["status"] = "error"
        meta_path.write_text(json.dumps(meta, indent=2))
        raise
    
    if not llm_result.get("success"):
        # LLM call failed
        meta["status"] = "error"
        meta["error"] = llm_result.get("error", "Unknown LLM error")
        meta_path.write_text(json.dumps(meta, indent=2))
        return {
            "success": False,
            "status": "error",
            "error": llm_result.get("error"),
            "turn_count": meta.get("turn_count", 0),
            "cost": meta.get("cost", {}),
        }
    
    # Persist updated harness state
    try:
        save_state(thread_id, harness, project_path)
    except Exception as e:
        logger.error(f"Failed to save harness state: {e}")
        raise
    
    # Update metadata with turn count and cumulative cost
    try:
        meta["status"] = "paused"
        meta["awaiting"] = "user"
        meta["turn_count"] = harness.cost.turns if hasattr(harness, "cost") else meta.get("turn_count", 0) + 1
        
        # Get cumulative cost
        if hasattr(harness, "cost"):
            cost_dict = harness.cost.to_dict() if hasattr(harness.cost, "to_dict") else {
                "turns": harness.cost.turns,
                "tokens": harness.cost.tokens,
                "spend": harness.cost.spend,
            }
            meta["cost"] = cost_dict
        
        meta["updated_at"] = datetime.now(timezone.utc).isoformat()
        meta_path.write_text(json.dumps(meta, indent=2))
    except Exception as e:
        logger.error(f"Failed to update thread metadata: {e}")
        raise
    
    # Emit thread_continue event
    try:
        transcript.write_event(thread_id, "thread_continue", {
            "turn_number": meta.get("turn_count", 0),
            "cost": meta.get("cost", {}),
        })
    except Exception as e:
        logger.warning(f"Failed to write thread_continue event: {e}")
    
    return {
        "success": True,
        "status": "paused",
        "thread_id": thread_id,
        "turn_count": meta.get("turn_count", 0),
        "cost": meta.get("cost", {}),
        "text": llm_result.get("text", ""),
        "tool_results": llm_result.get("tool_results", []),
    }
