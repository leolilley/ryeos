"""Subprocess execution primitive.

All process operations go through rye-proc. No POSIX fallbacks.
"""

import asyncio
import json
import os
import re
import shutil
import time
from dataclasses import dataclass
from typing import Any, Dict, List, Optional

from lilux.primitives.errors import ConfigurationError


@dataclass
class SubprocessResult:
    """Result of subprocess execution.

    Attributes:
        success: True if return code is 0.
        stdout: Standard output from process.
        stderr: Standard error from process.
        return_code: Exit code from process.
        duration_ms: Time taken for execution in milliseconds.
    """

    success: bool
    stdout: str
    stderr: str
    return_code: int
    duration_ms: float


@dataclass
class SpawnResult:
    """Result of detached process spawn.

    Attributes:
        success: True if spawn succeeded.
        pid: PID of spawned process, or None on failure.
        error: Error message on failure, or None on success.
    """

    success: bool
    pid: Optional[int] = None
    error: Optional[str] = None


@dataclass
class KillResult:
    """Result of process kill operation.

    Attributes:
        success: True if kill succeeded.
        pid: PID that was targeted.
        method: How the process was stopped ("terminated", "killed", "already_dead").
        error: Error message on failure, or None on success.
    """

    success: bool
    pid: int = 0
    method: str = ""
    error: Optional[str] = None


@dataclass
class StatusResult:
    """Result of process status check.

    Attributes:
        pid: PID that was checked.
        alive: True if process is running.
    """

    pid: int
    alive: bool


