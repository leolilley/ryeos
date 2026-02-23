# rye:signed:2026-02-14T00:22:36Z:4380a098d2e82cf0db4f62fb23d01465c53aa5dcc24b231622fd8c36b770469f:AkpI2WEzTuu21xOHWIxvIDLSX237bVZHfzxrSiztvgY9WBgSWG55Igo756YU8axYkc-uet2odmmN8WMYlTheCw==:440443d0858f0199
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
