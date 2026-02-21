# rye:signed:2026-02-21T05:56:40Z:2035cdfd61e709a1747d5e41f09f98244f622ec2b2014f9d1d5b48cdbe3b1b2d:oEP1ZX47rr7NCpAqbseolGyYDvkoDBKVbJ_hahz8M_w2HHd7eEK06eyJf6AU9_uwQNvlqZ-kxVjOVfgGIltUCg==:9fbfabe975fa5a7f
"""Python AST parser for extracting metadata from Python tools.

Extracts module-level variables and docstring using AST parsing.
"""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers"
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
