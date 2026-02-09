"""
Safe Expression Evaluator for Safety Harness Hooks.

Evaluates simple expressions against a context dictionary.
Intentionally NOT Turing-complete for security.

Supported:
- Comparison: ==, !=, <, >, <=, >=
- Logical: and, or, not
- Membership: in, not in
- Arithmetic: +, -, *, /
- Property access: event.code, limits.turns
- Literals: numbers, strings, true, false, null

NOT supported (security):
- Function calls
- Method access
- Imports
- Assignment
"""

import re
from typing import Any, Dict, List, Optional, Union


class ExpressionError(Exception):
    """Error during expression parsing or evaluation."""
    pass


# Token types - ORDER MATTERS (longer matches first)
TOKEN_TYPES = [
    ("NUMBER", r"\d+\.?\d*"),
    ("STRING", r'"[^"]*"|\'[^\']*\''),
    ("BOOL", r"\b(true|false)\b"),
    ("NULL", r"\bnull\b"),
    ("OP_COMP", r"==|!=|<=|>=|<|>"),
    ("OP_MEMBER", r"\bnot\s+in\b|\bin\b"),  # Must come before OP_LOGIC to catch "not in"
    ("OP_LOGIC", r"\b(and|or|not)\b"),
    ("OP_ARITH", r"[+\-*/]"),
    ("LPAREN", r"\("),
    ("RPAREN", r"\)"),
    ("IDENT", r"[a-zA-Z_][a-zA-Z0-9_]*"),
    ("DOT", r"\."),
    ("WHITESPACE", r"\s+"),
]

TOKEN_REGEX = "|".join(f"(?P<{name}>{pattern})" for name, pattern in TOKEN_TYPES)


def tokenize(expr: str) -> List[tuple]:
    """Tokenize expression into (type, value) pairs."""
    tokens = []
    for match in re.finditer(TOKEN_REGEX, expr):
        token_type = match.lastgroup
        value = match.group()
        if token_type == "WHITESPACE":
            continue
        tokens.append((token_type, value))
    return tokens


class Parser:
    """Recursive descent parser for safe expressions."""
    
    def __init__(self, tokens: List[tuple]):
        self.tokens = tokens
        self.pos = 0
    
    def peek(self) -> Optional[tuple]:
        if self.pos < len(self.tokens):
            return self.tokens[self.pos]
        return None
    
    def consume(self) -> Optional[tuple]:
        token = self.peek()
        self.pos += 1
        return token
    
    def expect(self, token_type: str) -> tuple:
        token = self.consume()
        if token is None or token[0] != token_type:
            raise ExpressionError(f"Expected {token_type}, got {token}")
        return token
    
    def parse(self) -> Any:
        """Parse full expression."""
        result = self.parse_or()
        if self.peek() is not None:
            raise ExpressionError(f"Unexpected token: {self.peek()}")
        return result
    
    def parse_or(self) -> Any:
        """or_expr := and_expr ("or" and_expr)*"""
        left = self.parse_and()
        while self.peek() and self.peek()[1] == "or":
            self.consume()
            right = self.parse_and()
            left = ("or", left, right)
        return left
    
    def parse_and(self) -> Any:
        """and_expr := not_expr ("and" not_expr)*"""
        left = self.parse_not()
        while self.peek() and self.peek()[1] == "and":
            self.consume()
            right = self.parse_not()
            left = ("and", left, right)
        return left
    
    def parse_not(self) -> Any:
        """not_expr := "not" not_expr | comparison"""
        if self.peek() and self.peek()[1] == "not":
            self.consume()
            operand = self.parse_not()
            return ("not", operand)
        return self.parse_comparison()
    
    def parse_comparison(self) -> Any:
        """comparison := additive (comp_op additive)?"""
        left = self.parse_additive()
        
        token = self.peek()
        if token and token[0] in ("OP_COMP", "OP_MEMBER"):
            op = self.consume()[1]
            if op == "not in":
                op = "not_in"
            right = self.parse_additive()
            return (op, left, right)
        
        return left
    
    def parse_additive(self) -> Any:
        """additive := term (("+"|"-") term)*"""
        left = self.parse_term()
        while self.peek() and self.peek()[1] in ("+", "-"):
            op = self.consume()[1]
            right = self.parse_term()
            left = (op, left, right)
        return left
    
    def parse_term(self) -> Any:
        """term := factor (("*"|"/") factor)*"""
        left = self.parse_factor()
        while self.peek() and self.peek()[1] in ("*", "/"):
            op = self.consume()[1]
            right = self.parse_factor()
            left = (op, left, right)
        return left
    
    def parse_factor(self) -> Any:
        """factor := literal | path | "(" expression ")" """
        token = self.peek()
        
        if token is None:
            raise ExpressionError("Unexpected end of expression")
        
        if token[0] == "NUMBER":
            self.consume()
            value = token[1]
            return ("literal", float(value) if "." in value else int(value))
        
        if token[0] == "STRING":
            self.consume()
            return ("literal", token[1][1:-1])
        
        if token[0] == "BOOL":
            self.consume()
            return ("literal", token[1] == "true")
        
        if token[0] == "NULL":
            self.consume()
            return ("literal", None)
        
        if token[0] == "LPAREN":
            self.consume()
            expr = self.parse_or()
            self.expect("RPAREN")
            return expr
        
        if token[0] == "IDENT":
            return self.parse_path()
        
        raise ExpressionError(f"Unexpected token: {token}")
    
    def parse_path(self) -> Any:
        """path := IDENT ("." IDENT)*"""
        parts = [self.expect("IDENT")[1]]
        while self.peek() and self.peek()[0] == "DOT":
            self.consume()
            parts.append(self.expect("IDENT")[1])
        return ("path", parts)


