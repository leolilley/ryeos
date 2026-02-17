# Data-Driven Hook Configuration

> Declarative hook conditions and actions for thread policy.
>
> Location: `.ai/config/hooks.yaml` (directive-level) or
> `rye/rye/.ai/tools/rye/agent/threads/config/hook_conditions.yaml` (builtins)

## Design Principle

Hook evaluation is data-driven from YAML configuration. No hook conditions or actions are hardcoded. Hooks use JSON Path expressions with standard operators for matching, eliminating the need for custom expression parsers.

## Hook Configuration Schema

```yaml
# hooks.yaml - defined in directive XML, compiled to this format
schema_version: "1.0.0"

# Built-in hook templates (system defaults)
builtin_hooks:
  # Default limit escalation
  - id: "default_limit_escalation"
    event: "limit"
    priority: 0  # Lower = evaluated later
    condition:
      path: "event.limit_code"
      op: "exists"
    action:
      type: "escalate"
      target: "user_approval"
    
  # Default retry for transient errors
  - id: "default_transient_retry"
    event: "error"
    priority: 0
    condition:
      path: "classification.category"
      op: "eq"
      value: "transient"
    action:
      type: "retry"
      max_attempts: 3

  # Context window pressure compaction
  - id: "default_compaction"
    event: "context_window_pressure"
    priority: 0
    condition:
      path: "event.pressure_ratio"
      op: "gt"
      value: 0.8
    action:
      type: "call_directive"
      directive: "rye/agent/threads/default_compaction"
      parameters:
        pressure_ratio: "${event.pressure_ratio}"
        tokens_used: "${event.tokens_used}"
        max_tokens: "${event.max_tokens}"

# Condition Operators Reference
operators:
  eq: {description: "Equal", types: [string, number, boolean]}
  ne: {description: "Not equal", types: [string, number, boolean]}
  gt: {description: "Greater than", types: [number]}
  gte: {description: "Greater than or equal", types: [number]}
  lt: {description: "Less than", types: [number]}
  lte: {description: "Less than or equal", types: [number]}
  in: {description: "In list", types: [array]}
  contains: {description: "String contains substring", types: [string]}
  starts_with: {description: "String starts with", types: [string]}
  ends_with: {description: "String ends with", types: [string]}
  regex: {description: "Matches regex pattern", types: [string]}
  exists: {description: "Path exists in context", types: [any]}
  matches: {description: "Matches sub-schema", types: [object]}

# Condition Combinators
combinators:
  and:
    description: "All conditions must match"
    syntax:
      all:
        - {condition}
        - {condition}
        
  or:
    description: "Any condition can match"
    syntax:
      any:
        - {condition}
        - {condition}
        
  not:
    description: "Condition must NOT match"
    syntax:
      not: {condition}

# Action Types
action_types:
  retry:
    description: "Retry the operation"
    parameters:
      max_attempts: {type: integer, default: 3}
      backoff_policy: 
        type: string
        enum: [exponential, fixed, inherit]
        default: inherit
      delay: {type: number, description: "For fixed backoff"}
    
  fail:
    description: "Fail the thread"
    parameters:
      error_message: {type: string, optional: true}
      
  abort:
    description: "Abort immediately (no cleanup)"
    parameters: {}
    
  continue:
    description: "Continue to next/default behavior"
    parameters: {}
    
  escalate:
    description: "Escalate to user/parent for approval"
    parameters:
      target:
        type: string
        enum: [user_approval, parent_notification, both]
      timeout_seconds: {type: integer, default: 3600}
      
  call_directive:
    description: "Execute another directive"
    parameters:
      directive: {type: string, required: true}
      parameters: {type: object, default: {}}
      inherit_token: {type: boolean, default: true}
      
  suspend:
    description: "Suspend thread awaiting resume"
    parameters:
      suspend_reason: 
        type: string
        enum: [limit, error, budget, approval]
        required: true
      
  emit_event:
    description: "Emit custom transcript event"
    parameters:
      event_type: {type: string, required: true}
      payload: {type: object, default: {}}

# Event Context Schema
# When a hook fires, it receives this context object
event_context_schema:
  type: object
  properties:
    # Always present
    event:
      type: object
      properties:
        type: {type: string}
        timestamp: {type: string, format: date-time}
        thread_id: {type: string}
        directive: {type: string}
    
    # Event-specific fields merged here
    # For 'error' event:
    error:
      type: object
      properties:
        type: {type: string}
        message: {type: string}
        code: {type: string}
    classification:
      type: object
      properties:
        category: {type: string}
        retryable: {type: boolean}
        retry_policy: {type: object}
    
    # For 'limit' event:
    limit_code: {type: string}
    limit_value: {type: number}
    limit_max: {type: number}
    
    # For 'context_window_pressure' event:
    tokens_used: {type: integer}
    max_tokens: {type: integer}
    pressure_ratio: {type: number}
    
    # For 'before_step'/'after_step' events:
    turn_number: {type: integer}
    cost: {type: object}

# Hook Return Values
# Hook directives must return one of these actions
return_schema:
  oneOf:
    - type: object
      properties:
        action:
          type: string
          enum: [RETRY, FAIL, ABORT, CONTINUE]
      required: [action]
      
    - type: object
      properties:
        action:
          type: string
          const: ESCALATE
        escalation:
          type: object
          properties:
            target: {type: string}
            message: {type: string}
      required: [action, escalation]
      
    - type: object
      properties:
        action:
          type: string
          const: SUSPEND
        suspend:
          type: object
          properties:
            reason: {type: string}
            resume_hint: {type: string}
      required: [action, suspend]
      
    - type: object
      properties:
        action:
          type: string
          const: CALL
        call:
          type: object
          properties:
            directive: {type: string}
            parameters: {type: object}
      required: [action, call]
      
    - type: object
      properties:
        compaction:
          type: object
          properties:
            summary: {type: string}
            prune_before_turn: {type: integer}
      required: [compaction]
```

