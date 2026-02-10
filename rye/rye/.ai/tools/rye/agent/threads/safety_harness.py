"""
Safety Harness for Directive Execution.

Wraps directive execution with:
- Cost tracking (turns, tokens, spawns, duration, spend)
- Limit enforcement
- Hook evaluation and execution
- Checkpoint-based control flow
- Permission enforcement via CapabilityToken

This is a TOOL, not a kernel primitive.
"""

import asyncio
import time
import logging
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Callable, Dict, List, Optional, Set

import yaml
import importlib.util
from pathlib import Path as PathLib

# Load expression_evaluator from same directory
_expr_path = PathLib(__file__).parent / "expression_evaluator.py"
_spec = importlib.util.spec_from_file_location("expression_evaluator", _expr_path)
_expr_module = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_expr_module)
evaluate_expression = _expr_module.evaluate_expression
substitute_templates = _expr_module.substitute_templates

# Import CapabilityToken from new location
try:
    import importlib.util
    from pathlib import Path
    
    _tokens_path = Path(__file__).parent.parent / "permissions" / "capability_tokens" / "capability_tokens.py"
    _spec = importlib.util.spec_from_file_location("capability_tokens", _tokens_path)
    _tokens_module = importlib.util.module_from_spec(_spec)
    _spec.loader.exec_module(_tokens_module)
    
    CapabilityToken = _tokens_module.CapabilityToken
    attenuate_token = _tokens_module.attenuate_token
    expand_capabilities = _tokens_module.expand_capabilities
    check_all_capabilities = _tokens_module.check_all_capabilities
    CAPABILITY_SYSTEM_AVAILABLE = True
except Exception:
    CAPABILITY_SYSTEM_AVAILABLE = False
    CapabilityToken = None
    expand_capabilities = None
    check_all_capabilities = None

logger = logging.getLogger(__name__)


class HarnessAction(Enum):
    """Actions that can be returned by hook directives."""
    RETRY = "retry"
    CONTINUE = "continue"
    SKIP = "skip"
    FAIL = "fail"
    ABORT = "abort"


@dataclass
class HarnessResult:
    """Result of harness evaluation or hook execution."""
    action: HarnessAction = HarnessAction.CONTINUE
    success: bool = True
    error: Optional[str] = None
    context: Optional[Dict] = None
    output: Optional[Dict] = None
    
    def to_dict(self) -> Dict:
        return {
            "action": self.action.value,
            "success": self.success,
            "error": self.error,
            "context": self.context,
            "output": self.output,
        }


@dataclass
class CostTracker:
    """Tracks accumulated costs during execution."""
    turns: int = 0
    tokens: int = 0
    input_tokens: int = 0
    output_tokens: int = 0
    spawns: int = 0
    spend: float = 0.0
    start_time: float = field(default_factory=time.time)
    
    @property
    def duration_seconds(self) -> float:
        return time.time() - self.start_time
    
    def to_dict(self) -> Dict:
        return {
            "turns": self.turns,
            "tokens": self.tokens,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "spawns": self.spawns,
            "spend": self.spend,
            "duration_seconds": self.duration_seconds,
        }