class SubprocessPrimitive:
    """All process operations go through rye-proc. No POSIX fallbacks."""

    def __init__(self):
        self._rye_proc: Optional[str] = shutil.which("rye-proc")
        if not self._rye_proc:
            raise ConfigurationError(
                "rye-proc binary not found on PATH. "
                "Ensure ryeos is installed correctly."
            )

    async def execute(
        self,
        config: Dict[str, Any],
        params: Dict[str, Any],
    ) -> SubprocessResult:
        """Execute subprocess command via rye-proc exec.

        Two-stage templating:
        1. Environment variable expansion: ${VAR:-default}
        2. Runtime parameter substitution: {param_name}

        Env merge heuristic:
        - <50 vars: merge config env with os.environ
        - >=50 vars: use config env directly (assumed resolved)

        Args:
            config: Configuration dict with keys:
                - command: Command to execute (required)
                - args: List of arguments (optional)
                - cwd: Working directory (optional)
                - input_data: Data to pipe to stdin (optional)
                - env: Environment variables (optional)
                - timeout: Timeout in seconds (default: 300)
            params: Runtime parameters for templating {param_name}

        Returns:
            SubprocessResult with execution details.
        """
        start_time = time.time()

        try:
            # Extract config
            command = config.get("command")
            args = config.get("args", [])
            cwd = config.get("cwd")
            input_data = config.get("input_data")
            config_env = config.get("env", {})
            timeout = config.get("timeout", 300)

            # Prepare environment FIRST (before templating)
            process_env = self._prepare_env(config_env)

            # Stage 1: Env var templating on command, args, cwd, input_data
            command = self._template_env_vars(command, process_env) if command else None
            args = [self._template_env_vars(arg, process_env) for arg in args]
            cwd = self._template_env_vars(cwd, process_env) if cwd else None
            input_data = (
                self._template_env_vars(input_data, process_env) if input_data else None
            )

            # Stage 2: Runtime param templating
            command = self._template_params(command, params) if command else None
            args = [self._template_params(arg, params) for arg in args]
            cwd = self._template_params(cwd, params) if cwd else None
            input_data = (
                self._template_params(input_data, params) if input_data else None
            )

            # Validate command
            if not command:
                return SubprocessResult(
                    success=False,
                    stdout="",
                    stderr="No command specified",
                    return_code=-1,
                    duration_ms=(time.time() - start_time) * 1000,
                )

            # Build rye-proc exec command
            exec_args: List[str] = [self._rye_proc, "exec", "--cmd", command]
            for arg in args:
                exec_args.extend(["--arg", arg])
            if cwd:
                exec_args.extend(["--cwd", cwd])
            if input_data:
                exec_args.extend(["--stdin", input_data])
            if timeout:
                exec_args.extend(["--timeout", str(timeout)])
            for key, value in process_env.items():
                exec_args.extend(["--env", f"{key}={value}"])

            try:
                proc = await asyncio.create_subprocess_exec(
                    *exec_args,
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )

                # rye-proc handles its own timeout, add buffer for the wrapper
                wrapper_timeout = timeout + 10 if timeout else 310
                try:
                    stdout_bytes, stderr_bytes = await asyncio.wait_for(
                        proc.communicate(),
                        timeout=wrapper_timeout,
                    )
                except asyncio.TimeoutError:
                    proc.kill()
                    await proc.wait()
                    duration_ms = (time.time() - start_time) * 1000
                    return SubprocessResult(
                        success=False,
                        stdout="",
                        stderr=f"rye-proc wrapper timed out after {wrapper_timeout} seconds",
                        return_code=-1,
                        duration_ms=duration_ms,
                    )

                # Parse rye-proc JSON output
                if proc.returncode == 0 and stdout_bytes:
                    try:
                        data = json.loads(stdout_bytes.strip())
                        return SubprocessResult(
                            success=data.get("success", False),
                            stdout=data.get("stdout", ""),
                            stderr=data.get("stderr", ""),
                            return_code=data.get("return_code", -1),
                            duration_ms=data.get("duration_ms", (time.time() - start_time) * 1000),
                        )
                    except json.JSONDecodeError:
                        pass

                # rye-proc itself failed
                duration_ms = (time.time() - start_time) * 1000
                return SubprocessResult(
                    success=False,
                    stdout=stdout_bytes.decode("utf-8", errors="replace") if stdout_bytes else "",
                    stderr=stderr_bytes.decode("utf-8", errors="replace") if stderr_bytes else "",
                    return_code=proc.returncode or -1,
                    duration_ms=duration_ms,
                )

            except FileNotFoundError:
                duration_ms = (time.time() - start_time) * 1000
                return SubprocessResult(
                    success=False,
                    stdout="",
                    stderr=f"rye-proc not found: {self._rye_proc}",
                    return_code=127,
                    duration_ms=duration_ms,
                )

        except Exception as e:
            duration_ms = (time.time() - start_time) * 1000
            return SubprocessResult(
                success=False,
                stdout="",
                stderr=str(e),
                return_code=-1,
                duration_ms=duration_ms,
            )

    async def spawn(
        self,
        cmd: str,
        args: List[str],
        log_path: Optional[str] = None,
        envs: Optional[Dict[str, str]] = None,
    ) -> SpawnResult:
        """Detached spawn via rye-proc spawn."""
        exec_args = [self._rye_proc, "spawn", "--cmd", cmd]
        for arg in args:
            exec_args.extend(["--arg", arg])
        if log_path:
            exec_args.extend(["--log", log_path])
        if envs:
            for k, v in envs.items():
                exec_args.extend(["--env", f"{k}={v}"])

        try:
            proc = await asyncio.create_subprocess_exec(
                *exec_args,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10)
            if proc.returncode == 0 and stdout:
                data = json.loads(stdout.strip())
                return SpawnResult(
                    success=data.get("success", False),
                    pid=data.get("pid"),
                    error=data.get("error"),
                )
        except (asyncio.TimeoutError, OSError, ValueError) as e:
            return SpawnResult(success=False, error=str(e))

        return SpawnResult(success=False, error=f"rye-proc exited {proc.returncode}")

    async def kill(self, pid: int, grace: float = 3.0) -> KillResult:
        """Kill via rye-proc kill."""
        try:
            proc = await asyncio.create_subprocess_exec(
                self._rye_proc, "kill", "--pid", str(pid), "--grace", str(grace),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=grace + 5)
            if proc.returncode == 0 and stdout:
                data = json.loads(stdout.strip())
                return KillResult(
                    success=data.get("success", False),
                    pid=pid,
                    method=data.get("method", ""),
                    error=data.get("error"),
                )
        except (asyncio.TimeoutError, OSError, ValueError) as e:
            return KillResult(success=False, pid=pid, error=str(e))

        return KillResult(success=False, pid=pid, error=f"rye-proc exited {proc.returncode}")

    async def status(self, pid: int) -> StatusResult:
        """Status check via rye-proc status."""
        try:
            proc = await asyncio.create_subprocess_exec(
                self._rye_proc, "status", "--pid", str(pid),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=5)
            if proc.returncode == 0 and stdout:
                data = json.loads(stdout.strip())
                return StatusResult(pid=pid, alive=data.get("alive", False))
        except (asyncio.TimeoutError, OSError, ValueError):
            pass

        return StatusResult(pid=pid, alive=False)

    def _template_env_vars(self, text: str, env: Dict[str, str]) -> str:
        """Expand ${VAR:-default} environment variables."""
        if not text:
            return text

        def replace_var(match):
            var_with_default = match.group(1)
            if ':-' in var_with_default:
                var_name, default = var_with_default.split(':-', 1)
            else:
                var_name = var_with_default
                default = ""
            return env.get(var_name, default)

        # Only match uppercase env var names to avoid consuming
        # context interpolation templates like ${state.issues}.
        return re.sub(r'\$\{([A-Z_][A-Z0-9_]*(?::-[^}]*)?)\}', replace_var, text)

    def _template_params(self, text: str, params: Dict[str, Any]) -> str:
        """Substitute {param_name} with parameter values.

        Missing parameters are left unchanged in the text.
        """
        if not text:
            return text

        def replace_param(match):
            param_name = match.group(1)
            if param_name in params:
                return str(params[param_name])
            return match.group(0)

        return re.sub(r'\{([^}]+)\}', replace_param, text)

    def _prepare_env(self, config_env: Dict[str, str]) -> Dict[str, str]:
        """Prepare process environment.

        Heuristic:
        - <50 vars: merge config_env over os.environ
        - >=50 vars: use config_env directly (assumed fully resolved)
        """
        if len(config_env) < 50:
            result = os.environ.copy()
            result.update(config_env)
            return result
        else:
            return config_env
