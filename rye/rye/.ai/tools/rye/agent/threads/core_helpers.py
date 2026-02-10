"""
Core helper modules for agent threads.

Phase 1 implementation: extracted from thread_directive.py for reusability.
- run_llm_loop: Execute LLM tool-use loop with pre-built messages
- save_state: Persist harness state atomically
- rebuild_conversation_from_transcript: Reconstruct messages from transcript

These are used by both initial thread execution and conversation continuation.
"""

import json
import logging
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)


def save_state(thread_id: str, harness: Any, project_path: Path) -> None:
    """
    Persist harness state to state.json atomically.
    
    Uses tmp -> rename pattern for atomicity. State includes cost tracking,
    limits, permissions, and execution context needed to resume a thread.
    
    Args:
        thread_id: Thread identifier
        harness: SafetyHarness instance with to_state_dict() method
        project_path: Project root path
        
    Raises:
        Exception: If write fails (tmp file not cleaned up on error)
    """
    state_path = project_path / ".ai" / "threads" / thread_id / "state.json"
    state_path.parent.mkdir(parents=True, exist_ok=True)
    
    tmp_path = state_path.with_suffix(".json.tmp")
    
    try:
        # Serialize harness state
        state_dict = harness.to_state_dict()
        with open(tmp_path, "w", encoding="utf-8") as f:
            json.dump(state_dict, f, indent=2)
        
        # Atomic rename
        tmp_path.rename(state_path)
        logger.debug(f"Saved harness state to {state_path}")
    except Exception as e:
        if tmp_path.exists():
            tmp_path.unlink()
        logger.error(f"Failed to save harness state: {e}")
        raise


def rebuild_conversation_from_transcript(
    thread_id: str,
    project_path: Path,
    provider_config: Dict,
) -> List[Dict]:
    """
    Reconstruct LLM conversation messages from transcript.jsonl.
    
    Reads the JSONL transcript and rebuilds provider-specific message format
    based on provider_config['message_reconstruction'] mappings. This enables
    conversation continuation without hardcoding provider details.
    
    Message reconstruction is DATA-DRIVEN:
    - Provider YAML defines field mappings (e.g., tool_use_id_field: "id")
    - This function uses those mappings to convert transcript events → messages
    - Different providers (Anthropic, OpenAI, etc.) define their own mappings
    
    Args:
        thread_id: Thread to reconstruct
        project_path: Project root
        provider_config: Loaded provider YAML dict with 'message_reconstruction' section
        
    Returns:
        List of messages in provider-specific format (e.g., Anthropic format)
        
    Raises:
        ValueError: If provider_config is missing 'message_reconstruction'
        FileNotFoundError: If transcript.jsonl doesn't exist
    """
    if "message_reconstruction" not in provider_config:
        raise ValueError(
            "Provider config missing 'message_reconstruction' section. "
            "Every provider MUST declare how transcript events are reconstructed "
            "into provider-specific message formats. "
            f"Provider: {provider_config.get('tool_id', 'unknown')}"
        )
    
    transcript_path = project_path / ".ai" / "threads" / thread_id / "transcript.jsonl"
    if not transcript_path.exists():
        raise FileNotFoundError(f"Transcript not found: {transcript_path}")
    
    recon_config = provider_config["message_reconstruction"]
    messages = []
    current_role = None
    current_blocks = []
    
    # Read transcript events and group by message role transitions
    try:
        with open(transcript_path, "r", encoding="utf-8") as f:
            for line in f:
                if not line.strip():
                    continue
                
                event = json.loads(line)
                event_type = event.get("type")
                
                # Handle different event types
                if event_type == "user_message":
                    # Flush previous message if exists
                    if current_role and current_blocks:
                        messages.append({
                            "role": current_role,
                            "content": current_blocks if current_role == "assistant" else 
                                     current_blocks[0] if current_blocks else ""
                        })
                        current_blocks = []
                    
                    # Start new user message
                    current_role = "user"
                    current_blocks = [event.get("text", "")]
                
                elif event_type == "tool_call_start":
                    # Switch to assistant role if needed
                    if current_role != "assistant":
                        if current_role and current_blocks:
                            messages.append({
                                "role": current_role,
                                "content": current_blocks[0] if isinstance(current_blocks, list) else current_blocks
                            })
                        current_role = "assistant"
                        current_blocks = []
                    
                    # Build tool_call block from config mapping
                    tool_call_config = recon_config.get("tool_call", {})
                    content_block = tool_call_config.get("content_block", {})
                    
                    block = {
                        "type": content_block.get("type", "tool_use"),
                        content_block.get("id_field", "id"): event.get("call_id", ""),
                        content_block.get("name_field", "name"): event.get("tool", ""),
                        content_block.get("input_field", "input"): event.get("input", {}),
                    }
                    current_blocks.append(block)
                
                elif event_type == "tool_call_result":
                    # Switch to user role for tool result
                    if current_role != "user":
                        if current_role and current_blocks:
                            messages.append({
                                "role": current_role,
                                "content": current_blocks
                            })
                        current_role = "user"
                        current_blocks = []
                    
                    # Build tool_result block from config mapping
                    tool_result_config = recon_config.get("tool_result", {})
                    content_block = tool_result_config.get("content_block", {})
                    
                    block = {
                        "type": content_block.get("type", "tool_result"),
                        content_block.get("id_target", "tool_use_id"): event.get("call_id", ""),
                        content_block.get("content_field", "content"): event.get("output", ""),
                    }
                    
                    # Handle error field if present
                    if event.get("error"):
                        block[content_block.get("error_target", "is_error")] = True
                    
                    current_blocks.append(block)
                
                elif event_type == "assistant_text":
                    # Text content from assistant
                    if current_role != "assistant":
                        if current_role and current_blocks:
                            messages.append({
                                "role": current_role,
                                "content": current_blocks[0] if isinstance(current_blocks, list) else current_blocks
                            })
                        current_role = "assistant"
                        current_blocks = []
                    
                    current_blocks.append(event.get("text", ""))
        
        # Flush final message
        if current_role and current_blocks:
            messages.append({
                "role": current_role,
                "content": current_blocks if current_role == "assistant" and isinstance(current_blocks, list) 
                         else (current_blocks[0] if isinstance(current_blocks, list) else current_blocks)
            })
    
    except json.JSONDecodeError as e:
        logger.error(f"Failed to parse transcript.jsonl: {e}")
        raise ValueError(f"Malformed transcript: {e}")
    
    return messages


