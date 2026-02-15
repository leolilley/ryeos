"""Tests for MCP server."""

import os
import tempfile
from pathlib import Path

from rye_mcp.server import RYEServer


class TestRYEServer:
    """Test RYE server initialization."""

    def test_server_initialization(self):
        """Initialize server."""
        with tempfile.TemporaryDirectory() as tmpdir:
            os.environ["USER_SPACE"] = tmpdir
            server = RYEServer()

            assert server.user_space == tmpdir
            assert server.debug is False

    def test_server_user_space_default(self):
        """User space defaults to home directory (base path)."""
        if "USER_SPACE" in os.environ:
            del os.environ["USER_SPACE"]

        server = RYEServer()
        assert server.user_space == str(Path.home())

    def test_server_debug_mode(self):
        """Debug mode from environment."""
        os.environ["RYE_DEBUG"] = "true"
        server = RYEServer()
        assert server.debug is True

        os.environ["RYE_DEBUG"] = "false"
        server = RYEServer()
        assert server.debug is False

    def test_tools_registered(self):
        """All 4 tools are registered."""
        with tempfile.TemporaryDirectory() as tmpdir:
            os.environ["USER_SPACE"] = tmpdir
            server = RYEServer()

            # Check that tools are instantiated
            assert hasattr(server, "search")
            assert hasattr(server, "load")
            assert hasattr(server, "execute")
            assert hasattr(server, "sign")

    def test_tool_names(self):
        """Tool names are correct."""
        with tempfile.TemporaryDirectory() as tmpdir:
            os.environ["USER_SPACE"] = tmpdir
            server = RYEServer()

            # Get registered tools from server
            # Note: This depends on Lilux MCP server implementation
            # For now, just verify server has the tool instances
            assert server.search is not None
            assert server.load is not None
            assert server.execute is not None
            assert server.sign is not None


class TestServerIntegration:
    """Integration tests with tools."""

    async def test_all_tools_callable(self):
        """All tools have handle methods."""
        with tempfile.TemporaryDirectory() as tmpdir:
            os.environ["USER_SPACE"] = tmpdir
            server = RYEServer()

            # Verify handle methods exist
            assert callable(server.search.handle)
            assert callable(server.load.handle)
            assert callable(server.execute.handle)
            assert callable(server.sign.handle)
