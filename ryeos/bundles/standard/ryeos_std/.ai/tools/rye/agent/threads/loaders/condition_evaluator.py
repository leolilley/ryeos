# rye:signed:2026-02-26T06:42:42Z:c93f7bf6dd8358ff237a5cdc3dc9304b7bd5b6a482a802e1b4f08dcfc132e3ca:SeuoTZpLrDkOPdDPcIlzFiXdcDV7BKCVwP_-4DEqTXqQnDrtn2OSBS_fMwj479Q8EIXGY1ZCW-eoBMZ02Q3WDw==:4b987fd4e40303ac
"""Condition evaluator — re-exports from rye/core/runtimes/python/lib/.

Canonical implementation lives in core. This module re-exports
so relative imports (from .condition_evaluator import ...) keep working.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Condition evaluator and path resolver"

from condition_evaluator import matches, resolve_path, apply_operator  # noqa: F401