async def run_llm_loop(
    *,
    project_path: Path,
    model_id: str,
    provider_id: str,
    provider_config: Dict,
    tool_defs: List[Dict],
    tool_map: Dict[str, str],
    harness: Any,  # SafetyHarness type
    messages: List[Dict],
    max_tokens: int = 1024,
    directive_name: str = "",
    thread_id: str = "",
    transcript: Optional[Any] = None,
) -> Dict[str, Any]:
    """
    Run LLM tool-use loop with pre-built messages.
    
    This is the core execution loop extracted from thread_directive._run_tool_use_loop.
    It accepts messages directly instead of building them from system/user prompts,
    enabling both initial execution and conversation continuation.
    
    The function:
    1. Calls LLM with current messages and available tools
    2. Extracts response (text, tool calls, cost)
    3. Executes any tool calls
    4. Appends tool results to messages
    5. Repeats until LLM signals end-of-turn or max roundtrips reached
    6. Updates harness cost tracking and emits transcript events
    
    Args:
        project_path: Project root for tool execution
        model_id: LLM model identifier
        provider_id: Provider identifier (e.g., "rye/agent/providers/anthropic_messages")
        provider_config: Provider YAML config dict with tool_use, response parsing, etc.
        tool_defs: Available tool definitions
        tool_map: Mapping of tool name → tool item_id
        harness: SafetyHarness instance (tracks cost, limits, permissions)
        messages: Pre-built message history (for initial: [{"role": "user", "content": ...}],
                  for continuation: complete conversation history)
        max_tokens: Max tokens per LLM call
        directive_name: Current directive name (for logging/transcripts)
        thread_id: Thread ID (for logging/transcripts)
        transcript: TranscriptWriter instance for event logging (optional)
        
    Returns:
        Dict with keys:
        - success: bool
        - text: final assistant text
        - usage: token counts {prompt_tokens, completion_tokens, ...}
        - model: actual model used
        - raw: raw response object
        - tool_results: list of executed tool results
        
    Raises:
        Exception: If LLM call fails (unless caught and returned in result)
    """
    # Placeholder implementation — will call helper functions below
    # This is refactored from thread_directive._run_tool_use_loop
    
    from rye.rye._ai.tools.rye.agent.threads.thread_directive import (
        _format_tool_defs,
        _call_llm,
        _extract_response,
        _execute_tool_call,
        _build_tool_result_message,
        MAX_TOOL_ROUNDTRIPS,
    )
    
    formatted_tools = _format_tool_defs(tool_defs, provider_config) if tool_defs else []
    
    all_tool_results = []
    final_text = ""
    
    # Emit user message event (only for initial turn if not already in messages)
    if transcript and thread_id and not any(m.get("role") == "user" and "directive" not in m for m in messages):
        try:
            user_text = next(
                (m.get("content", "") for m in messages if m.get("role") == "user"),
                ""
            )
            if user_text:
                transcript.write_event(thread_id, "user_message", {
                    "directive": directive_name,
                    "text": user_text,
                    "role": "user",
                })
        except Exception as e:
            logger.warning(f"Failed to write user_message event: {e}")
    
    for turn in range(MAX_TOOL_ROUNDTRIPS):
        # Emit step_start event
        if transcript and thread_id:
            try:
                transcript.write_event(thread_id, "step_start", {
                    "directive": directive_name,
                    "turn_number": turn + 1,
                })
            except Exception as e:
                logger.warning(f"Failed to write step_start event: {e}")
        
        # Call LLM
        llm_result = await _call_llm(
            project_path=project_path,
            model=model_id,
            system_prompt="",  # Already in messages for conversation continuation
            messages=messages,
            max_tokens=max_tokens,
            provider_id=provider_id,
            tools=formatted_tools if formatted_tools else None,
        )
        
        if not llm_result["success"]:
            return llm_result
        
        raw = llm_result["raw"]
        parsed = _extract_response(raw, provider_config)
        
        # Update cost tracking
        harness.update_cost_after_turn({"usage": parsed["usage"]}, parsed["model"] or model_id)
        
        # Emit assistant_text event
        if transcript and thread_id and parsed.get("text"):
            try:
                transcript.write_event(thread_id, "assistant_text", {
                    "directive": directive_name,
                    "text": parsed["text"],
                })
            except Exception as e:
                logger.warning(f"Failed to write assistant_text event: {e}")
        
        # Emit assistant_reasoning if available
        if transcript and thread_id and parsed.get("reasoning"):
            try:
                transcript.write_event(thread_id, "assistant_reasoning", {
                    "directive": directive_name,
                    "text": parsed["reasoning"],
                })
            except Exception as e:
                logger.warning(f"Failed to write assistant_reasoning event: {e}")
        
        # Check limits
        limit_event = harness.check_limits()
        if limit_event:
            harness.evaluate_hooks(limit_event)
        
        # Append assistant response to messages
        messages.append({"role": "assistant", "content": parsed["content_blocks"]})
        
        # Check if LLM wants to continue tool loop
        if not parsed["has_tool_use"] or not parsed["tool_calls"]:
            final_text = parsed["text"]
            
            # Emit step_finish event
            if transcript and thread_id:
                try:
                    step_cost = harness.cost.per_turn[-1]["spend"] if harness.cost.per_turn else 0
                    step_tokens = (harness.cost.per_turn[-1].get("prompt_tokens", 0) + 
                                 harness.cost.per_turn[-1].get("completion_tokens", 0)) if harness.cost.per_turn else 0
                    transcript.write_event(thread_id, "step_finish", {
                        "directive": directive_name,
                        "cost": step_cost,
                        "tokens": step_tokens,
                        "finish_reason": "end_turn",
                    })
                except Exception as e:
                    logger.warning(f"Failed to write step_finish event: {e}")
            break
        
        # Execute tool calls
        tool_results = []
        for tc in parsed["tool_calls"]:
            call_id = tc.get("id", "")
            
            # Emit tool_call_start
            if transcript and thread_id:
                try:
                    transcript.write_event(thread_id, "tool_call_start", {
                        "directive": directive_name,
                        "tool": tc["name"],
                        "call_id": call_id,
                        "input": tc["input"],
                    })
                except Exception as e:
                    logger.warning(f"Failed to write tool_call_start event: {e}")
            
            # Execute tool
            start_time = time.time()
            tr = await _execute_tool_call(tc, tool_map, project_path)
            duration_ms = int((time.time() - start_time) * 1000)
            
            # Emit tool_call_result
            if transcript and thread_id:
                try:
                    error_str = tr.get("error") if tr.get("is_error") else None
                    transcript.write_event(thread_id, "tool_call_result", {
                        "directive": directive_name,
                        "call_id": call_id,
                        "output": str(tr.get("result", ""))[:1000],
                        "error": error_str,
                        "duration_ms": duration_ms,
                    })
                except Exception as e:
                    logger.warning(f"Failed to write tool_call_result event: {e}")
            
            tool_results.append(tr)
            all_tool_results.append({
                "tool": tc["name"],
                "input": tc["input"],
                "result": tr["result"],
                "is_error": tr.get("is_error", False),
            })
        
        # Emit step_finish event
        if transcript and thread_id:
            try:
                step_cost = harness.cost.per_turn[-1]["spend"] if harness.cost.per_turn else 0
                step_tokens = (harness.cost.per_turn[-1].get("prompt_tokens", 0) + 
                             harness.cost.per_turn[-1].get("completion_tokens", 0)) if harness.cost.per_turn else 0
                transcript.write_event(thread_id, "step_finish", {
                    "directive": directive_name,
                    "cost": step_cost,
                    "tokens": step_tokens,
                    "finish_reason": "tool_use",
                })
            except Exception as e:
                logger.warning(f"Failed to write step_finish event: {e}")
        
        # Append tool results to messages for next turn
        result_msg = _build_tool_result_message(tool_results, provider_config)
        messages.append(result_msg)
    else:
        # Hit max roundtrips
        final_text = parsed.get("text", "")
    
    return {
        "success": True,
        "text": final_text,
        "usage": parsed["usage"],
        "model": parsed["model"] or model_id,
        "raw": raw,
        "tool_results": all_tool_results,
    }
