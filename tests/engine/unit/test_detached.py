"""Tests for rye.utils.detached — shared detached process launcher."""

import os
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from rye.utils.detached import collect_env, launch_detached, spawn_thread


class TestCollectEnv:
    """Tests for env var collection logic."""

    def test_forwards_prefixed_vars(self, monkeypatch):
        monkeypatch.setenv("RYE_DEBUG", "1")
        monkeypatch.setenv("PYTHONPATH", "/app")
        monkeypatch.setenv("OPENAI_API_KEY", "sk-test")
        monkeypatch.setenv("UNRELATED_VAR", "nope")

        envs = collect_env()
        assert envs["RYE_DEBUG"] == "1"
        assert envs["PYTHONPATH"] == "/app"
        assert envs["OPENAI_API_KEY"] == "sk-test"
        assert "UNRELATED_VAR" not in envs

    def test_forwards_system_keys(self, monkeypatch):
        monkeypatch.setenv("HOME", "/home/test")
        monkeypatch.setenv("PATH", "/usr/bin")
        monkeypatch.setenv("LANG", "en_US.UTF-8")
        monkeypatch.setenv("TERM", "xterm")

        envs = collect_env()
        assert envs["HOME"] == "/home/test"
        assert envs["PATH"] == "/usr/bin"
        assert envs["LANG"] == "en_US.UTF-8"
        assert envs["TERM"] == "xterm"

    def test_extra_overrides_env(self, monkeypatch):
        monkeypatch.setenv("RYE_DEBUG", "old")

        envs = collect_env(extra={"RYE_DEBUG": "new", "CUSTOM": "val"})
        assert envs["RYE_DEBUG"] == "new"
        assert envs["CUSTOM"] == "val"

    def test_extra_none_is_safe(self):
        envs = collect_env(extra=None)
        assert isinstance(envs, dict)


class TestLaunchDetached:
    """Tests for the launch_detached async helper."""

    @pytest.mark.asyncio
    async def test_success(self, tmp_path):
        mock_result = MagicMock(success=True, pid=12345, error=None)
        mock_proc = AsyncMock()
        mock_proc.spawn = AsyncMock(return_value=mock_result)

        with patch("lillux.primitives.subprocess.SubprocessPrimitive", return_value=mock_proc):
            result = await launch_detached(
                cmd=["python", "-c", "pass"],
                thread_id="test-thread-1",
                log_dir=tmp_path / "logs",
            )

        assert result == {"success": True, "pid": 12345}
        assert (tmp_path / "logs").is_dir()
        mock_proc.spawn.assert_awaited_once()

    @pytest.mark.asyncio
    async def test_failure(self, tmp_path):
        mock_result = MagicMock(success=False, pid=None, error="proc not found")
        mock_proc = AsyncMock()
        mock_proc.spawn = AsyncMock(return_value=mock_result)

        with patch("lillux.primitives.subprocess.SubprocessPrimitive", return_value=mock_proc):
            result = await launch_detached(
                cmd=["nonexistent"],
                thread_id="test-thread-2",
                log_dir=tmp_path / "logs",
            )

        assert result == {"success": False, "error": "proc not found"}

    @pytest.mark.asyncio
    async def test_passes_input_data(self, tmp_path):
        mock_result = MagicMock(success=True, pid=99, error=None)
        mock_proc = AsyncMock()
        mock_proc.spawn = AsyncMock(return_value=mock_result)

        with patch("lillux.primitives.subprocess.SubprocessPrimitive", return_value=mock_proc):
            await launch_detached(
                cmd=["python", "script.py"],
                thread_id="test-thread-3",
                log_dir=tmp_path / "logs",
                input_data='{"key": "value"}',
            )

        call_kwargs = mock_proc.spawn.call_args
        assert call_kwargs.kwargs.get("input_data") == '{"key": "value"}'

    @pytest.mark.asyncio
    async def test_env_extra_forwarded(self, tmp_path):
        mock_result = MagicMock(success=True, pid=77, error=None)
        mock_proc = AsyncMock()
        mock_proc.spawn = AsyncMock(return_value=mock_result)

        with patch("lillux.primitives.subprocess.SubprocessPrimitive", return_value=mock_proc):
            await launch_detached(
                cmd=["python", "script.py"],
                thread_id="test-thread-4",
                log_dir=tmp_path / "logs",
                env_extra={"RYE_PARENT_THREAD_ID": "tid-123"},
            )

        call_kwargs = mock_proc.spawn.call_args
        envs = call_kwargs.kwargs.get("envs", {})
        assert envs["RYE_PARENT_THREAD_ID"] == "tid-123"

    @pytest.mark.asyncio
    async def test_creates_log_dir(self, tmp_path):
        nested = tmp_path / "a" / "b" / "c"
        mock_result = MagicMock(success=True, pid=1, error=None)
        mock_proc = AsyncMock()
        mock_proc.spawn = AsyncMock(return_value=mock_result)

        with patch("lillux.primitives.subprocess.SubprocessPrimitive", return_value=mock_proc):
            await launch_detached(
                cmd=["echo"],
                thread_id="test-thread-5",
                log_dir=nested,
            )

        assert nested.is_dir()
        # log_path passed to spawn is nested/spawn.log
        call_kwargs = mock_proc.spawn.call_args
        assert call_kwargs.kwargs.get("log_path") == str(nested / "spawn.log")

    @pytest.mark.asyncio
    async def test_exception_returns_error_dict(self, tmp_path):
        with patch(
            "lillux.primitives.subprocess.SubprocessPrimitive",
            side_effect=RuntimeError("lillux not installed"),
        ):
            result = await launch_detached(
                cmd=["python", "-c", "pass"],
                thread_id="test-thread-exc",
                log_dir=tmp_path / "logs",
            )

        assert result["success"] is False
        assert "lillux not installed" in result["error"]


