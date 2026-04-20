# rye:signed:2026-04-19T09:49:53Z:8e1ab83b0df0e582e7c00467338e11bbfd40ef1932a98481f8733abecccc38d3:ZVcP92Hl8jhkC1NDXtdUHOt2vZ4OvuX7z7cmBuCUzlV8CUROQfka0rzye8dB338NCxgcPWN7l2IhVvOMv7UNAA==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
"""Telemetry tools package."""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/telemetry"
__tool_description__ = "Telemetry tools package"

from .mcp_logs import get_logs, get_log_stats

__all__ = ["get_logs", "get_log_stats"]
