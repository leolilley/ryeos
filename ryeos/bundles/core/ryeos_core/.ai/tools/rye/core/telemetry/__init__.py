# rye:signed:2026-02-26T05:02:30Z:8e1ab83b0df0e582e7c00467338e11bbfd40ef1932a98481f8733abecccc38d3:ZVcP92Hl8jhkC1NDXtdUHOt2vZ4OvuX7z7cmBuCUzlV8CUROQfka0rzye8dB338NCxgcPWN7l2IhVvOMv7UNAA==:4b987fd4e40303ac
"""Telemetry tools package."""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/telemetry"
__tool_description__ = "Telemetry tools package"

from .mcp_logs import get_logs, get_log_stats

__all__ = ["get_logs", "get_log_stats"]
