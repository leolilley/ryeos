"""Environment resolver service (Phase 4.1).

Resolves environment variables from multiple sources using ENV_CONFIG rules.
Pure resolver with no side effects.

Interpreter resolver types (all data-driven):
    local_binary  — Search for a binary in configured local directories
    system_binary — Find a binary on system PATH via which/where
    command       — Run a resolve command and use stdout as the path
"""

import os
import re
import subprocess
from pathlib import Path
from typing import Any, Dict, Optional


class EnvResolver:
    """Pure environment resolver - no side effects, no venv creation."""

    def __init__(self, project_path: Optional[Path] = None):
        """Initialize resolver with project context.

        Args:
            project_path: Root directory for relative path resolution.
                         If None, uses current working directory.
        """
        self.project_path = Path(project_path) if project_path else Path.cwd()

    def resolve(
        self,
        env_config: Optional[Dict[str, Any]] = None,
        tool_env: Optional[Dict[str, str]] = None,
        include_dotenv: bool = True,
    ) -> Dict[str, str]:
        """Resolve environment from all sources.

        Resolution order:
        1. Start with os.environ
        2. Load .env files (if include_dotenv=True)
        3. Apply ENV_CONFIG rules (interpreter resolution + static vars)
        4. Apply tool-level overrides

        Args:
            env_config: Configuration dict with:
                       - interpreter: resolver config (type, var, paths, etc)
                       - env: static environment variables
            tool_env: Tool-level environment overrides.
            include_dotenv: Load .env files (default: True).

        Returns:
            Complete environment dict as strings.
        """
        # Start with system environment
        env = os.environ.copy()

        # Load .env file if present and requested
        if include_dotenv:
            env = self._load_dotenv(env)

        # Apply ENV_CONFIG rules if provided
        if env_config:
            # Apply interpreter resolution
            if "interpreter" in env_config:
                interpreter_config = env_config["interpreter"]
                env = self._resolve_interpreter(env, interpreter_config)

            # Apply static environment variables
            if "env" in env_config:
                static_vars = env_config["env"]
                env = self._apply_static_env(env, static_vars)

        # Apply tool-level overrides (highest priority)
        if tool_env:
            env.update(tool_env)

        return env

    def _load_dotenv(self, env: Dict[str, str]) -> Dict[str, str]:
        """Load .env file if exists.

        Args:
            env: Current environment dict.

        Returns:
            Environment with .env variables loaded.
        """
        env_file = self.project_path / ".env"

        if not env_file.exists():
            return env

        try:
            content = env_file.read_text()
            for line in content.split("\n"):
                line = line.strip()
                if not line or line.startswith("#"):
                    continue

                if "=" in line:
                    key, value = line.split("=", 1)
                    key = key.strip()
                    value = value.strip()
                    if key and not key.startswith("export "):
                        env[key] = value
        except Exception:
            pass

        return env

    def _resolve_interpreter(
        self, env: Dict[str, str], config: Dict[str, Any]
    ) -> Dict[str, str]:
        """Resolve interpreter based on config type.

        Args:
            env: Current environment.
            config: Interpreter resolver config with keys:
                   - type: local_binary, system_binary, command
                   - var: environment variable name
                   - other type-specific keys

        Returns:
            Environment with interpreter variable set.
        """
        resolver_type = config.get("type")
        var_name = config.get("var")

        if not var_name:
            return env

        if resolver_type == "local_binary":
            path = self._resolve_local_binary(config)
        elif resolver_type == "system_binary":
            path = self._resolve_system_binary(config)
        elif resolver_type == "command":
            path = self._resolve_command(config)
        else:
            return env

        if path:
            env[var_name] = path
        elif "fallback" in config:
            env[var_name] = config["fallback"]

        return env

    def _resolve_local_binary(self, config: Dict[str, Any]) -> Optional[str]:
        """Find a binary in configured local directories.

        Config keys:
            binary: Name of the binary to find (required)
            candidates: Additional binary names to try (e.g. ["python3"])
            search_paths: Relative directory paths to search (required)
            search_roots: Additional absolute roots to search before project_path
        """
        binary = config.get("binary")
        if not binary:
            return None

        search_paths = config.get("search_paths", [])
        if not search_paths:
            return None

        # Build candidate binary names
        candidates = [binary]
        for extra in config.get("candidates", []):
            if extra not in candidates:
                candidates.append(extra)
        if os.name == "nt":
            nt_candidates = []
            for c in candidates:
                nt_candidates.extend([c, f"{c}.exe", f"{c}.cmd"])
            candidates = nt_candidates

        # Build search roots: explicit roots first, then project_path
        roots = [Path(r) for r in config.get("search_roots", [])]
        roots.append(self.project_path)

        for root in roots:
            for search_path_str in search_paths:
                search_dir = root / search_path_str
                for name in candidates:
                    bin_path = search_dir / name
                    if bin_path.exists():
                        return str(bin_path)

        return None

    def _resolve_system_binary(self, config: Dict[str, Any]) -> Optional[str]:
        """Find binary in system PATH using which/where.

        Config keys:
            binary: Name of the binary to find (required)
        """
        binary = config.get("binary")

        if not binary:
            return None

        try:
            cmd = "where" if os.name == "nt" else "which"
            result = subprocess.run(
                [cmd, binary],
                capture_output=True,
                text=True,
                timeout=5,
            )

            if result.returncode == 0:
                return result.stdout.strip().split("\n")[0]
        except Exception:
            pass

        return None

    def _resolve_command(self, config: Dict[str, Any]) -> Optional[str]:
        """Run a resolve command and use stdout as the binary path.

        Config keys:
            resolve_cmd: Command list to execute (e.g. ["pyenv", "which", "python"])
        """
        cmd = config.get("resolve_cmd")

        if not cmd or not isinstance(cmd, list):
            return None

        try:
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=5,
            )

            if result.returncode == 0:
                path = result.stdout.strip().split("\n")[0]
                if path:
                    return path
        except Exception:
            pass

        return None

    def _apply_static_env(
        self,
        env: Dict[str, str],
        static_vars: Dict[str, str],
    ) -> Dict[str, str]:
        """Apply static environment variables with variable expansion.

        Expands ${VAR} and ${VAR:-default} syntax.

        Args:
            env: Current environment.
            static_vars: Static variables to apply.

        Returns:
            Environment with static vars applied and expanded.
        """
        # Apply variables in order to allow references
        for key, value in static_vars.items():
            if isinstance(value, str):
                # Expand variables in this value
                expanded = self._expand_variables(value, env)
                env[key] = expanded
            else:
                env[key] = str(value)

        return env

    def _expand_variables(self, text: str, env: Dict[str, str]) -> str:
        """Expand ${VAR} and ${VAR:-default} in text.

        Args:
            text: Text with variable references.
            env: Environment dict for lookups.

        Returns:
            Text with variables expanded.
        """
        if not text:
            return text

        def replace_var(match: Any) -> str:
            var_with_default = match.group(1)

            # Handle ${VAR:-default} format
            if ":-" in var_with_default:
                var_name, default_value = var_with_default.split(":-", 1)
            else:
                var_name = var_with_default
                default_value = ""

            return env.get(var_name, default_value)

        # Pattern: ${VAR_NAME:-default_value} or ${VAR_NAME}
        # Only match uppercase env var names (no dots, no lowercase) to avoid
        # consuming context interpolation templates like ${state.issues}.
        return re.sub(r"\$\{([A-Z_][A-Z0-9_]*(?::-[^}]*)?)\}", replace_var, text)