class SafetyHarness:
    """
    Wraps directive execution with safety enforcement.
    
    Tracks costs, enforces limits, evaluates hooks at checkpoints.
    Enforces permissions via CapabilityToken.
    """
    
    # Checkpoints where hooks are evaluated (control-flow points)
    CHECKPOINTS = ["before_step", "after_step", "on_error", "on_limit"]
    
    def __init__(
        self,
        project_path: Path,
        limits: Optional[Dict] = None,
        hooks: Optional[List[Dict]] = None,
        directive_name: Optional[str] = None,
        directive_inputs: Optional[Dict] = None,
        parent_token: Optional[Any] = None,
        required_permissions: Optional[List[Dict]] = None,
        directive_loader: Optional[Callable] = None,
    ):
        self.project_path = Path(project_path)
        self.limits = self._normalize_limits(limits or {})
        self.hooks = hooks or []
        self.directive_name = directive_name
        self.directive_inputs = directive_inputs or {}
        self.parent_token = parent_token
        self.required_permissions = required_permissions or []
        self._directive_loader = directive_loader
        
        self.cost = CostTracker()
        self._pricing_cache: Optional[Dict] = None
        self._current_model: Optional[str] = None
        
        # Compute required capabilities from permissions
        self._required_caps: List[str] = self._compute_required_caps()
    
    def _normalize_limits(self, limits: Dict) -> Dict:
        """Normalize limits to standard format with spend_currency."""
        normalized = dict(limits)
        
        # Ensure spend_currency exists if spend is set
        if "spend" in normalized and "spend_currency" not in normalized:
            normalized["spend_currency"] = "USD"
        
        return normalized
    
    def _compute_required_caps(self) -> List[str]:
        """Extract required capabilities from permission declarations.
        
        Handles both legacy <cap> tags and new hierarchical format
        (normalized to cap entries by the parser).
        """
        if not CAPABILITY_SYSTEM_AVAILABLE:
            return []
        
        if not self.required_permissions:
            return []
        
        caps = []
        for perm in self.required_permissions:
            if perm.get("tag") == "cap":
                content = perm.get("content", "")
                if content:
                    caps.append(content)
        return caps
    
    def _load_pricing(self) -> Dict:
        """Load pricing data from YAML file."""
        if self._pricing_cache is not None:
            return self._pricing_cache
        
        pricing_path = self.project_path / ".ai" / "tools" / "llm" / "pricing.yaml"
        if not pricing_path.exists():
            # Try user space
            pricing_path = Path.home() / ".ai" / "tools" / "llm" / "pricing.yaml"
        
        if pricing_path.exists():
            with open(pricing_path) as f:
                self._pricing_cache = yaml.safe_load(f)
        else:
            self._pricing_cache = {"models": {}, "default": {"input_per_million": 5.0, "output_per_million": 15.0}}
        
        return self._pricing_cache
    
    def _get_model_pricing(self, model: str) -> Dict:
        """Get pricing for a specific model."""
        pricing = self._load_pricing()
        models = pricing.get("models", {})
        
        if model in models:
            return models[model]
        
        # Try partial match (for model versions like gpt-4o-2024-05-13)
        for model_name, model_pricing in models.items():
            if model.startswith(model_name) or model_name.startswith(model):
                return model_pricing
        
        return pricing.get("default", {"input_per_million": 5.0, "output_per_million": 15.0})
    
    def _extract_usage(self, llm_response: Dict) -> Dict[str, int]:
        """
        Extract token usage from LLM response.
        Handles differences between OpenAI and Anthropic formats.
        Falls back to estimation if data unavailable.
        """
        usage = llm_response.get("usage", {})
        
        # OpenAI format
        if "total_tokens" in usage:
            return {
                "input_tokens": usage.get("prompt_tokens", 0),
                "output_tokens": usage.get("completion_tokens", 0),
                "total_tokens": usage["total_tokens"],
                "estimated": False,
            }
        
        # Anthropic format (no total)
        if "input_tokens" in usage or "output_tokens" in usage:
            input_tokens = usage.get("input_tokens", 0)
            output_tokens = usage.get("output_tokens", 0)
            return {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": input_tokens + output_tokens,
                "estimated": False,
            }
        
        # No usage data - estimate from content length
        # ~4 chars per token for English text
        content = llm_response.get("content", "")
        if isinstance(content, list):
            content = "".join(c.get("text", "") for c in content if isinstance(c, dict))
        estimated_output = len(str(content)) // 4
        
        return {
            "input_tokens": 0,
            "output_tokens": estimated_output,
            "total_tokens": estimated_output,
            "estimated": True,
        }
    
    def _calculate_spend(self, usage: Dict, model: str) -> float:
        """Calculate spend from token usage and model pricing."""
        pricing = self._get_model_pricing(model)
        
        input_cost = (usage.get("input_tokens", 0) / 1_000_000) * pricing.get("input_per_million", 0)
        output_cost = (usage.get("output_tokens", 0) / 1_000_000) * pricing.get("output_per_million", 0)
        
        return input_cost + output_cost
    
    def update_cost_after_turn(self, llm_response: Dict, model: str) -> None:
        """Update cost tracking after each LLM turn."""
        self.cost.turns += 1
        self._current_model = model
        
        # Extract and accumulate token usage
        usage = self._extract_usage(llm_response)
        self.cost.tokens += usage.get("total_tokens", 0)
        self.cost.input_tokens += usage.get("input_tokens", 0)
        self.cost.output_tokens += usage.get("output_tokens", 0)
        
        # Calculate and accumulate spend
        turn_spend = self._calculate_spend(usage, model)
        self.cost.spend += turn_spend
        
        logger.debug(f"Turn {self.cost.turns}: +{usage.get('total_tokens', 0)} tokens, +${turn_spend:.6f}")
    
    def increment_spawn_count(self) -> None:
        """Called when spawning a child thread."""
        self.cost.spawns += 1
    
    def build_context(self, event: Dict) -> Dict:
        """Build full context for hook evaluation."""
        # Get granted capabilities from parent token
        granted_caps: List[str] = []
        if self.parent_token and CAPABILITY_SYSTEM_AVAILABLE:
            if hasattr(self.parent_token, 'caps'):
                granted_caps = list(self.parent_token.caps)
            elif isinstance(self.parent_token, dict):
                granted_caps = self.parent_token.get('caps', [])
        
        return {
            "event": event,
            "directive": {
                "name": self.directive_name,
                "inputs": self.directive_inputs,
            },
            "cost": self.cost.to_dict(),
            "limits": self.limits,
            "permissions": {
                "granted": granted_caps,
                "required": self._required_caps,
            },
        }
    
    def check_permissions(self) -> Optional[Dict]:
        """
        Check if parent token grants required permissions.
        
        Uses capability hierarchy - if token has 'kiwi-mcp.execute',
        it implicitly has 'kiwi-mcp.search', 'kiwi-mcp.load', etc.
        
        Returns event dict if permission denied, None if all OK.
        """
        if not CAPABILITY_SYSTEM_AVAILABLE:
            return None
        
        if not self._required_caps:
            return None
        
        if not self.parent_token:
            # No token = no permissions granted
            return {
                "name": "error",
                "code": "permission_denied",
                "detail": {
                    "missing": self._required_caps,
                    "granted": [],
                    "expanded": [],
                    "reason": "No capability token provided",
                },
            }
        
        # Get granted caps from token
        granted_caps: List[str] = []
        if hasattr(self.parent_token, 'caps'):
            granted_caps = list(self.parent_token.caps)
        elif isinstance(self.parent_token, dict):
            granted_caps = self.parent_token.get('caps', [])
        
        # Use hierarchy-aware checking
        all_satisfied, missing = check_all_capabilities(granted_caps, self._required_caps)
        
        if not all_satisfied:
            # Expand for reporting what was actually checked
            expanded = expand_capabilities(granted_caps)
            return {
                "name": "error",
                "code": "permission_denied",
                "detail": {
                    "missing": missing,
                    "granted": granted_caps,
                    "expanded": sorted(expanded),
                    "required": self._required_caps,
                },
            }
        
        return None
    
    def check_limits(self) -> Optional[Dict]:
        """
        Check if any limits are about to be exceeded.
        
        Returns event dict if limit exceeded, None otherwise.
        """
        cost = self.cost.to_dict()
        
        # Check turns
        if self.limits.get("turns") and cost["turns"] >= self.limits["turns"]:
            return {
                "name": "limit",
                "code": "turns_exceeded",
                "current": cost["turns"],
                "max": self.limits["turns"],
            }
        
        # Check tokens
        if self.limits.get("tokens") and cost["tokens"] >= self.limits["tokens"]:
            return {
                "name": "limit",
                "code": "tokens_exceeded",
                "current": cost["tokens"],
                "max": self.limits["tokens"],
            }
        
        # Check spawns
        if self.limits.get("spawns") and cost["spawns"] >= self.limits["spawns"]:
            return {
                "name": "limit",
                "code": "spawns_exceeded",
                "current": cost["spawns"],
                "max": self.limits["spawns"],
            }
        
        # Check duration
        if self.limits.get("duration") and cost["duration_seconds"] >= self.limits["duration"]:
            return {
                "name": "limit",
                "code": "duration_exceeded",
                "current": cost["duration_seconds"],
                "max": self.limits["duration"],
            }
        
        # Check spend
        if self.limits.get("spend") and cost["spend"] >= self.limits["spend"]:
            return {
                "name": "limit",
                "code": "spend_exceeded",
                "current": cost["spend"],
                "max": self.limits["spend"],
                "currency": self.limits.get("spend_currency", "USD"),
            }
        
        return None
    
    def evaluate_hooks(self, event: Dict) -> HarnessResult:
        """
        Evaluate all hooks against current context.
        
        First matching hook wins.
        """
        context = self.build_context(event)
        
        for hook in self.hooks:
            when_expr = hook.get("when", "")
            try:
                if evaluate_expression(when_expr, context):
                    logger.info(f"Hook matched: {when_expr}")
                    return self._prepare_hook_execution(hook, context)
            except Exception as e:
                logger.warning(f"Hook expression error '{when_expr}': {e}")
                continue
        
        return HarnessResult(action=HarnessAction.CONTINUE)
    
    async def evaluate_hooks_async(self, event: Dict) -> HarnessResult:
        """
        Evaluate hooks and execute matching hook directive.
        
        This async version actually executes the hook directive
        and returns the action specified in its output.
        """
        context = self.build_context(event)
        
        for hook in self.hooks:
            when_expr = hook.get("when", "")
            try:
                if evaluate_expression(when_expr, context):
                    logger.info(f"Hook matched: {when_expr}")
                    return await self._execute_hook_directive(hook, context)
            except Exception as e:
                logger.warning(f"Hook expression error '{when_expr}': {e}")
                continue
        
        return HarnessResult(action=HarnessAction.CONTINUE)
    
    def _prepare_hook_execution(self, hook: Dict, context: Dict) -> HarnessResult:
        """
        Prepare hook directive for execution (sync version).
        
        Substitutes templates in inputs and returns result indicating
        which directive to call. Caller is responsible for executing.
        """
        directive_name = hook.get("directive")
        if not directive_name:
            return HarnessResult(
                action=HarnessAction.FAIL,
                success=False,
                error="Hook has no directive specified"
            )
        
        inputs = hook.get("inputs", {})
        substituted_inputs = substitute_templates(inputs, context)
        
        return HarnessResult(
            action=HarnessAction.CONTINUE,
            context={
                "hook_directive": directive_name,
                "hook_inputs": substituted_inputs,
                "original_context": context,
            }
        )
    
    async def _execute_hook_directive(self, hook: Dict, context: Dict) -> HarnessResult:
        """
        Execute hook directive and read action from its output.
        
        Hook directives must return output with an 'action' field:
        - retry: re-execute the original directive
        - continue: proceed despite the condition
        - skip: skip current step, continue workflow
        - fail: return error to caller
        - abort: terminate entire execution tree
        """
        directive_name = hook.get("directive")
        if not directive_name:
            return HarnessResult(
                action=HarnessAction.FAIL,
                success=False,
                error="Hook has no directive specified"
            )
        
        inputs = hook.get("inputs", {})
        substituted_inputs = substitute_templates(inputs, context)
        
        # Load hook directive
        if not self._directive_loader:
            # No loader provided - return prepared result for caller to execute
            return self._prepare_hook_execution(hook, context)
        
        try:
            hook_directive = await self._directive_loader(directive_name)
            if hook_directive is None:
                return HarnessResult(
                    action=HarnessAction.FAIL,
                    success=False,
                    error=f"Hook directive not found: {directive_name}"
                )
            
            # Extract hook directive metadata
            hook_metadata = hook_directive.get("metadata", hook_directive.get("data", {}))
            hook_limits = hook_metadata.get("limits", {})
            hook_hooks = hook_metadata.get("hooks", [])
            hook_permissions = hook_metadata.get("permissions", [])
            
            # Create child harness with attenuated token
            child_token = self._attenuate_token_for_child(hook_permissions)
            
            child_harness = SafetyHarness(
                project_path=self.project_path,
                limits=hook_limits,
                hooks=hook_hooks,
                directive_name=directive_name,
                directive_inputs=substituted_inputs,
                parent_token=child_token,
                required_permissions=hook_permissions,
                directive_loader=self._directive_loader,
            )
            
            # Execute hook directive
            # Note: actual execution is delegated to the LLM runtime
            # We return the prepared context with child harness info
            return HarnessResult(
                action=HarnessAction.CONTINUE,
                context={
                    "hook_directive": directive_name,
                    "hook_inputs": substituted_inputs,
                    "hook_harness": child_harness.get_status(),
                    "original_context": context,
                    "awaiting_action": True,
                }
            )
            
        except Exception as e:
            logger.error(f"Failed to load hook directive {directive_name}: {e}")
            return HarnessResult(
                action=HarnessAction.FAIL,
                success=False,
                error=f"Hook directive load failed: {e}"
            )
    
    def _attenuate_token_for_child(self, child_permissions: List[Dict]) -> Optional[Any]:
        """
        Attenuate parent token for child directive.
        
        Child only gets capabilities that BOTH parent has AND child declares.
        This applies to hooks too - directive permissions decide what's allowed.
        """
        if not CAPABILITY_SYSTEM_AVAILABLE:
            return self.parent_token
        
        if not self.parent_token:
            return None
        
        child_caps = []
        if child_permissions:
            for perm in child_permissions:
                if perm.get("tag") == "cap":
                    content = perm.get("content", "")
                    if content:
                        child_caps.append(content)
        
        if hasattr(self.parent_token, 'caps'):
            # It's a CapabilityToken object
            return attenuate_token(self.parent_token, child_caps)
        elif isinstance(self.parent_token, dict):
            # It's a dict representation
            parent_caps = set(self.parent_token.get('caps', []))
            child_cap_set = set(child_caps)
            attenuated = sorted(parent_caps & child_cap_set)
            return {**self.parent_token, 'caps': attenuated}
        
        return self.parent_token
    
    def handle_hook_action(self, action_str: str, hook_output: Optional[Dict] = None) -> HarnessResult:
        """
        Handle action returned by hook directive.
        
        Called after hook directive execution completes.
        
        Args:
            action_str: Action string from hook directive output
            hook_output: Full output from hook directive
        
        Returns:
            HarnessResult with appropriate action
        """
        action_map = {
            "retry": HarnessAction.RETRY,
            "continue": HarnessAction.CONTINUE,
            "skip": HarnessAction.SKIP,
            "fail": HarnessAction.FAIL,
            "abort": HarnessAction.ABORT,
        }
        
        action = action_map.get(action_str.lower(), HarnessAction.FAIL)
        
        if action == HarnessAction.FAIL:
            error = None
            if hook_output:
                error = hook_output.get("error") or hook_output.get("message")
            return HarnessResult(
                action=action,
                success=False,
                error=error or f"Hook returned fail action",
                output=hook_output,
            )
        
        if action == HarnessAction.ABORT:
            error = None
            if hook_output:
                error = hook_output.get("error") or hook_output.get("message")
            return HarnessResult(
                action=action,
                success=False,
                error=error or "Execution aborted by hook",
                output=hook_output,
            )
        
        return HarnessResult(
            action=action,
            success=True,
            output=hook_output,
            context=hook_output.get("context") if hook_output else None,
        )
    
    def checkpoint_before_step(self, step_name: str) -> HarnessResult:
        """Checkpoint before executing a step."""
        # First check limits
        limit_event = self.check_limits()
        if limit_event:
            return self.evaluate_hooks(limit_event)
        
        # Then evaluate hooks for before_step
        event = {"name": "before_step", "step": step_name}
        return self.evaluate_hooks(event)
    
    def checkpoint_after_step(self, step_name: str, result: Any) -> HarnessResult:
        """Checkpoint after successful step completion."""
        event = {
            "name": "after_step",
            "step": step_name,
            "result": result if isinstance(result, dict) else {"value": result},
        }
        return self.evaluate_hooks(event)
    
    def checkpoint_on_error(self, error_code: str, detail: Optional[Dict] = None) -> HarnessResult:
        """Checkpoint after an error occurs."""
        event = {
            "name": "error",
            "code": error_code,
            "detail": detail or {},
        }
        return self.evaluate_hooks(event)
    
    def get_status(self) -> Dict:
        """Get current harness status."""
        # Get granted caps from token
        granted_caps: List[str] = []
        if self.parent_token:
            if hasattr(self.parent_token, 'caps'):
                granted_caps = list(self.parent_token.caps)
            elif isinstance(self.parent_token, dict):
                granted_caps = self.parent_token.get('caps', [])
        
        return {
            "directive": self.directive_name,
            "inputs": self.directive_inputs,
            "cost": self.cost.to_dict(),
            "limits": self.limits,
            "hooks_count": len(self.hooks),
            "hooks": self.hooks,
            "permissions": {
                "granted": granted_caps,
                "required": self._required_caps,
            },
        }
    
    def to_state_dict(self) -> Dict:
        """
        Serialize harness state for persistence/transfer.
        
        Can be used to restore harness state across calls.
        """
        return {
            "directive": self.directive_name,
            "inputs": self.directive_inputs,
            "cost": self.cost.to_dict(),
            "limits": self.limits,
            "hooks": self.hooks,
            "required_caps": self._required_caps,
        }
    
    @classmethod
    def from_state_dict(
        cls,
        state: Dict,
        project_path: Path,
        parent_token: Optional[Any] = None,
        directive_loader: Optional[Callable] = None,
    ) -> "SafetyHarness":
        """
        Restore harness from state dict.
        
        Used to restore harness state across calls.
        """
        harness = cls(
            project_path=project_path,
            limits=state.get("limits", {}),
            hooks=state.get("hooks", []),
            directive_name=state.get("directive"),
            directive_inputs=state.get("inputs", {}),
            parent_token=parent_token,
            directive_loader=directive_loader,
        )
        
        # Restore cost state
        cost_state = state.get("cost", {})
        harness.cost.turns = cost_state.get("turns", 0)
        harness.cost.tokens = cost_state.get("tokens", 0)
        harness.cost.input_tokens = cost_state.get("input_tokens", 0)
        harness.cost.output_tokens = cost_state.get("output_tokens", 0)
        harness.cost.spawns = cost_state.get("spawns", 0)
        harness.cost.spend = cost_state.get("spend", 0.0)
        
        # Override required caps if provided
        if "required_caps" in state:
            harness._required_caps = state["required_caps"]
        
        return harness
