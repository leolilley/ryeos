# rye:signed:2026-05-02T11:21:57Z:89e62ca704311c56cd16bd9fb91f1f5ff9586327d7610727c7a33a9ac472f2a2:ewaNHmMnmTZH+5EWwRAL9p6+az30iIH7AMX3huWGoIlivkWLTWEPsy4ncdJvbmmmix15MENgiv0YlEHvNfBHBA==:09674c8998e9dd01bfc40ec9f8c4b6b2c1bd01333842582a9c34b3c7db5aa86c
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
