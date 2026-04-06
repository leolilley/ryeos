"""Tests for PRIMITIVE_MAP after HTTP primitive removal."""

from rye.executor.primitive_executor import PrimitiveExecutor
from rye.primitives.execute import ExecutePrimitive


class TestPrimitiveMap:
    def test_single_entry(self):
        """PRIMITIVE_MAP has exactly one entry after HTTP primitive removal."""
        assert len(PrimitiveExecutor.PRIMITIVE_MAP) == 1

    def test_execute_primitive_registered(self):
        """rye/core/primitives/execute maps to ExecutePrimitive."""
        assert PrimitiveExecutor.PRIMITIVE_MAP["rye/core/primitives/execute"] is ExecutePrimitive

    def test_http_client_removed(self):
        """rye/core/primitives/http_client is no longer registered."""
        assert "rye/core/primitives/http_client" not in PrimitiveExecutor.PRIMITIVE_MAP

    def test_subprocess_removed(self):
        """rye/core/primitives/subprocess is no longer registered."""
        assert "rye/core/primitives/subprocess" not in PrimitiveExecutor.PRIMITIVE_MAP