## Example Directive Hooks

```yaml
# User-defined hooks from directive XML
hooks:
  # Custom error handling for rate limits
  - event: "error"
    condition:
      path: "classification.category"
      op: "eq"
      value: "rate_limited"
    action:
      type: "call_directive"
      directive: "my_app/custom_backoff"
      parameters:
        retry_after: "${error.headers['retry-after']}"
    
  # Budget alert hook
  - event: "limit"
    condition:
      all:
        - path: "limit_code"
          op: "eq"
          value: "spend_exceeded"
        - path: "cost.spend"
          op: "gt"
          value: 10.0
    action:
      type: "escalate"
      target: "user_approval"
      timeout_seconds: 7200
    
  # Smart compaction hook
  - event: "context_window_pressure"
    condition:
      path: "pressure_ratio"
      op: "gt"
      value: 0.85
    action:
      type: "call_directive"
      directive: "my_app/smart_summarizer"

  # Error notification hook
  - event: "error"
    condition:
      path: "classification.category"
      op: "eq"
      value: "permanent"
    action:
      type: "emit_event"
      event_type: "my_app/error_notification"
      payload:
        error_type: "${error.type}"
        message: "${error.message}"
```

## Condition Evaluation

```python
# Hook evaluation uses data-driven conditions
async def evaluate_hooks(event: str, context: dict, hooks: list) -> Optional[dict]:
    """Evaluate hooks in priority order, return first matching action."""
    
    # Sort by priority (higher first)
    sorted_hooks = sorted(hooks, key=lambda h: h.get('priority', 0), reverse=True)
    
    for hook in sorted_hooks:
        if hook['event'] != event:
            continue
            
        # Evaluate condition against context
        if matches_condition(context, hook['condition']):
            return hook['action']
    
    return None

def matches_condition(context: dict, condition: dict) -> bool:
    """Match condition against context using JSON Path operators."""
    
    # Handle combinators
    if 'all' in condition:
        return all(matches_condition(context, c) for c in condition['all'])
    if 'any' in condition:
        return any(matches_condition(context, c) for c in condition['any'])
    if 'not' in condition:
        return not matches_condition(context, condition['not'])
    
    # Simple condition
    path = condition['path']
    op = condition['op']
    expected = condition.get('value')
    
    actual = resolve_path(context, path)
    
    return apply_operator(actual, op, expected)

def apply_operator(actual, op: str, expected) -> bool:
    """Apply operator from config."""
    operators = {
        'eq': lambda a, e: a == e,
        'ne': lambda a, e: a != e,
        'gt': lambda a, e: a > e,
        'gte': lambda a, e: a >= e,
        'lt': lambda a, e: a < e,
        'lte': lambda a, e: a <= e,
        'in': lambda a, e: a in e,
        'contains': lambda a, e: e in str(a),
        'starts_with': lambda a, e: str(a).startswith(e),
        'ends_with': lambda a, e: str(a).endswith(e),
        'regex': lambda a, e: re.search(e, str(a)) is not None,
        'exists': lambda a, e: a is not None,
    }
    
    return operators[op](actual, expected)
```

## Integration with Directive XML

Hooks defined in directive metadata XML are compiled to this YAML format:

```xml
<!-- Directive XML -->
<directive name="my_directive">
  <hooks>
    <hook event="error" when="classification.category == 'rate_limited'">
      <directive>my_app/custom_backoff</directive>
    </hook>
  </hooks>
</directive>
```

Compiled to:

```yaml
hooks:
  - event: "error"
    condition:
      path: "classification.category"
      op: "eq"
      value: "rate_limited"
    action:
      type: "call_directive"
      directive: "my_app/custom_backoff"
```

The XML parser is simple - it only handles basic equality. Complex conditions use YAML directly.