class TestSpawnThread:
    """Tests for spawn_thread — lifecycle helper that wraps launch_detached."""

    def _mock_registry(self):
        reg = MagicMock()
        reg.register = MagicMock()
        reg.update_status = MagicMock()
        reg.update_pid = MagicMock()
        return reg

    def _mock_spawn_success(self, pid=12345):
        mock_result = MagicMock(success=True, pid=pid, error=None)
        mock_proc = AsyncMock()
        mock_proc.spawn = AsyncMock(return_value=mock_result)
        return mock_proc

    def _mock_spawn_failure(self, error="spawn failed"):
        mock_result = MagicMock(success=False, pid=None, error=error)
        mock_proc = AsyncMock()
        mock_proc.spawn = AsyncMock(return_value=mock_result)
        return mock_proc

    @pytest.mark.asyncio
    async def test_success_lifecycle(self, tmp_path):
        """On success: register → running → update PID with child pid."""
        reg = self._mock_registry()

        with patch("lillux.primitives.subprocess.SubprocessPrimitive",
                   return_value=self._mock_spawn_success(pid=42)):
            result = await spawn_thread(
                registry=reg,
                thread_id="tid-001",
                directive="tool/my-tool",
                cmd=["python", "-c", "pass"],
                log_dir=tmp_path / "threads" / "tid-001",
            )

        assert result["success"] is True
        assert result["pid"] == 42
        reg.register.assert_called_once_with("tid-001", "tool/my-tool", None)
        reg.update_status.assert_called_once_with("tid-001", "running")
        reg.update_pid.assert_called_once_with("tid-001", 42)

    @pytest.mark.asyncio
    async def test_failure_lifecycle(self, tmp_path):
        """On failure: register → running → error status, no PID update."""
        reg = self._mock_registry()

        with patch("lillux.primitives.subprocess.SubprocessPrimitive",
                   return_value=self._mock_spawn_failure("proc died")):
            result = await spawn_thread(
                registry=reg,
                thread_id="tid-002",
                directive="tool/broken",
                cmd=["nonexistent"],
                log_dir=tmp_path / "threads" / "tid-002",
            )

        assert result["success"] is False
        assert result["error"] == "proc died"
        reg.register.assert_called_once_with("tid-002", "tool/broken", None)
        # update_status called twice: "running" then "error"
        assert reg.update_status.call_count == 2
        reg.update_status.assert_any_call("tid-002", "running")
        reg.update_status.assert_any_call("tid-002", "error")
        reg.update_pid.assert_not_called()

    @pytest.mark.asyncio
    async def test_parent_id_forwarded(self, tmp_path):
        """parent_id is passed through to registry.register()."""
        reg = self._mock_registry()

        with patch("lillux.primitives.subprocess.SubprocessPrimitive",
                   return_value=self._mock_spawn_success()):
            await spawn_thread(
                registry=reg,
                thread_id="child-001",
                directive="tool/x",
                cmd=["python", "-c", "pass"],
                log_dir=tmp_path / "logs",
                parent_id="parent-001",
            )

        reg.register.assert_called_once_with("child-001", "tool/x", "parent-001")

    @pytest.mark.asyncio
    async def test_input_data_forwarded(self, tmp_path):
        """input_data is piped through to launch_detached."""
        reg = self._mock_registry()
        mock_proc = self._mock_spawn_success()

        with patch("lillux.primitives.subprocess.SubprocessPrimitive",
                   return_value=mock_proc):
            await spawn_thread(
                registry=reg,
                thread_id="tid-003",
                directive="tool/y",
                cmd=["python", "script.py"],
                log_dir=tmp_path / "logs",
                input_data='{"payload": true}',
            )

        call_kwargs = mock_proc.spawn.call_args.kwargs
        assert call_kwargs["input_data"] == '{"payload": true}'

    @pytest.mark.asyncio
    async def test_creates_log_dir(self, tmp_path):
        """Log dir is created by the underlying launch_detached."""
        reg = self._mock_registry()
        log_dir = tmp_path / "deep" / "nested" / "dir"

        with patch("lillux.primitives.subprocess.SubprocessPrimitive",
                   return_value=self._mock_spawn_success()):
            await spawn_thread(
                registry=reg,
                thread_id="tid-004",
                directive="tool/z",
                cmd=["echo"],
                log_dir=log_dir,
            )

        assert log_dir.is_dir()

    @pytest.mark.asyncio
    async def test_pid_is_child_not_parent(self, tmp_path):
        """The PID recorded is the spawned child's PID, not os.getpid()."""
        reg = self._mock_registry()
        child_pid = 99999

        with patch("lillux.primitives.subprocess.SubprocessPrimitive",
                   return_value=self._mock_spawn_success(pid=child_pid)):
            result = await spawn_thread(
                registry=reg,
                thread_id="tid-005",
                directive="tool/w",
                cmd=["python", "-c", "pass"],
                log_dir=tmp_path / "logs",
            )

        assert result["pid"] == child_pid
        reg.update_pid.assert_called_once_with("tid-005", child_pid)
        # Verify it's NOT the parent PID
        assert child_pid != os.getpid()
