"""Error types for Lilux primitives (Phase 1.1).

Primitives return result objects with success field instead of raising
exceptions for expected failures. These errors are for exceptional cases:
- Primitives: Unexpected errors only (bad types, missing config)
- Runtime services: Precondition failures (no token, no file)
"""

from dataclasses import dataclass
from typing import Any, Optional


@dataclass
class ValidationError:
    """Validation error with field, error message, and value.
    
    Attributes:
        field: The field name that failed validation.
        error: Description of the validation error.
        value: The value that failed validation.
    """

    field: str
    error: str
    value: Any

    def __str__(self) -> str:
        return f"ValidationError: {self.field} - {self.error} (got {self.value!r})"


class ToolExecutionError(Exception):
    """Base exception for tool execution failures.
    
    Use for unexpected errors that should not be caught by normal flow.
    For expected failures (command failed, HTTP error), use result objects instead.
    
    Attributes:
        message: Error description.
        cause: Optional underlying exception being wrapped.
    """

    def __init__(self, message: str, cause: Optional[Exception] = None):
        """Initialize ToolExecutionError.
        
        Args:
            message: Description of the error.
            cause: Optional exception that triggered this error.
        """
        super().__init__(message)
        self.message = message
        self.cause = cause


class IntegrityError(ToolExecutionError):
    """Integrity/hash verification failure.
    
    Raised when computed hash doesn't match expected value.
    Can store additional context via **kwargs.
    """

    def __init__(self, message: str, **kwargs):
        """Initialize IntegrityError.
        
        Args:
            message: Description of the integrity error.
            **kwargs: Additional context (e.g., expected, actual).
        """
        super().__init__(message)
        for key, value in kwargs.items():
            setattr(self, key, value)


class LockfileError(ToolExecutionError):
    """Lockfile I/O or format error.
    
    Raised for lockfile-related failures (missing, invalid format, etc).
    
    Attributes:
        message: Description of the error.
        path: Optional path to the problematic lockfile.
    """

    def __init__(self, message: str, path: Optional[str] = None):
        """Initialize LockfileError.
        
        Args:
            message: Description of the error.
            path: Optional path to the lockfile.
        """
        super().__init__(message)
        self.path = path


class ConfigurationError(ToolExecutionError):
    """Configuration error (missing field, invalid value, etc).
    
    Raised when configuration validation fails at executor level.
    For configuration done by orchestrator, this is rarely raised.
    
    Attributes:
        message: Description of the error.
        field: Optional field name that failed.
    """

    def __init__(self, message: str, field: Optional[str] = None):
        """Initialize ConfigurationError.
        
        Args:
            message: Description of the error.
            field: Optional field that caused the error.
        """
        super().__init__(message)
        self.field = field


class AuthenticationRequired(Exception):
    """Authentication is required but not available.
    
    Raised by runtime services (AuthStore, EnvResolver) when a precondition
    for accessing a resource is not met (no token, no credentials, etc).
    
    Attributes:
        message: Description of what's needed.
        service: Optional name of the service requiring authentication.
    """

    def __init__(self, message: str, service: Optional[str] = None):
        """Initialize AuthenticationRequired.
        
        Args:
            message: Description of authentication requirement.
            service: Optional service name.
        """
        super().__init__(message)
        self.message = message
        self.service = service


class RefreshError(Exception):
    """OAuth2 token refresh failed.
    
    Raised by AuthStore when attempting to refresh an expired token.
    
    Attributes:
        message: Description of the refresh failure.
        service: Optional service name that had the refresh failure.
    """

    def __init__(self, message: str, service: Optional[str] = None):
        """Initialize RefreshError.
        
        Args:
            message: Description of the refresh failure.
            service: Optional service name.
        """
        super().__init__(message)
        self.message = message
        self.service = service
