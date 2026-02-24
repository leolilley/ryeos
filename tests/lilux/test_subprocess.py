"""Tests for subprocess primitive."""

import asyncio
import shutil
import tempfile
from pathlib import Path
from unittest.mock import patch

import pytest
from lilux.primitives.subprocess import (
    SubprocessPrimitive,
    SubprocessResult,
    SpawnResult,
    KillResult,
    StatusResult,
)
from lilux.primitives.errors import ConfigurationError


# Skip all tests if rye-proc is not on PATH
pytestmark = pytest.mark.skipif(
    shutil.which("rye-proc") is None,
    reason="rye-proc binary not found on PATH",
)


class TestSubprocessResult:
    """Test SubprocessResult dataclass."""

    def test_create_subprocess_result_success(self):
        """Create successful SubprocessResult."""
        result = SubprocessResult(
            success=True,
            stdout="output",
            stderr="",
            return_code=0,
            duration_ms=100,
        )
        assert result.success is True
        assert result.stdout == "output"
        assert result.stderr == ""
        assert result.return_code == 0
        assert result.duration_ms == 100

    def test_create_subprocess_result_failure(self):
        """Create failed SubprocessResult."""
        result = SubprocessResult(
            success=False,
            stdout="",
            stderr="error",
            return_code=1,
            duration_ms=50,
        )
        assert result.success is False
        assert result.stderr == "error"
        assert result.return_code == 1


class TestSpawnResult:
    """Test SpawnResult dataclass."""

    def test_spawn_result_success(self):
        result = SpawnResult(success=True, pid=12345)
        assert result.success is True
        assert result.pid == 12345
        assert result.error is None

    def test_spawn_result_failure(self):
        result = SpawnResult(success=False, error="spawn failed")
        assert result.success is False
        assert result.pid is None
        assert result.error == "spawn failed"


class TestKillResult:
    """Test KillResult dataclass."""

    def test_kill_result_terminated(self):
        result = KillResult(success=True, pid=123, method="terminated")
        assert result.success is True
        assert result.method == "terminated"

    def test_kill_result_failure(self):
        result = KillResult(success=False, pid=123, error="not found")
        assert result.success is False
        assert result.error == "not found"


class TestStatusResult:
    """Test StatusResult dataclass."""

    def test_status_alive(self):
        result = StatusResult(pid=123, alive=True)
        assert result.alive is True

    def test_status_dead(self):
        result = StatusResult(pid=123, alive=False)
        assert result.alive is False


class TestConfigurationError:
    """Test rye-proc hard requirement."""

    def test_raises_when_rye_proc_missing(self):
        with patch("shutil.which", return_value=None):
            with pytest.raises(ConfigurationError, match="rye-proc"):
                SubprocessPrimitive()


