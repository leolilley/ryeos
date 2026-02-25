# rye:signed:2026-02-25T00:02:14Z:a10d2b176bab755251d8d5fa79ae1b3f7faeb72fa9abcb30e3245e0ea813d18c:7Ct1oL-z4DDfhELZfEtEatne-awPehiv5pOV9o-UqeEV3UTu3cNyz5QXVdrdKcgoLtcXFbC2hgMQkyf0F2yUCQ==:9fbfabe975fa5a7f
"""
errors.py: Typed exceptions for the thread system.

All Part 2 modules raise typed exceptions instead of returning
None/False/empty dicts. Classified by error_classification.yaml.
"""

__version__ = "1.2.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads"
__tool_description__ = "Typed exceptions for the thread system"


class ThreadSystemError(Exception):
    """Base for all thread system errors."""


class TranscriptCorrupt(ThreadSystemError):
    """Transcript JSONL has unparseable lines."""

    def __init__(self, path: str, line_no: int, raw_line: str):
        self.path = path
        self.line_no = line_no
        super().__init__(f"Corrupt transcript at {path}:{line_no}")


class ResumeImpossible(ThreadSystemError):
    """Cannot resume thread — insufficient recovery data."""

    def __init__(self, thread_id: str, reason: str):
        self.thread_id = thread_id
        self.reason = reason
        super().__init__(f"Cannot resume {thread_id}: {reason}")


class ThreadNotFound(ThreadSystemError):
    """No registry entry or completion event for thread."""

    def __init__(self, thread_id: str, context: str = ""):
        self.thread_id = thread_id
        super().__init__(
            f"Thread not found: {thread_id}" + (f" ({context})" if context else "")
        )


class CheckpointFailed(ThreadSystemError):
    """State checkpoint write failed. Thread must stop."""

    def __init__(self, thread_id: str, trigger: str, cause: Exception):
        self.thread_id = thread_id
        self.trigger = trigger
        self.cause = cause
        super().__init__(f"Checkpoint failed for {thread_id} at {trigger}: {cause}")


class ProviderCallError(ThreadSystemError):
    """HTTP/API failure from a provider."""

    def __init__(
        self,
        provider_id: str,
        message: str,
        http_status: int = None,
        request_id: str = None,
        error_type: str = None,
        retryable: bool = False,
    ):
        self.provider_id = provider_id
        self.message = message
        self.http_status = http_status
        self.request_id = request_id
        self.error_type = error_type
        self.retryable = retryable
        super().__init__(str(self))

    def __str__(self):
        base = f"Provider '{self.provider_id}' failed"
        if self.http_status is not None:
            base += f" (HTTP {self.http_status})"
        return f"{base}: {self.message}"

    def to_dict(self):
        return {
            "provider_id": self.provider_id,
            "message": self.message,
            "http_status": self.http_status,
            "request_id": self.request_id,
            "error_type": self.error_type,
            "retryable": self.retryable,
        }


class LockfileIntegrityError(ThreadSystemError):
    """Stale lockfile failure."""

    def __init__(
        self,
        item_id: str,
        lockfile_path: str = None,
        expected_hash: str = None,
        actual_hash: str = None,
    ):
        self.item_id = item_id
        self.lockfile_path = lockfile_path
        self.expected_hash = expected_hash
        self.actual_hash = actual_hash
        super().__init__(str(self))

    def __str__(self):
        base = f"Lockfile integrity mismatch for '{self.item_id}'."
        if self.lockfile_path is not None:
            base += f" Delete stale lockfile: {self.lockfile_path}"
        return base

    def to_dict(self):
        return {
            "item_id": self.item_id,
            "lockfile_path": self.lockfile_path,
            "expected_hash": self.expected_hash,
            "actual_hash": self.actual_hash,
        }


class HookOverrideError(ThreadSystemError):
    """Hook tried to blank an error."""

    def __init__(self, hook_event: str, original_error: str):
        self.hook_event = hook_event
        self.original_error = original_error
        super().__init__(str(self))

    def __str__(self):
        return (
            f"Hook for '{self.hook_event}' attempted empty error override. "
            f"Original: {self.original_error}"
        )


class BudgetNotRegistered(ThreadSystemError):
    """Thread has no budget ledger entry."""
    def __init__(self, thread_id: str):
        self.thread_id = thread_id
        super().__init__(f"No budget ledger entry for thread: {thread_id}")


class InsufficientBudget(ThreadSystemError):
    """Parent cannot afford requested reservation."""
    def __init__(self, parent_id: str, remaining: float, requested: float):
        self.parent_id = parent_id
        self.remaining = remaining
        self.requested = requested
        super().__init__(f"Insufficient budget: parent={parent_id} remaining={remaining} requested={requested}")


class BudgetOverspend(ThreadSystemError):
    """Actual spend exceeded reserved amount."""
    def __init__(self, thread_id: str, reserved: float, actual: float):
        self.thread_id = thread_id
        self.reserved = reserved
        self.actual = actual
        super().__init__(f"Overspend: thread={thread_id} reserved={reserved} actual={actual}")


class BudgetLedgerLocked(ThreadSystemError):
    """SQLite write lock contention."""
    def __init__(self, operation: str):
        self.operation = operation
        super().__init__(f"Budget ledger locked during: {operation}")


class ContinuationFailed(ThreadSystemError):
    """Failed to spawn continuation thread."""
    def __init__(self, thread_id: str, reason: str):
        self.thread_id = thread_id
        self.reason = reason
        super().__init__(f"Continuation failed for {thread_id}: {reason}")


class ChainResolutionError(ThreadSystemError):
    """Cycle or break in continuation chain."""
    def __init__(self, thread_id: str, chain_issue: str):
        self.thread_id = thread_id
        self.chain_issue = chain_issue
        super().__init__(f"Chain resolution error at {thread_id}: {chain_issue}")


class ToolInputParseError(ThreadSystemError):
    """Streaming tool input JSON could not be parsed."""
    def __init__(self, tool_id: str, raw: str):
        self.tool_id = tool_id
        self.raw = raw[:200]
        super().__init__(f"Failed to parse tool input for {tool_id}")


def make_error_dict(
    message,
    error_type="unknown",
    code=None,
    component=None,
    retryable=False,
    cause=None,
    diagnostics=None,
):
    return {
        "message": message,
        "type": error_type,
        "code": code,
        "component": component,
        "retryable": retryable,
        "cause": cause,
        "diagnostics": diagnostics or {},
    }