def resolve_path(path_parts: List[str], context: Dict) -> Any:
    """
    Resolve dotted path like ['event', 'detail', 'missing'] from context.
    
    Returns None if path doesn't exist (no KeyError).
    """
    current = context
    for part in path_parts:
        if isinstance(current, dict):
            current = current.get(part)
        else:
            return None
        if current is None:
            return None
    return current


def evaluate_ast(ast: Any, context: Dict) -> Any:
    """Evaluate parsed AST against context."""
    if not isinstance(ast, tuple):
        return ast
    
    op = ast[0]
    
    if op == "literal":
        return ast[1]
    
    if op == "path":
        return resolve_path(ast[1], context)
    
    if op == "not":
        return not evaluate_ast(ast[1], context)
    
    if op == "and":
        return evaluate_ast(ast[1], context) and evaluate_ast(ast[2], context)
    
    if op == "or":
        return evaluate_ast(ast[1], context) or evaluate_ast(ast[2], context)
    
    left = evaluate_ast(ast[1], context)
    right = evaluate_ast(ast[2], context)
    
    if op == "==":
        return left == right
    if op == "!=":
        return left != right
    if op == "<":
        return left < right if left is not None and right is not None else False
    if op == ">":
        return left > right if left is not None and right is not None else False
    if op == "<=":
        return left <= right if left is not None and right is not None else False
    if op == ">=":
        return left >= right if left is not None and right is not None else False
    if op == "in":
        if right is None:
            return False
        return left in right
    if op == "not_in":
        if right is None:
            return True
        return left not in right
    if op == "+":
        return (left or 0) + (right or 0)
    if op == "-":
        return (left or 0) - (right or 0)
    if op == "*":
        return (left or 0) * (right or 0)
    if op == "/":
        if right == 0:
            return 0
        return (left or 0) / (right or 1)
    
    raise ExpressionError(f"Unknown operator: {op}")


def evaluate_expression(expr: str, context: Dict) -> bool:
    """
    Safely evaluate expression against context.
    
    Args:
        expr: Expression string like 'event.code == "permission_denied"'
        context: Dict with values to evaluate against
    
    Returns:
        Boolean result of expression evaluation
    
    Raises:
        ExpressionError: If expression is invalid
    """
    tokens = tokenize(expr)
    parser = Parser(tokens)
    ast = parser.parse()
    result = evaluate_ast(ast, context)
    return bool(result)


def substitute_templates(obj: Any, context: Dict) -> Any:
    """
    Replace ${path.to.value} with values from context.
    
    Works recursively on dicts, lists, and strings.
    Missing paths are left as-is.
    
    Args:
        obj: Object to substitute templates in
        context: Dict with values for substitution
    
    Returns:
        Object with templates replaced
    """
    if isinstance(obj, str):
        def replace_template(match):
            path = match.group(1)
            parts = path.split(".")
            value = resolve_path(parts, context)
            if value is None:
                return match.group(0)
            return str(value)
        
        return re.sub(r"\$\{([^}]+)\}", replace_template, obj)
    
    if isinstance(obj, dict):
        return {k: substitute_templates(v, context) for k, v in obj.items()}
    
    if isinstance(obj, list):
        return [substitute_templates(item, context) for item in obj]
    
    return obj
