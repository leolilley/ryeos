# ryeos:signed:2026-11-16T03:55:16Z:c360c882185b104bdf251077fedd4da9b8cb26d22d95fe46b30a829bb84824a1:+7BwgsV6O2NpTY1MWDuOhnLW0CWbrThbJxIjgUAy2mt82WIOGyZ9/9wWL6DZ1BpKC2o7PKk4qeKuJeHpQ4vuAQ==:ae0ca349cdef0b514a7589669b7b5ff12ac666d38374eb657abd94b53aeccbfb
# ryeos-tool:
#   category: test/fail_demo
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   execution_protocol: "protocol:ryeos/core/tool_callback"
#   description: "Always fails, so a graph retry and an error path have something to exercise"
"""Fail demo tool: raises on every call."""

CONFIG_SCHEMA = {"type": "object", "properties": {}}


def execute(params: dict, project_path: str) -> dict:
    raise ValueError("fail_demo always fails")
