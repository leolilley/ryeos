# rye:signed:2026-02-23T11:53:19Z:e99f7aad39c7d7305af6fc17b9eb3f2eda56f2d59d9993bc9b04e5b7b32c1904:YZzpCaGeXnsr62j9K5QwpHxJ7araODnfS8eNJl5B8UdWjRPnkSD2qJsRghlYcLoY7XVkRNWKoy-cpjfuBg4MDw==:9fbfabe975fa5a7f
"""
safety_harness.py: Thread safety harness — limits, hooks, cancellation, permissions
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads"
__tool_description__ = "Thread safety harness — limits, hooks, cancellation, permissions"

import fnmatch
import re
from pathlib import Path
from typing import Any, Dict, List, Optional

from module_loader import load_module

_ANCHOR = Path(__file__).parent

condition_evaluator = load_module("loaders/condition_evaluator", anchor=_ANCHOR)
interpolation = load_module("loaders/interpolation", anchor=_ANCHOR)


class SafetyHarness:
    """Manages thread limits, hooks, cancellation, and permission enforcement.

    NOT an execution engine — it checks limits, evaluates hook conditions,
    and enforces directive permissions on tool calls.

    Permissions are declared in directive XML as capability strings:
        rye.<primary>.<item_type>.<item_id_dotted>
    Example: rye.execute.tool.rye.file-system.* allows executing any
    tool under rye/file-system/.

    Two hook dispatch methods:
      - run_hooks()         — for error/limit/after_step events. Returns control action or None.
      - run_hooks_context() — for thread_started/thread_continued. Returns concatenated context string.
    """

    def __init__(
        self,
        thread_id: str,
        limits: Dict,
        hooks: List[Dict],
        project_path: Path,
        directive_name: str = "",
        permissions: Optional[List[Dict]] = None,
        parent_capabilities: Optional[List[str]] = None,
    ):
        self.thread_id = thread_id
        self.limits = limits
        self.hooks = hooks
        self.project_path = project_path
        self.directive_name = directive_name
        self._cancelled = False
        self.available_tools: List[Dict] = []

        child_caps = []
        if permissions:
            child_caps = [
                p["content"].replace("/", ".") for p in permissions if p.get("tag") == "cap"
            ]

        parent_caps = [c.replace("/", ".") for c in parent_capabilities] if parent_capabilities else []

        if child_caps and parent_caps:
            # Attenuate: capabilities only narrow down the hierarchy.
            # A child cap is kept if a parent cap covers it (parent as pattern).
            # If the child is broader than the parent, narrow to parent's scope.
            attenuated = []
            for cc in child_caps:
                for pc in parent_caps:
                    if fnmatch.fnmatch(cc, pc):
                        # Parent covers child exactly, or child is narrower
                        attenuated.append(cc)
                        break
                    elif fnmatch.fnmatch(pc, cc):
                        # Child is broader — narrow to parent's scope
                        attenuated.append(pc)
                        break
            self._capabilities = attenuated
        elif child_caps:
            self._capabilities = child_caps
        elif parent_caps:
            self._capabilities = parent_caps
        else:
            self._capabilities = []

    def check_permission(self, primary: str, item_type: str, item_id: str = "") -> Optional[Dict]:
        """Check if an action is permitted by directive capabilities.

        Returns None if allowed, or an error dict if denied.

        If no capabilities are declared, all actions are denied (fail-closed).
        Internal thread tools (rye/agent/threads/internal/*) are always allowed.

        Capability format depends on the primary action:
          execute/load/sign: rye.<primary>.<item_type>.<item_id_dotted>
          search:            rye.search.<item_type>

        Item IDs use / separators, capabilities use . separators with fnmatch wildcards.
        Example: capability "rye.execute.tool.rye.file-system.*"
                 matches item_id "rye/file-system/fs_write"
        """
        if item_id and item_id.startswith("rye/agent/threads/internal/"):
            return None

        if not self._capabilities:
            target = item_id or item_type
            return {
                "error": f"Permission denied: no capabilities declared. "
                f"Cannot {primary} {item_type} '{target}'",
                "denied_action": primary,
                "denied_item_type": item_type,
                "denied_item_id": item_id,
            }

        # Build the capability string to check
        if item_id:
            item_id_dotted = item_id.replace("/", ".")
            required = f"rye.{primary}.{item_type}.{item_id_dotted}"
        else:
            # search has no item_id — check rye.search.<item_type>
            required = f"rye.{primary}.{item_type}"

        for cap in self._capabilities:
            if fnmatch.fnmatch(required, cap):
                return None

        return {
            "error": f"Permission denied: '{required}' not covered by "
            f"capabilities {self._capabilities}",
            "denied_action": primary,
            "denied_item_type": item_type,
            "denied_item_id": item_id,
        }

    def check_limits(self, cost: Dict) -> Optional[Dict]:
        """Check all limits against current cost. Returns limit event or None."""
        checks = [
            ("turns", cost.get("turns", 0), self.limits.get("turns")),
            (
                "tokens",
                cost.get("input_tokens", 0) + cost.get("output_tokens", 0),
                self.limits.get("tokens"),
            ),
            ("spend", cost.get("spend", 0.0), self.limits.get("spend")),
            ("duration_seconds", cost.get("elapsed_seconds", 0), self.limits.get("duration_seconds")),
        ]
        for limit_code, current, maximum in checks:
            if maximum is not None and current >= maximum:
                return {
                    "limit_code": f"{limit_code}_exceeded",
                    "current_value": current,
                    "current_max": maximum,
                }
        return None

    async def run_hooks(
        self,
        event: str,
        context: Dict,
        dispatcher: Any,
        thread_context: Dict,
    ) -> Optional[Dict]:
        """Evaluate hooks for error/limit/after_step events.

        Hook evaluation order: layer 1 (directive) → layer 2 (builtin) → layer 3 (infra).
        First hook action that returns a non-None result wins (for control flow).
        Infra hooks (layer 3) always run regardless.

        Returns:
            None = continue, Dict = terminating action (from control.py)
        """
        control_result = None
        for hook in self.hooks:
            if hook.get("event") != event:
                continue
            if not condition_evaluator.matches(context, hook.get("condition", {})):
                continue

            action = hook.get("action", {})
            interpolated = interpolation.interpolate_action(action, context)
            result = await dispatcher.dispatch(
                interpolated, thread_context=thread_context
            )

            if hook.get("layer") == 3:
                continue

            if result and control_result is None:
                data = result.get("data", result)
                if data is not None and data != {"success": True}:
                    control_result = data

        return control_result

    async def run_hooks_context(
        self,
        context: Dict,
        dispatcher: Any,
        event: str,
    ) -> Dict[str, str]:
        """Run context-injection hooks for a given event and collect context blocks.

        Unlike run_hooks(), this method:
        - Filters by the specified event (not hardcoded to thread_started)
        - Runs ALL matching hooks (no short-circuit)
        - Maps LoadTool/ExecuteTool results to XML-wrapped context blocks
        - Returns {"before": str, "after": str} keyed by hook position

        Hook position is controlled by the `position` field (default: "before"):
        - "before": injected before the directive prompt (identity, rules)
        - "after": injected after the directive prompt (supplementary knowledge)
        """
        before_blocks = []
        after_blocks = []
        before_raw = []
        after_raw = []
        for hook in self.hooks:
            if hook.get("event") != event:
                continue
            if not condition_evaluator.matches(context, hook.get("condition", {})):
                continue

            # Support both single action and multi-action hooks
            actions = hook.get("actions", [])
            if not actions:
                single = hook.get("action", {})
                if single:
                    actions = [single]

            for action in actions:
                interpolated = interpolation.interpolate_action(action, context)
                result = await dispatcher.dispatch(interpolated)

                if result and result.get("status") == "success":
                    data = result.get("data", {})
                    content = (
                        data.get("content") or data.get("body") or data.get("raw", "")
                        # LoadTool returns content at top level (no data wrapper)
                        or result.get("content") or result.get("body", "")
                    )
                    if content:
                        item_id = action.get("item_id", "")
                        item_type = action.get("item_type", "")
                        tag = item_type or "context"
                        block = f'<{tag} id="{item_id}">\n{content.strip()}\n</{tag}>'
                        raw_entry = {"id": item_id, "content": content.strip()}
                        if hook.get("position", "before") == "after":
                            after_blocks.append(block)
                            after_raw.append(raw_entry)
                        else:
                            before_blocks.append(block)
                            before_raw.append(raw_entry)

        return {
            "before": "\n\n".join(before_blocks),
            "after": "\n\n".join(after_blocks),
            "before_raw": before_raw,
            "after_raw": after_raw,
        }

    def request_cancel(self):
        self._cancelled = True

    def is_cancelled(self) -> bool:
        return self._cancelled
