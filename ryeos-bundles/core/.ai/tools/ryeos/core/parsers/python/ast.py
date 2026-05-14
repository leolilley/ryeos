# ryeos:signed:2026-05-13T13:48:49Z:e9878f66b230d75842a57017fb71cee4386a9823d6b091702338cc531197c3b3:ISnyvBTy/MnkahWm132k7ykw9+T3Of9CKaskA1KqsZcaD8VpGxLHQWpKk6BBhe+LkUr9zzbxWBIMh8CJLwnRCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Python AST parser for extracting metadata from Python tools.

Extracts module-level variables and docstring using AST parsing.
"""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "ryeos/core/parsers/python"
__description__ = "Python AST parser - extracts metadata from Python source code"

import ast
from typing import Any, Dict


def parse(content: str) -> Dict[str, Any]:
    """Parse Python source and extract metadata.

    Returns dict of module-level variables, docstring, and raw content.
    """
    result: Dict[str, Any] = {
        "raw": content,
    }

    try:
        tree = ast.parse(content)
    except SyntaxError as e:
        return {**result, "error": f"Syntax error: {e}"}

    # Extract module-level variables
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1:
            target = node.targets[0]
            if isinstance(target, ast.Name):
                try:
                    # Try to evaluate literal values
                    value = ast.literal_eval(node.value)
                    result[target.id] = value
                except (ValueError, TypeError):
                    # Can't evaluate - skip
                    pass

    # Extract docstring
    if tree.body and isinstance(tree.body[0], ast.Expr):
        if isinstance(tree.body[0].value, ast.Constant):
            if isinstance(tree.body[0].value.value, str):
                result["__docstring__"] = tree.body[0].value.value.strip()

    return result
