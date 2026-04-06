"""Tests for lillux exec stream — raw passthrough execution mode."""

import asyncio
import shutil
import subprocess
import tempfile
from pathlib import Path

import pytest


LILLUX = shutil.which("lillux")

pytestmark = pytest.mark.skipif(
    LILLUX is None,
    reason="lillux binary not found on PATH",
)


class TestStreamBasicOutput:
    """Test raw stdout/stderr passthrough."""

    def test_stdout_passthrough(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "echo", "--arg", "hello stream"],
            capture_output=True, text=True,
        )
        assert r.returncode == 0
        assert r.stdout == "hello stream\n"

    def test_multiline_stdout(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "sh", "--arg", "-c",
             "--arg", "echo line1; echo line2; echo line3"],
            capture_output=True, text=True,
        )
        assert r.returncode == 0
        assert r.stdout == "line1\nline2\nline3\n"

    def test_stderr_passthrough(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "sh", "--arg", "-c",
             "--arg", "echo err_msg >&2"],
            capture_output=True, text=True,
        )
        assert r.returncode == 0
        assert "err_msg" in r.stderr

    def test_stdout_and_stderr_separate(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "sh", "--arg", "-c",
             "--arg", "echo out_msg; echo err_msg >&2"],
            capture_output=True, text=True,
        )
        assert r.returncode == 0
        assert "out_msg" in r.stdout
        assert "err_msg" in r.stderr

    def test_no_json_wrapping(self):
        """Stream mode must NOT wrap output in JSON (unlike exec run)."""
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "echo", "--arg", "raw"],
            capture_output=True, text=True,
        )
        assert r.stdout == "raw\n"
        assert "{" not in r.stdout

    def test_binary_passthrough(self):
        """Binary data passes through without corruption."""
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "sh", "--arg", "-c",
             "--arg", r"printf '\x00\x01\x02\xff'"],
            capture_output=True,
        )
        assert r.returncode == 0
        assert r.stdout == b"\x00\x01\x02\xff"

    def test_empty_output(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "true"],
            capture_output=True, text=True,
        )
        assert r.returncode == 0
        assert r.stdout == ""

    def test_large_output(self):
        """Large output is forwarded completely."""
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "sh", "--arg", "-c",
             "--arg", "seq 1 10000"],
            capture_output=True, text=True,
        )
        assert r.returncode == 0
        lines = r.stdout.strip().split("\n")
        assert len(lines) == 10000
        assert lines[0] == "1"
        assert lines[-1] == "10000"


class TestStreamExitCodes:
    """Test exit code passthrough and special codes."""

    def test_success_exit_zero(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "true"],
            capture_output=True,
        )
        assert r.returncode == 0

    def test_failure_exit_code(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "sh", "--arg", "-c",
             "--arg", "exit 42"],
            capture_output=True,
        )
        assert r.returncode == 42

    def test_exit_code_one(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "false"],
            capture_output=True,
        )
        assert r.returncode == 1

    def test_timeout_returns_124(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "sleep", "--arg", "60",
             "--timeout", "0.3"],
            capture_output=True, text=True,
        )
        assert r.returncode == 124
        assert "timed out" in r.stderr

    def test_spawn_failure_returns_125(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "/nonexistent/binary"],
            capture_output=True, text=True,
        )
        assert r.returncode == 125
        assert "Failed to spawn" in r.stderr


class TestStreamArguments:
    """Test argument handling mirrors exec run."""

    def test_cwd(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            r = subprocess.run(
                [LILLUX, "exec", "stream", "--cmd", "pwd", "--cwd", tmpdir],
                capture_output=True, text=True,
            )
            assert r.returncode == 0
            assert tmpdir in r.stdout

    def test_env_vars(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "sh", "--arg", "-c",
             "--arg", "echo $MY_VAR", "--env", "MY_VAR=hello_env"],
            capture_output=True, text=True,
        )
        assert r.returncode == 0
        assert "hello_env" in r.stdout

    def test_multiple_args(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "echo",
             "--arg", "a", "--arg", "b", "--arg", "c"],
            capture_output=True, text=True,
        )
        assert r.returncode == 0
        assert r.stdout == "a b c\n"

    def test_stdin_data(self):
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "cat",
             "--stdin", "piped input"],
            capture_output=True, text=True,
        )
        assert r.returncode == 0
        assert "piped input" in r.stdout

    def test_pythonunbuffered_set(self):
        """PYTHONUNBUFFERED=1 is automatically set for Python children."""
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "python3", "--arg", "-c",
             "--arg", "import os; print(os.environ.get('PYTHONUNBUFFERED', 'unset'))"],
            capture_output=True, text=True,
        )
        assert r.returncode == 0
        assert "1" in r.stdout


class TestStreamIncremental:
    """Test that output arrives incrementally, not buffered until exit."""

    def test_incremental_output(self):
        """Lines arrive before the child exits."""
        proc = subprocess.Popen(
            [LILLUX, "exec", "stream", "--cmd", "python3", "--arg", "-c",
             "--arg", "import time; print('first', flush=True); time.sleep(0.3); print('second', flush=True)"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        # Read the first line — should arrive before child exits
        first_line = proc.stdout.readline()
        assert first_line.strip() == "first"
        # Child is still running at this point
        assert proc.poll() is None
        # Wait for completion
        proc.wait()
        second_line = proc.stdout.readline()
        assert second_line.strip() == "second"
        assert proc.returncode == 0

    def test_timeout_with_prior_output(self):
        """Output before timeout is forwarded, then 124."""
        r = subprocess.run(
            [LILLUX, "exec", "stream", "--cmd", "sh", "--arg", "-c",
             "--arg", "echo before; sleep 60",
             "--timeout", "0.5"],
            capture_output=True, text=True,
        )
        assert r.returncode == 124
        assert "before" in r.stdout
