"""Environment resolver service (Phase 4.1).

Resolves environment variables from multiple sources using ENV_CONFIG rules.
Pure resolver with no side effects.
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
                   - type: venv_python, node_modules, system_binary, version_manager
                   - var: environment variable name
                   - other type-specific keys
        
        Returns:
            Environment with interpreter variable set.
        """
        resolver_type = config.get("type")
        var_name = config.get("var")

        if not var_name:
            return env

        if resolver_type == "venv_python":
            path = self._resolve_venv_python(config)
        elif resolver_type == "node_modules":
            path = self._resolve_node_modules(config)
        elif resolver_type == "system_binary":
            path = self._resolve_system_binary(config)
        elif resolver_type == "version_manager":
            path = self._resolve_version_manager(config)
        else:
            return env

        if path:
            env[var_name] = path
        elif "fallback" in config:
            env[var_name] = config["fallback"]

        return env

    def _resolve_venv_python(self, config: Dict[str, Any]) -> Optional[str]:
        """Find Python in virtual environment.
        
        Searches:
        - Unix: .venv/bin/python
        - Windows: .venv\Scripts\python.exe
        """
        venv_path_str = config.get("venv_path", ".venv")
        venv_path = self.project_path / venv_path_str

        # Try Unix paths first
        unix_python = venv_path / "bin" / "python"
        if unix_python.exists():
            return str(unix_python)

        # Try Windows paths
        windows_python = venv_path / "Scripts" / "python.exe"
        if windows_python.exists():
            return str(windows_python)

        # Try with version suffix (python3)
        unix_python3 = venv_path / "bin" / "python3"
        if unix_python3.exists():
            return str(unix_python3)

        return None

    def _resolve_node_modules(self, config: Dict[str, Any]) -> Optional[str]:
        """Find Node in node_modules/.bin directory."""
        search_paths = config.get("search_paths", ["node_modules/.bin"])

        for search_path_str in search_paths:
            search_path = self.project_path / search_path_str

            # Try node executable
            node_path = search_path / "node"
            if node_path.exists():
                return str(node_path)

            # Try node.exe (Windows)
            node_exe_path = search_path / "node.exe"
            if node_exe_path.exists():
                return str(node_exe_path)

        return None

    def _resolve_system_binary(self, config: Dict[str, Any]) -> Optional[str]:
        """Find binary in system PATH using which/where."""
        binary = config.get("binary")

        if not binary:
            return None

        try:
            # Use 'which' on Unix, 'where' on Windows
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

    def _resolve_version_manager(
        self, config: Dict[str, Any]
    ) -> Optional[str]:
        """Find interpreter via version manager (pyenv, nvm, rbenv, asdf)."""
        manager = config.get("manager")
        version = config.get("version")

        if not manager or not version:
            return None

        try:
            if manager == "pyenv":
                # Use pyenv to find Python
                result = subprocess.run(
                    ["pyenv", "which", version],
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                if result.returncode == 0:
                    return result.stdout.strip()

            elif manager == "nvm":
                # Use nvm to find Node
                result = subprocess.run(
                    ["nvm", "which", version],
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                if result.returncode == 0:
                    return result.stdout.strip()

            elif manager == "rbenv":
                # Use rbenv to find Ruby
                result = subprocess.run(
                    ["rbenv", "which", version],
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                if result.returncode == 0:
                    return result.stdout.strip()

            elif manager == "asdf":
                # Use asdf to find interpreter
                result = subprocess.run(
                    ["asdf", "which", manager, version],
                    capture_output=True,
                    text=True,
                    timeout=5,
                )
                if result.returncode == 0:
                    return result.stdout.strip()

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
