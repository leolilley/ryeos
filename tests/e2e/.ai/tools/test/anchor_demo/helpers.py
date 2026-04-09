# rye:signed:2026-04-09T00:59:52Z:369de002f174f9d4d8d3e2baf39492de1bddb5fbf8c9568fd64f236c256467c5:1XgRDW_AybNzcAi2FSwx8RO99S_cS9K-gu9F-NH4lKD_TthxzSKUmkw1V8SrIjUTSRltrH9BDbjyTVh-0BDXDA:4b987fd4e40303ac
"""Helper module for anchor demo tool.

Tests that PYTHONPATH injection via anchor system allows
sibling module imports to work.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "test/anchor_demo"
__tool_description__ = "Helper module for anchor demo"


def format_greeting(name: str, style: str = "friendly") -> str:
    """Format a greeting message."""
    if style == "formal":
        return f"Good day, {name}. How do you do?"
    return f"Hey {name}, nice to see you!"
