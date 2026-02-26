"""Tests for Lillux error types (Phase 1.1)."""

import pytest
from lillux.primitives.errors import (
    ValidationError,
    ToolExecutionError,
    IntegrityError,
    LockfileError,
    ConfigurationError,
    AuthenticationRequired,
    RefreshError,
)


class TestValidationError:
    """ValidationError(field, error, value) dataclass."""

    def test_create_validation_error(self):
        """ValidationError creates with field, error, value."""
        err = ValidationError(field="timeout", error="must be positive", value="-5")
        assert err.field == "timeout"
        assert err.error == "must be positive"
        assert err.value == "-5"

    def test_validation_error_str(self):
        """ValidationError has readable string representation."""
        err = ValidationError(field="method", error="unknown method", value="PATCH")
        assert "method" in str(err)
        assert "unknown method" in str(err)


class TestToolExecutionError:
    """ToolExecutionError base exception."""

    def test_create_tool_execution_error(self):
        """ToolExecutionError(message, cause=None)."""
        err = ToolExecutionError("command failed")
        assert "command failed" in str(err)

    def test_tool_execution_error_with_cause(self):
        """ToolExecutionError can wrap another exception."""
        cause = ValueError("invalid value")
        err = ToolExecutionError("execution failed", cause=cause)
        assert err.cause is cause
        assert "execution failed" in str(err)


class TestIntegrityError:
    """IntegrityError for hash mismatches."""

    def test_create_integrity_error(self):
        """IntegrityError(message, **kwargs)."""
        err = IntegrityError("hash mismatch", expected="abc123", actual="xyz789")
        assert "hash mismatch" in str(err)
        assert err.expected == "abc123"
        assert err.actual == "xyz789"

    def test_integrity_error_minimal(self):
        """IntegrityError works with just message."""
        err = IntegrityError("corrupted file")
        assert "corrupted file" in str(err)


class TestLockfileError:
    """LockfileError for lockfile problems."""

    def test_create_lockfile_error(self):
        """LockfileError(message, path=None)."""
        err = LockfileError("invalid format", path="/path/to/lockfile.json")
        assert "invalid format" in str(err)
        assert err.path == "/path/to/lockfile.json"

    def test_lockfile_error_no_path(self):
        """LockfileError works without path."""
        err = LockfileError("lockfile not found")
        assert "lockfile not found" in str(err)
        assert err.path is None


class TestConfigurationError:
    """ConfigurationError for config issues."""

    def test_create_configuration_error(self):
        """ConfigurationError(message, field=None)."""
        err = ConfigurationError("missing required field", field="api_key")
        assert "missing required field" in str(err)
        assert err.field == "api_key"

    def test_configuration_error_no_field(self):
        """ConfigurationError works without field."""
        err = ConfigurationError("invalid config structure")
        assert "invalid config structure" in str(err)
        assert err.field is None


class TestAuthenticationRequired:
    """AuthenticationRequired for auth failures."""

    def test_create_authentication_required(self):
        """AuthenticationRequired(message, service=None)."""
        err = AuthenticationRequired("token expired", service="github")
        assert "token expired" in str(err)
        assert err.service == "github"

    def test_authentication_required_no_service(self):
        """AuthenticationRequired works without service."""
        err = AuthenticationRequired("not authenticated")
        assert "not authenticated" in str(err)
        assert err.service is None


class TestRefreshError:
    """RefreshError for OAuth2 refresh failures."""

    def test_create_refresh_error(self):
        """RefreshError(message, service=None)."""
        err = RefreshError("refresh token expired", service="google")
        assert "refresh token expired" in str(err)
        assert err.service == "google"

    def test_refresh_error_no_service(self):
        """RefreshError works without service."""
        err = RefreshError("failed to refresh")
        assert "failed to refresh" in str(err)
        assert err.service is None


class TestErrorHierarchy:
    """Test that errors are properly categorized."""

    def test_tool_execution_error_is_exception(self):
        """ToolExecutionError is an Exception subclass."""
        assert issubclass(ToolExecutionError, Exception)

    def test_integrity_error_is_tool_execution_error(self):
        """IntegrityError is a ToolExecutionError subclass."""
        assert issubclass(IntegrityError, ToolExecutionError)

    def test_lockfile_error_is_tool_execution_error(self):
        """LockfileError is a ToolExecutionError subclass."""
        assert issubclass(LockfileError, ToolExecutionError)

    def test_configuration_error_is_tool_execution_error(self):
        """ConfigurationError is a ToolExecutionError subclass."""
        assert issubclass(ConfigurationError, ToolExecutionError)

    def test_authentication_required_is_exception(self):
        """AuthenticationRequired is an Exception (runtime precondition)."""
        assert issubclass(AuthenticationRequired, Exception)

    def test_refresh_error_is_exception(self):
        """RefreshError is an Exception (runtime precondition)."""
        assert issubclass(RefreshError, Exception)
