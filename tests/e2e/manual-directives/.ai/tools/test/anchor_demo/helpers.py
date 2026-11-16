# ryeos:signed:2026-11-16T03:24:11Z:a9d75147e2e53bffd399c6f58688135ebcb7502e53f2fe853f7498ab52f09c0c:q1hVoS1dSs0MCoIZ2yEm35+00TQpYrRsSHArbSLCg1rYl3vIyzS9QuZm+IK9i0wwrL+oQZ8IQlTCzCUTq+1ACA==:ae0ca349cdef0b514a7589669b7b5ff12ac666d38374eb657abd94b53aeccbfb
# ryeos-tool:
#   category: test/anchor_demo
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   execution_protocol: "protocol:ryeos/core/tool_callback"
#   description: "Helper module for anchor demo"
"""Helper module for anchor demo tool.

Tests that PYTHONPATH injection via anchor system allows
sibling module imports to work.
"""


def format_greeting(name: str, style: str = "friendly") -> str:
    """Format a greeting message."""
    if style == "formal":
        return f"Good day, {name}. How do you do?"
    return f"Hey {name}, nice to see you!"
