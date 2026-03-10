# rye:signed:2026-03-10T04:07:19Z:e8df58d7dd74cef449d96731b430a10a2b1696abc8558503ae4a2c910be96908:RZk68WmesIc2NKbaghucOJmD7neNaYCDlsjy_wgZjnJnYaoXqBS1lcL9ySuvVQCMlCg9a3Br-_Kn4OHs8A-KBA==:4b987fd4e40303ac
"""Test tool for registry flow validation.

A simple Python tool to test push/pull operations.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_tool_runtime"
__category__ = "test"
__tool_description__ = "Test tool for registry flow"


async def execute(action: str, project_path: str, params: dict = None) -> dict:
    """Execute the test tool."""
    params = params or {}
    
    if action == "greet":
        name = params.get("name", "World")
        return {"message": f"Hello, {name}!"}
    
    return {"error": f"Unknown action: {action}"}
