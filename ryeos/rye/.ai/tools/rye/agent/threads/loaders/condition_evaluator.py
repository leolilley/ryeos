# rye:signed:2026-02-22T09:00:56Z:5900ba6007a4cfba000794e435d755b651cbd9e7059898c2e59083cc06782d35:ON6qjDGMqY07yUtqxMf7QchS6mCoTYm6JpfTGWDJP5qOEAOVWH4xWf1Q7GiiwvyBAJ6PSaT1ZPYcgZ_0ySjFBw==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Condition evaluator for hook matching"

import re
from typing import Any, Dict


def matches(doc: Dict, condition: Dict) -> bool:
    """Evaluate a condition against a document.

    Supports:
    - path/op/value: Basic comparison
    - any: Match if any child matches
    - all: Match only if all children match
    - not: Match if child does not match
    """
    if not condition:
        return True
    if "any" in condition:
        return any(matches(doc, c) for c in condition["any"])
    if "all" in condition:
        return all(matches(doc, c) for c in condition["all"])
    if "not" in condition:
        return not matches(doc, condition["not"])

    path = condition.get("path", "")
    op = condition.get("op", "eq")
    expected = condition.get("value")
    actual = resolve_path(doc, path)
    return apply_operator(actual, op, expected)


def resolve_path(doc: Dict, path: str) -> Any:
    """Resolve a dotted path in a nested dict/list structure.

    Supports dict key lookups and numeric list indices:
        state.items.0.name  →  state["items"][0]["name"]
    """
    if not path:
        return doc
    parts = path.split(".")
    current = doc
    for part in parts:
        if isinstance(current, dict):
            current = current.get(part)
        elif isinstance(current, list):
            try:
                current = current[int(part)]
            except (ValueError, IndexError):
                return None
        else:
            return None
    return current


def apply_operator(actual: Any, op: str, expected: Any) -> bool:
    """Apply a comparison operator."""
    ops = {
        "eq": lambda a, e: a == e,
        "ne": lambda a, e: a != e,
        "gt": lambda a, e: a is not None and a > e,
        "gte": lambda a, e: a is not None and a >= e,
        "lt": lambda a, e: a is not None and a < e,
        "lte": lambda a, e: a is not None and a <= e,
        "in": lambda a, e: a in e if isinstance(e, (list, tuple, set)) else False,
        "contains": lambda a, e: e in str(a) if a is not None else False,
        "regex": lambda a, e: bool(re.search(e, str(a))) if a is not None else False,
        "exists": lambda a, e: a is not None,
    }
    return ops.get(op, lambda a, e: False)(actual, expected)
