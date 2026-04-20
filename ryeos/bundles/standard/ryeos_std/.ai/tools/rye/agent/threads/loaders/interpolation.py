# rye:signed:2026-04-19T09:49:53Z:1ba9b2b1b656d5801c7af2646727e4bff8ff17acd349a033b04057cd6593960e:ZHTxuEyUfQkOaCDcKa0NukqDu0fvgkio99UL0wyBKzByeUdg1Gpulrl8d8y9TuOqT4tvoQIqz/Y529qqYYk0DQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
"""Template interpolation — re-exports from rye/core/runtimes/python/lib/.

Canonical implementation lives in core. This module re-exports
so relative imports (from .interpolation import ...) keep working.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Template interpolation for hook actions"

from interpolation import interpolate, interpolate_action  # noqa: F401
