"""
Thread Directive Tool.

User-facing tool that spawns a thread and executes a directive
with full SafetyHarness enforcement.

This is the primary entry point for running directives with:
- Cost tracking and limits
- Permission enforcement via CapabilityToken
- Hook-based error handling
- Checkpoint-based control flow
"""

import logging
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional

import importlib.util
from pathlib import Path as PathLib

# Load safety_harness from same directory
_harness_path = PathLib(__file__).parent / "safety_harness.py"
_spec = importlib.util.spec_from_file_location("safety_harness", _harness_path)
_harness_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_harness_module)
SafetyHarness = _harness_module.SafetyHarness
HarnessAction = _harness_module.HarnessAction
HarnessResult = _harness_module.HarnessResult

logger = logging.getLogger(__name__)

# Tool metadata
__version__ = "1.1.0"
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "threads"


async def execute(
    directive_name: str,
    inputs: Optional[Dict] = None,
    project_path: Optional[str] = None,
    **params
) -> Dict[str, Any]:
    """
    Spawn a thread and execute a directive with harness enforcement.
    
    Args:
        directive_name: Name of the directive to execute
        inputs: Input parameters for the directive
        project_path: Path to the project root
        **params: Additional parameters (e.g., _token for capability token)
    
    Returns:
        Dict with execution result:
        {
            "status": "ready" | "failed" | "hook_triggered" | "permission_denied",
            "result": <directive output>,
            "cost": {turns, tokens, spend, ...},
            "hook": {directive, inputs} if hook was triggered
        }
    """
    if project_path is None:
        project_path = Path.cwd()
    else:
        project_path = Path(project_path)
    
    # Load directive
    directive = await _load_directive(directive_name, project_path)
    if directive is None:
        return {
            "status": "failed",
            "error": f"Directive not found: {directive_name}",
        }
    
    # Extract metadata
    metadata = directive.get("metadata", directive.get("data", {}))
    limits = metadata.get("limits", {})
    hooks = metadata.get("hooks", [])
    model_config = metadata.get("model", {})
    permissions = metadata.get("permissions", [])
    
    # Create directive loader for hook execution
    async def directive_loader(name: str) -> Optional[Dict]:
        return await _load_directive(name, project_path)
    
    # Create harness
    harness = SafetyHarness(
        project_path=project_path,
        limits=limits,
        hooks=hooks,
        directive_name=directive_name,
        directive_inputs=inputs or {},
        parent_token=params.get("_token"),
        required_permissions=permissions,
        directive_loader=directive_loader,
    )
    
    # Check permissions first
    perm_event = harness.check_permissions()
    if perm_event:
        hook_result = harness.evaluate_hooks(perm_event)
        if hook_result.context and "hook_directive" in hook_result.context:
            return {
                "status": "hook_triggered",
                "hook": {
                    "directive": hook_result.context["hook_directive"],
                    "inputs": hook_result.context["hook_inputs"],
                },
                "event": perm_event,
                "cost": harness.cost.to_dict(),
            }
        # No hook matched - return permission denied
        return {
            "status": "permission_denied",
            "error": perm_event,
            "cost": harness.cost.to_dict(),
        }
    
    # Check limits before starting
    limit_event = harness.check_limits()
    if limit_event:
        hook_result = harness.evaluate_hooks(limit_event)
        if hook_result.context and "hook_directive" in hook_result.context:
            return {
                "status": "hook_triggered",
                "hook": {
                    "directive": hook_result.context["hook_directive"],
                    "inputs": hook_result.context["hook_inputs"],
                },
                "event": limit_event,
                "cost": harness.cost.to_dict(),
            }
    
    # Return harness info for the LLM to use during execution
    # The actual execution is handled by the LLM following the directive content
    return {
        "status": "ready",
        "directive": {
            "name": directive_name,
            "content": directive.get("content", ""),
            "inputs": inputs or {},
        },
        "harness": harness.get_status(),
        "harness_state": harness.to_state_dict(),
        "model": model_config,
        "instruction": directive.get("instruction", "execute directive now"),
    }


async def update_turn(
    harness_state: Dict,
    llm_response: Dict,
    model: str,
    project_path: Optional[str] = None,
    parent_token: Optional[Any] = None,
) -> Dict[str, Any]:
    """
    Update harness after an LLM turn.
    
    Called by the thread tool after each LLM interaction.
    
    Args:
        harness_state: Current harness state from previous call
        llm_response: LLM response with usage data
        model: Model identifier used
        project_path: Path to project root
        parent_token: Capability token for permission checking
    
    Returns:
        Updated harness state with any triggered hooks
    """
    if project_path is None:
        project_path = Path.cwd()
    else:
        project_path = Path(project_path)
    
    # Create directive loader for hook execution
    async def directive_loader(name: str) -> Optional[Dict]:
        return await _load_directive(name, project_path)
    
    # Reconstruct harness from state
    harness = SafetyHarness.from_state_dict(
        state=harness_state,
        project_path=project_path,
        parent_token=parent_token,
        directive_loader=directive_loader,
    )
    
    # Update with new turn
    harness.update_cost_after_turn(llm_response, model)
    
    # Check limits
    limit_event = harness.check_limits()
    if limit_event:
        hook_result = harness.evaluate_hooks(limit_event)
        if hook_result.context and "hook_directive" in hook_result.context:
            return {
                "status": "hook_triggered",
                "hook": {
                    "directive": hook_result.context["hook_directive"],
                    "inputs": hook_result.context["hook_inputs"],
                },
                "event": limit_event,
                "harness_state": harness.to_state_dict(),
            }
    
    return {
        "status": "continue",
        "harness_state": harness.to_state_dict(),
    }