@pytest.mark.asyncio
class TestSubprocessPrimitive:
    """Test SubprocessPrimitive execution via rye-proc exec."""

    async def test_execute_simple_command(self):
        """Execute simple echo command."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "echo",
            "args": ["hello"],
        }
        result = await primitive.execute(config, {})

        assert result.success is True
        assert "hello" in result.stdout
        assert result.return_code == 0
        assert result.duration_ms >= 0

    async def test_execute_command_with_cwd(self):
        """Execute command in specific directory."""
        with tempfile.TemporaryDirectory() as tmpdir:
            primitive = SubprocessPrimitive()
            config = {
                "command": "pwd",
                "cwd": tmpdir,
            }
            result = await primitive.execute(config, {})

            assert result.success is True
            assert tmpdir in result.stdout

    async def test_execute_failed_command(self):
        """Capture failed command (non-zero return code)."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "sh",
            "args": ["-c", "exit 42"],
        }
        result = await primitive.execute(config, {})

        assert result.success is False
        assert result.return_code == 42

    async def test_execute_with_input_data(self):
        """Pass input data to command."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "cat",
            "input_data": "test input",
        }
        result = await primitive.execute(config, {})

        assert result.success is True
        assert "test input" in result.stdout

    async def test_env_var_templating_simple(self):
        """Environment variable templating: ${VAR:-default}."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "echo",
            "args": ["${MY_VAR:-default_value}"],
            "env": {"MY_VAR": "custom"},
        }
        result = await primitive.execute(config, {})

        assert result.success is True
        assert "custom" in result.stdout

    async def test_env_var_templating_uses_default(self):
        """Environment variable templating uses default when var missing."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "echo",
            "args": ["${MISSING_VAR:-fallback}"],
            "env": {},
        }
        result = await primitive.execute(config, {})

        assert result.success is True
        assert "fallback" in result.stdout

    async def test_param_templating(self):
        """Runtime parameter templating: {param_name}."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "echo",
            "args": ["parameter is {value}"],
        }
        result = await primitive.execute(config, {"value": "injected"})

        assert result.success is True
        assert "injected" in result.stdout

    async def test_missing_param_left_unchanged(self):
        """Missing parameters are left unchanged in output."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "echo",
            "args": ["{missing_param}"],
        }
        result = await primitive.execute(config, {})

        assert result.success is True
        assert "{missing_param}" in result.stdout

    async def test_both_templating_systems(self):
        """Both env and param templating work together."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "echo",
            "args": ["${VAR1:-default1} {param1}"],
            "env": {"VAR1": "env_value"},
        }
        result = await primitive.execute(config, {"param1": "param_value"})

        assert result.success is True
        assert "env_value" in result.stdout
        assert "param_value" in result.stdout

    async def test_env_merge_small_count(self):
        """Small env count (<50) merges with os.environ."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "sh",
            "args": ["-c", "echo $USER"],
            "env": {"CUSTOM_VAR": "value"},
        }
        result = await primitive.execute(config, {})

        assert result.success is True

    async def test_env_merge_large_count(self):
        """Large env count (>=50) uses directly as resolved env."""
        primitive = SubprocessPrimitive()
        large_env = {f"VAR_{i}": f"value_{i}" for i in range(50)}
        large_env["CUSTOM_VAR"] = "custom_value"

        config = {
            "command": "echo",
            "args": ["test"],
            "env": large_env,
        }
        result = await primitive.execute(config, {})

        assert result.success is True

    async def test_stderr_captured(self):
        """Stderr is captured separately from stdout."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "sh",
            "args": ["-c", "echo stdout_msg && echo stderr_msg >&2"],
        }
        result = await primitive.execute(config, {})

        assert result.success is True
        assert "stdout_msg" in result.stdout
        assert "stderr_msg" in result.stderr

    async def test_duration_ms_populated(self):
        """duration_ms is always populated."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "echo",
            "args": ["test"],
        }
        result = await primitive.execute(config, {})

        assert result.duration_ms is not None
        assert result.duration_ms >= 0

    async def test_command_with_multiple_args(self):
        """Command with multiple arguments."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "echo",
            "args": ["arg1", "arg2", "arg3"],
        }
        result = await primitive.execute(config, {})

        assert result.success is True
        assert "arg1" in result.stdout
        assert "arg2" in result.stdout
        assert "arg3" in result.stdout

    async def test_nonexistent_command_fails(self):
        """Nonexistent command results in failure."""
        primitive = SubprocessPrimitive()
        config = {
            "command": "nonexistent_command_xyz",
        }
        result = await primitive.execute(config, {})

        assert result.success is False

    async def test_no_command_specified(self):
        """Missing command returns error."""
        primitive = SubprocessPrimitive()
        result = await primitive.execute({}, {})

        assert result.success is False
        assert "No command specified" in result.stderr


@pytest.mark.asyncio
class TestSubprocessLifecycle:
    """Test spawn/kill/status lifecycle methods."""

    async def test_spawn_and_kill(self):
        """Spawn a process and kill it."""
        primitive = SubprocessPrimitive()
        spawn_result = await primitive.spawn("sleep", ["60"])
        assert spawn_result.success is True
        assert spawn_result.pid is not None
        assert spawn_result.pid > 0

        # Verify alive
        status_result = await primitive.status(spawn_result.pid)
        assert status_result.alive is True

        # Kill it
        kill_result = await primitive.kill(spawn_result.pid, grace=1.0)
        assert kill_result.success is True
        assert kill_result.method in ("terminated", "killed")

    async def test_kill_nonexistent_pid(self):
        """Killing a nonexistent PID returns already_dead."""
        primitive = SubprocessPrimitive()
        result = await primitive.kill(999999, grace=0.5)
        assert result.success is True
        assert result.method == "already_dead"

    async def test_status_nonexistent_pid(self):
        """Status of nonexistent PID returns not alive."""
        primitive = SubprocessPrimitive()
        result = await primitive.status(999999)
        assert result.alive is False

    async def test_spawn_with_log_file(self):
        """Spawn with log file redirection."""
        with tempfile.NamedTemporaryFile(suffix=".log", delete=False) as f:
            log_path = f.name

        primitive = SubprocessPrimitive()
        result = await primitive.spawn(
            "sh", ["-c", "echo hello_log"],
            log_path=log_path,
        )
        assert result.success is True

        # Wait for process to finish writing
        await asyncio.sleep(0.5)

        log_content = Path(log_path).read_text()
        assert "hello_log" in log_content

        # Clean up
        Path(log_path).unlink(missing_ok=True)
