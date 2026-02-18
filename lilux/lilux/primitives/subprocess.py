"""Subprocess execution primitive (Phase 3.1).

Stateless, async-first subprocess execution with two-stage templating.
"""

import asyncio
import os
import re
import time
from dataclasses import dataclass
from typing import Dict, Any, List


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


class SubprocessPrimitive:
    """Execute subprocess commands with templating and environment handling."""

    async def execute(
        self,
        config: Dict[str, Any],
        params: Dict[str, Any],
    ) -> SubprocessResult:
        """Execute subprocess command.
        
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

            # Build command args
            cmd: List[str] = [command] + args if args else [command]

            try:
                # Execute process
                process = await asyncio.create_subprocess_exec(
                    *cmd,
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                    cwd=cwd,
                    env=process_env,
                    stdin=asyncio.subprocess.PIPE if input_data else None,
                )

                input_bytes = input_data.encode() if input_data else None
                
                # Wait with timeout
                try:
                    stdout, stderr = await asyncio.wait_for(
                        process.communicate(input=input_bytes),
                        timeout=timeout,
                    )
                except asyncio.TimeoutError:
                    # Kill the process on timeout
                    process.kill()
                    await process.wait()
                    duration_ms = (time.time() - start_time) * 1000
                    return SubprocessResult(
                        success=False,
                        stdout="",
                        stderr=f"Command timed out after {timeout} seconds",
                        return_code=-1,
                        duration_ms=duration_ms,
                    )
                
                return_code = process.returncode or 0

            except FileNotFoundError:
                # Command not found
                duration_ms = (time.time() - start_time) * 1000
                return SubprocessResult(
                    success=False,
                    stdout="",
                    stderr=f"Command not found: {command}",
                    return_code=127,
                    duration_ms=duration_ms,
                )

            duration_ms = (time.time() - start_time) * 1000

            return SubprocessResult(
                success=return_code == 0,
                stdout=stdout.decode("utf-8", errors="replace"),
                stderr=stderr.decode("utf-8", errors="replace"),
                return_code=return_code,
                duration_ms=duration_ms,
            )

        except Exception as e:
            # Unexpected error
            duration_ms = (time.time() - start_time) * 1000
            return SubprocessResult(
                success=False,
                stdout="",
                stderr=str(e),
                return_code=-1,
                duration_ms=duration_ms,
            )

    def _template_env_vars(self, text: str, env: Dict[str, str]) -> str:
        """Expand ${VAR:-default} environment variables.
        
        Args:
            text: Text with ${VAR:-default} patterns.
            env: Environment variables dict.
            
        Returns:
            Text with variables expanded.
        """
        if not text:
            return text

        def replace_var(match):
            var_with_default = match.group(1)
            # Handle ${VAR:-default} format
            if ':-' in var_with_default:
                var_name, default = var_with_default.split(':-', 1)
            else:
                var_name = var_with_default
                default = ""
            
            return env.get(var_name, default)

        # Pattern: ${VAR_NAME:-default_value} or ${VAR_NAME}
        # Only match uppercase env var names (no dots, no lowercase) to avoid
        # consuming context interpolation templates like ${state.issues}.
        return re.sub(r'\$\{([A-Z_][A-Z0-9_]*(?::-[^}]*)?)\}', replace_var, text)

    def _template_params(self, text: str, params: Dict[str, Any]) -> str:
        """Substitute {param_name} with parameter values.
        
        Missing parameters are left unchanged in the text.
        
        Args:
            text: Text with {param_name} patterns.
            params: Parameter values dict.
            
        Returns:
            Text with parameters substituted (missing ones unchanged).
        """
        if not text:
            return text

        def replace_param(match):
            param_name = match.group(1)
            if param_name in params:
                return str(params[param_name])
            return match.group(0)  # Leave unchanged

        return re.sub(r'\{([^}]+)\}', replace_param, text)

    def _prepare_env(self, config_env: Dict[str, str]) -> Dict[str, str]:
        """Prepare process environment.
        
        Heuristic:
        - <50 vars: merge config_env over os.environ
        - >=50 vars: use config_env directly (assumed fully resolved)
        
        Args:
            config_env: Environment from config.
            
        Returns:
            Environment dict for process.
        """
        if len(config_env) < 50:
            # Merge with os.environ
            result = os.environ.copy()
            result.update(config_env)
            return result
        else:
            # Use as-is (fully resolved from orchestrator)
            return config_env
