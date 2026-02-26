# rye:signed:2026-02-26T03:49:26Z:8e1ab83b0df0e582e7c00467338e11bbfd40ef1932a98481f8733abecccc38d3:q2Qc6Q46de92yoS2h1esHDppsWezPUUSH-AgVRVRtPWacVO2ZfUSYx2jTH9A_D03_i7V386gaqz5xCYFpNDfCA==:9fbfabe975fa5a7f
"""Telemetry tools package."""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/telemetry"
__tool_description__ = "Telemetry tools package"

from .mcp_logs import get_logs, get_log_stats

__all__ = ["get_logs", "get_log_stats"]
