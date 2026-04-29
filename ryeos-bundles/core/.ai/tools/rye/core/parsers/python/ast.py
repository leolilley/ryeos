# rye:signed:2026-04-29T02:47:29Z:89e62ca704311c56cd16bd9fb91f1f5ff9586327d7610727c7a33a9ac472f2a2:lDfzxgQTZVCYUPp2g7oShighQGfQep92If5+YDE/aNLRQ8UikEsKeqe+yX3aJPyOjSMOfdwLDgTxCCgCpsYMAg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
"""Python AST parser for extracting metadata from Python tools.

Extracts module-level variables and docstring using AST parsing.
"""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers/python"
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
