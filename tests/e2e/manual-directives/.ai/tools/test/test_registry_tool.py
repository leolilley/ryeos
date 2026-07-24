# ryeos:signed:2026-06-07T05:42:18Z:2c0d78385c7fa70a959f8f3f4be2deda53e42863d420cc594d563a1056ffd611:BKIBjy8XPWuwsJxXVYz9LHRp2GjUsWL3z/eZPYOFqY5IVWwZoZqSGHeOKbkkK9qNej9MbQUqNNeKDB4pD/G0Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: test
#   version: "1.0.0"
#   tool_type: python
#   executor_id: ryeos/core/runtimes/python_tool_runtime
#   tool_description: "Test tool for registry flow"
"""Test tool for registry flow validation.

A simple Python tool to test push/pull operations.
"""


async def execute(action: str, project_path: str, params: dict = None) -> dict:
    """Execute the test tool."""
    params = params or {}
    
    if action == "greet":
        name = params.get("name", "World")
        return {"message": f"Hello, {name}!"}
    
    return {"error": f"Unknown action: {action}"}
