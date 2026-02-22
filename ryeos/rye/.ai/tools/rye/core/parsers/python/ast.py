# rye:signed:2026-02-22T23:50:44Z:c37a8b96280d5c5904f0b9606858f04b5be6550abe3e0c40799ba2fce4776490:HKSn8mr8N0nMtAY3uvUq_Iph3RqnLrh1Uf2mDQnLzrO1nmsT5AOfqXQXqVeKuT1kxX9XwmDKLkyudN3ZwyrTAA==:9fbfabe975fa5a7f
"""Python AST parser for extracting metadata from Python tools.

Extracts module-level variables and docstring using AST parsing.
"""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers/python"
__tool_description__ = "Python AST parser - extracts metadata from Python source code"

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
