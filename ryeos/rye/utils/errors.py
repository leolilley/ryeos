"""Standardized error responses for consistency across RYE OS.

Provides:
- ErrorCode enum with all possible error codes
- ErrorResponse dataclass for standard error format
- Helper functions for common error patterns
"""

from dataclasses import dataclass
from enum import Enum
from typing import Any, Dict, Optional


class ErrorCode(Enum):
    """Standardized error codes."""

    # Validation errors
    VALIDATION_FAILED = "VALIDATION_FAILED"
    SCHEMA_NOT_FOUND = "SCHEMA_NOT_FOUND"
    VERSION_MISMATCH = "VERSION_MISMATCH"

    # Execution errors
    TOOL_NOT_FOUND = "TOOL_NOT_FOUND"
    EXECUTOR_NOT_FOUND = "EXECUTOR_NOT_FOUND"
    CIRCULAR_DEPENDENCY = "CIRCULAR_DEPENDENCY"
    CHAIN_TOO_DEEP = "CHAIN_TOO_DEEP"

    # Runtime errors
    EXECUTION_FAILED = "EXECUTION_FAILED"
    TIMEOUT = "TIMEOUT"
    RESOURCE_EXCEEDED = "RESOURCE_EXCEEDED"

    # Auth errors
    AUTH_REQUIRED = "AUTH_REQUIRED"
    AUTH_FAILED = "AUTH_FAILED"
    SESSION_EXPIRED = "SESSION_EXPIRED"

    # Registry errors
    REGISTRY_ERROR = "REGISTRY_ERROR"
    ITEM_NOT_FOUND = "ITEM_NOT_FOUND"

    # System errors
    FILE_SYSTEM_ERROR = "FILE_SYSTEM_ERROR"
    PARSING_ERROR = "PARSING_ERROR"
    CONFIG_ERROR = "CONFIG_ERROR"
    UNKNOWN_ERROR = "UNKNOWN_ERROR"


@dataclass
class ErrorResponse:
    """Standardized error response format.

    All errors should use this format for consistency.
    """

    code: ErrorCode
    message: str
    details: Optional[Dict[str, Any]] = None
    retryable: bool = False
    suggestion: Optional[str] = None

    def to_dict(self) -> Dict[str, Any]:
        """Convert to dictionary for JSON serialization."""
        return {
            "success": False,
            "error": {
                "code": self.code.value,
                "message": self.message,
                "details": self.details,
                "retryable": self.retryable,
                "suggestion": self.suggestion,
            },
        }

    @classmethod
    def not_found(cls, item_type: str, item_id: str) -> "ErrorResponse":
        """Create 'not found' error."""
        return cls(
            code=ErrorCode.TOOL_NOT_FOUND,
            message=f"{item_type} not found: {item_id}",
            suggestion=f"Check the item_id and ensure it exists in .ai/{item_type}s/",
        )

    @classmethod
    def validation_failed(
        cls, issues: list, item_id: str
    ) -> "ErrorResponse":
        """Create validation failed error."""
        return cls(
            code=ErrorCode.VALIDATION_FAILED,
            message=f"Validation failed for {item_id}",
            details={"issues": issues},
            retryable=False,
            suggestion="Fix the validation issues and try again",
        )

    @classmethod
    def auth_required(cls, action: str = "perform this action") -> "ErrorResponse":
        """Create auth required error."""
        return cls(
            code=ErrorCode.AUTH_REQUIRED,
            message=f"Authentication required to {action}",
            suggestion="Run 'registry login' to authenticate",
        )

    @classmethod
    def circular_dependency(cls, chain: list) -> "ErrorResponse":
        """Create circular dependency error."""
        return cls(
            code=ErrorCode.CIRCULAR_DEPENDENCY,
            message="Circular dependency detected in tool chain",
            details={"chain": chain},
            suggestion="Check tool executor_id references for cycles",
        )

    @classmethod
    def execution_failed(cls, item_id: str, reason: str) -> "ErrorResponse":
        """Create execution failed error."""
        return cls(
            code=ErrorCode.EXECUTION_FAILED,
            message=f"Execution failed for {item_id}: {reason}",
            retryable=True,
        )


# Helper functions for common patterns
def ok_response(
    data: Any = None, metadata: Optional[Dict[str, Any]] = None
) -> Dict[str, Any]:
    """Create standard success response."""
    result: Dict[str, Any] = {"success": True}
    if data is not None:
        result["data"] = data
    if metadata:
        result["metadata"] = metadata
    return result