async def handle_error(
    harness_state: Dict,
    error_code: str,
    error_detail: Optional[Dict] = None,
    project_path: Optional[str] = None,
    parent_token: Optional[Any] = None,
) -> Dict[str, Any]:
    """
    Handle an error during directive execution.
    
    Args:
        harness_state: Current harness state
        error_code: Error code (e.g., "permission_denied", "timeout")
        error_detail: Additional error details
        project_path: Path to project root
        parent_token: Capability token for permission checking
    
    Returns:
        Hook response if a hook matches, or default error response
    """
    if project_path is None:
        project_path = Path.cwd()
    else:
        project_path = Path(project_path)
    
    # Create directive loader for hook execution
    async def directive_loader(name: str) -> Optional[Dict]:
        return await _load_directive(name, project_path)
    
    # Reconstruct harness from state
    harness = SafetyHarness.from_state_dict(
        state=harness_state,
        project_path=project_path,
        parent_token=parent_token,
        directive_loader=directive_loader,
    )
    
    # Evaluate hooks for this error
    result = harness.checkpoint_on_error(error_code, error_detail)
    
    if result.context and "hook_directive" in result.context:
        return {
            "status": "hook_triggered",
            "hook": {
                "directive": result.context["hook_directive"],
                "inputs": result.context["hook_inputs"],
            },
            "error": {"code": error_code, "detail": error_detail},
            "harness_state": harness.to_state_dict(),
        }
    
    return {
        "status": "error",
        "error": {"code": error_code, "detail": error_detail},
        "action": "fail",
        "harness_state": harness.to_state_dict(),
    }


async def handle_hook_result(
    harness_state: Dict,
    hook_output: Dict,
    project_path: Optional[str] = None,
    parent_token: Optional[Any] = None,
) -> Dict[str, Any]:
    """
    Handle the result of a hook directive execution.
    
    Called after a hook directive has completed to determine next action.
    
    Args:
        harness_state: Current harness state
        hook_output: Output from the hook directive (must contain 'action')
        project_path: Path to project root
        parent_token: Capability token
    
    Returns:
        Dict with action to take and updated harness state
    """
    if project_path is None:
        project_path = Path.cwd()
    else:
        project_path = Path(project_path)
    
    # Create directive loader for potential nested hook execution
    async def directive_loader(name: str) -> Optional[Dict]:
        return await _load_directive(name, project_path)
    
    # Reconstruct harness from state
    harness = SafetyHarness.from_state_dict(
        state=harness_state,
        project_path=project_path,
        parent_token=parent_token,
        directive_loader=directive_loader,
    )
    
    # Get action from hook output
    action_str = hook_output.get("action", "fail")
    
    # Handle the action
    result = harness.handle_hook_action(action_str, hook_output)
    
    return {
        "status": result.action.value,
        "success": result.success,
        "error": result.error,
        "output": result.output,
        "harness_state": harness.to_state_dict(),
    }


async def _load_directive(name: str, project_path: Path) -> Optional[Dict]:
    """
    Load directive using the kiwi-mcp handler.
    
    Args:
        name: Directive name
        project_path: Path to project root
    
    Returns:
        Parsed directive dict or None if not found
    """
    try:
        from rye.handlers.directive.handler import DirectiveHandler
        handler = DirectiveHandler(str(project_path))
        file_path = handler.resolve(name)
        if file_path:
            result = handler.parse(file_path)
            return result
        else:
            return None
    except Exception as e:
        logger.error(f"Failed to load directive {name}: {e}")
        return None


async def _fallback_load_directive(name: str, project_path: Path) -> Optional[Dict]:
    """Fallback directive loader when kiwi_mcp is not available."""
    import glob
    
    # Search for directive file
    patterns = [
        project_path / ".ai" / "directives" / f"{name}.md",
        project_path / ".ai" / "directives" / "**" / f"{name}.md",
    ]
    
    for pattern in patterns:
        matches = glob.glob(str(pattern), recursive=True)
        if matches:
            # Parse the first match
            from pathlib import Path as P
            file_path = P(matches[0])
            
            # Load parser
            parser_path = project_path / ".ai" / "parsers" / "markdown_xml.py"
            if parser_path.exists():
                spec = importlib.util.spec_from_file_location("md_xml_parser", parser_path)
                parser = importlib.util.module_from_spec(spec)
                spec.loader.exec_module(parser)
                
                content = file_path.read_text()
                return parser.parse(content)
    
    return None
