# ryeos:signed:2026-06-07T05:42:18Z:a353d4bb5f0e0547cc2922f3b731f5f68b2603528759d145338c9cd55fd17c9d:g6pja547e7jlLkltoc78S0W/u7JTNRQGD1yvQrxH4B4zsm2TCv0yeya/xChlGj0CZ8FFauZbo0eT6zTgtT4ZDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: test/anchor_demo
#   version: "1.0.0"
#   tool_type: python
#   executor_id: ryeos/core/runtimes/python/function
#   tool_description: "Helper module for anchor demo"
"""Helper module for anchor demo tool.

Tests that PYTHONPATH injection via anchor system allows
sibling module imports to work.
"""


def format_greeting(name: str, style: str = "friendly") -> str:
    """Format a greeting message."""
    if style == "formal":
        return f"Good day, {name}. How do you do?"
    return f"Hey {name}, nice to see you!"
