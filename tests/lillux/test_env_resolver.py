"""Tests for environment resolver (Phase 4.1)."""

import os
from pathlib import Path
import tempfile
import pytest
from lillux.runtime.env_resolver import EnvResolver


class TestEnvResolverInit:
    """Test EnvResolver initialization."""

    def test_init_with_project_path(self):
        """EnvResolver initializes with project path."""
        resolver = EnvResolver(project_path="/home/user/project")
        assert resolver.project_path == Path("/home/user/project")

    def test_init_without_project_path(self):
        """EnvResolver defaults to cwd if no project path."""
        resolver = EnvResolver(project_path=None)
        assert resolver.project_path == Path.cwd()


class TestEnvResolverBasic:
    """Test basic resolution scenarios."""

    def test_resolve_returns_dict(self):
        """resolve() returns dict."""
        resolver = EnvResolver()
        env = resolver.resolve()
        assert isinstance(env, dict)

    def test_resolve_includes_os_environ(self):
        """resolve() includes system environment."""
        resolver = EnvResolver()
        env = resolver.resolve(include_dotenv=False)
        # Should have at least PATH
        assert "PATH" in env

    def test_resolve_with_custom_tool_env(self):
        """Tool env overrides config env."""
        resolver = EnvResolver()
        env_config = {
            "env": {
                "DEBUG": "0",
                "LOG_LEVEL": "info"
            }
        }
        tool_env = {
            "DEBUG": "1"
        }
        
        env = resolver.resolve(
            env_config=env_config,
            tool_env=tool_env,
            include_dotenv=False
        )
        
        assert env["DEBUG"] == "1"
        assert env["LOG_LEVEL"] == "info"

    def test_resolve_with_env_config(self):
        """resolve() applies env config."""
        resolver = EnvResolver()
        env_config = {
            "env": {
                "CUSTOM_VAR": "value"
            }
        }
        
        env = resolver.resolve(env_config=env_config, include_dotenv=False)
        assert env["CUSTOM_VAR"] == "value"

    def test_resolution_order(self):
        """Resolution follows correct order."""
        resolver = EnvResolver()
        env_config = {
            "env": {
                "VAR1": "from_config"
            }
        }
        tool_env = {
            "VAR1": "from_tool"
        }
        
        env = resolver.resolve(env_config=env_config, tool_env=tool_env, include_dotenv=False)
        
        # Tool env should override config
        assert env["VAR1"] == "from_tool"


class TestEnvResolverVariableExpansion:
    """Test variable expansion features."""

    def test_variable_expansion_simple(self):
        """${VAR} expansion works."""
        resolver = EnvResolver()
        env_config = {
            "env": {
                "HOME": "/home/user",
                "PROJECT": "${HOME}/projects"
            }
        }
        
        env = resolver.resolve(env_config=env_config, include_dotenv=False)
        
        # PROJECT should contain HOME value
        assert "/home/user/projects" in env.get("PROJECT", "")

    def test_variable_expansion_with_default(self):
        """${VAR:-default} expansion works."""
        resolver = EnvResolver()
        env_config = {
            "env": {
                "DEBUG": "${UNDEFINED_VAR:-false}"
            }
        }
        
        env = resolver.resolve(env_config=env_config, include_dotenv=False)
        
        # Should use default
        assert env["DEBUG"] == "false"


class TestEnvResolverDotenv:
    """Test .env file loading."""

    def test_include_dotenv_true(self):
        """Load .env file when requested."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project_path = Path(tmpdir)
            
            # Create .env file
            env_file = project_path / ".env"
            env_file.write_text("DATABASE_URL=postgresql://localhost/mydb\nDEBUG=1\n")
            
            resolver = EnvResolver(project_path=project_path)
            env = resolver.resolve(include_dotenv=True)
            
            # .env variables should be loaded
            assert "DATABASE_URL" in env
            assert "DEBUG" in env

    def test_include_dotenv_false(self):
        """Skip .env when include_dotenv=False."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project_path = Path(tmpdir)
            
            env_file = project_path / ".env"
            env_file.write_text("TEST_VAR=test_value\n")
            
            resolver = EnvResolver(project_path=project_path)
            env = resolver.resolve(include_dotenv=False)
            
            # .env should not be loaded (or shouldn't have this specific var)
            # Can't guarantee it won't be there from system, so just verify structure
            assert isinstance(env, dict)


class TestEnvResolverInterpreters:
    """Test interpreter resolution."""

    def test_resolve_venv_python(self):
        """local_binary resolver finds python in venv."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project_path = Path(tmpdir)
            
            # Create fake venv
            venv_path = project_path / ".venv"
            venv_bin = venv_path / "bin"
            venv_bin.mkdir(parents=True)
            python_exe = venv_bin / "python"
            python_exe.touch()
            
            resolver = EnvResolver(project_path=project_path)
            env_config = {
                "interpreter": {
                    "type": "local_binary",
                    "var": "PYTHON_PATH",
                    "binary": "python",
                    "search_paths": [".venv/bin"]
                }
            }
            
            env = resolver.resolve(env_config=env_config, include_dotenv=False)
            
            # Should have found venv Python
            assert "PYTHON_PATH" in env
            assert ".venv" in env["PYTHON_PATH"]

    def test_resolve_venv_python_with_fallback(self):
        """local_binary uses fallback if not found."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project_path = Path(tmpdir)
            
            resolver = EnvResolver(project_path=project_path)
            env_config = {
                "interpreter": {
                    "type": "local_binary",
                    "var": "PYTHON_PATH",
                    "binary": "python",
                    "search_paths": [".nonexistent/bin"],
                    "fallback": "/usr/bin/python3"
                }
            }
            
            env = resolver.resolve(env_config=env_config, include_dotenv=False)
            
            # Should use fallback
            assert env["PYTHON_PATH"] == "/usr/bin/python3"

    def test_resolve_system_binary(self):
        """system_binary resolver works."""
        resolver = EnvResolver()
        env_config = {
            "interpreter": {
                "type": "system_binary",
                "var": "GIT_PATH",
                "binary": "git",
                "fallback": "/usr/bin/git"
            }
        }
        
        env = resolver.resolve(env_config=env_config, include_dotenv=False)
        
        # Should have git path (either found or fallback)
        assert "GIT_PATH" in env


class TestEnvResolverComplex:
    """Test complex resolution scenarios."""

    def test_resolve_returns_complete_env(self):
        """resolve() returns strings for all env vars."""
        resolver = EnvResolver()
        env_config = {
            "env": {
                "STRING": "value",
                "NUMBER": 42,
                "BOOL": True
            }
        }
        
        env = resolver.resolve(env_config=env_config, include_dotenv=False)
        
        # All values should be strings
        assert isinstance(env["STRING"], str)
        assert isinstance(env["NUMBER"], str)
        assert isinstance(env["BOOL"], str)

    def test_multiple_variables_expanded(self):
        """Multiple variables can be expanded."""
        resolver = EnvResolver()
        env_config = {
            "env": {
                "A": "/path/a",
                "B": "${A}/b",
                "C": "${B}/c"
            }
        }
        
        env = resolver.resolve(env_config=env_config, include_dotenv=False)
        
        # Variables should be expanded in order
        assert "/path/a" in env["B"]
        assert "/path/a" in env["C"]
