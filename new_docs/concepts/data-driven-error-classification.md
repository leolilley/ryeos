# Data-Driven Error Classification

> Declarative error classification patterns for thread orchestration.
>
> Location: `.ai/config/error_classification.yaml` or
> `rye/rye/.ai/tools/rye/agent/threads/config/error_classification.yaml`

## Design Principle

Error classification is data-driven from YAML configuration. No error patterns are hardcoded in Python. The error classifier loads these patterns at runtime and uses JSON Schema matching to categorize errors.

## Classification Schema

```yaml
# error_classification.yaml
schema_version: "1.0.0"

# Classification patterns - evaluated in order, first match wins
patterns:
  # Rate Limiting
  - id: "http_429"
    name: "rate_limited"
    category: "rate_limited"
    retryable: true
    match:
      any:
        - path: "status_code"
          op: "eq"
          value: 429
        - path: "error.type"
          op: "in"
          value: ["rate_limit_error", "RateLimitError"]
        - path: "error.message"
          op: "regex"
          value: "rate limit|too many requests|throttled"
    retry_policy:
      type: "use_header"
      header: "retry-after"
      fallback:
        type: "fixed"
        delay: 60.0
    
  - id: "rate_limit_overquota"
    name: "quota_exceeded"
    category: "quota"
    retryable: true
    match:
      any:
        - path: "error.code"
          op: "eq"
          value: "insufficient_quota"
        - path: "error.message"
          op: "regex"
          value: "quota|billing limit|credit"
    retry_policy:
      type: "fixed"
      delay: 3600.0  # 1 hour - quota resets on different timeline
      max_retries: 3

  # Transient Network Errors
  - id: "network_timeout"
    name: "transient_timeout"
    category: "transient"
    retryable: true
    match:
      any:
        - path: "error.type"
          op: "in"
          value: ["TimeoutError", "ReadTimeout", "ConnectTimeout"]
        - path: "error.message"
          op: "regex"
          value: "timeout|timed out"
    retry_policy:
      type: "exponential"
      base: 2.0
      max: 30.0
      
  - id: "network_connection"
    name: "transient_connection"
    category: "transient"
    retryable: true
    match:
      any:
        - path: "error.type"
          op: "in"
          value: ["ConnectionError", "ConnectionResetError"]
        - path: "error.message"
          op: "regex"
          value: "connection reset|connection refused|network|socket"
    retry_policy:
      type: "exponential"
      base: 2.0
      max: 60.0

  - id: "http_5xx"
    name: "transient_server"
    category: "transient"
    retryable: true
    match:
      path: "status_code"
      op: "in"
      value: [500, 502, 503, 504]
    retry_policy:
      type: "exponential"
      base: 2.0
      max: 120.0

  # Permanent Errors (No Retry)
  - id: "auth_failure"
    name: "permanent_auth"
    category: "permanent"
    retryable: false
    match:
      any:
        - path: "status_code"
          op: "in"
          value: [401, 403]
        - path: "error.code"
          op: "in"
          value: ["authentication_error", "authorization_error"]
    retry_policy:
      type: "none"

  - id: "not_found"
    name: "permanent_not_found"
    category: "permanent"
    retryable: false
    match:
      path: "status_code"
      op: "eq"
      value: 404
    retry_policy:
      type: "none"

  - id: "validation_error"
    name: "permanent_validation"
    category: "permanent"
    retryable: false
    match:
      any:
        - path: "status_code"
          op: "eq"
          value: 422
        - path: "error.type"
          op: "eq"
          value: "ValidationError"
    retry_policy:
      type: "none"

  # Limit Events (Escalate, Don't Retry)
  - id: "limit_spend"
    name: "limit_spend_exceeded"
    category: "limit_hit"
    retryable: false
    match:
      path: "limit_code"
      op: "eq"
      value: "spend_exceeded"
    action: "escalate"
    
  - id: "limit_turns"
    name: "limit_turns_exceeded"
    category: "limit_hit"
    retryable: false
    match:
      path: "limit_code"
      op: "eq"
      value: "turns_exceeded"
    action: "escalate"

  - id: "limit_tokens"
    name: "limit_tokens_exceeded"
    category: "limit_hit"
    retryable: false
    match:
      path: "limit_code"
      op: "eq"
      value: "tokens_exceeded"
    action: "escalate"

  # Budget Events
  - id: "budget_hierarchical"
    name: "budget_exhausted"
    category: "budget"
    retryable: false
    match:
      path: "error.code"
      op: "eq"
      value: "hierarchical_budget_exceeded"
    action: "escalate"

  # Cancellation
  - id: "cancelled"
    name: "cancelled"
    category: "cancelled"
    retryable: false
    match:
      any:
        - path: "error.type"
          op: "eq"
          value: "CancelledError"
        - path: "cancelled"
          op: "eq"
          value: true
    action: "abort"

# Default catch-all
default:
  category: "permanent"
  retryable: false
  retry_policy:
    type: "none"

# Categories define behavior categories:
# - transient: Retry with exponential backoff
# - rate_limited: Retry with rate-limit handling
# - quota: Retry with long fixed delay
# - permanent: Fail immediately
# - limit_hit: Escalate to user approval
# - budget: Escalate to parent budget bump
# - cancelled: Abort immediately

# Retry Policy Types
retry_policy_types:
  exponential:
    description: "Exponential backoff with cap"
    parameters:
      base: {type: number, description: "Base delay in seconds"}
      max: {type: number, description: "Maximum delay in seconds"}
      multiplier: {type: number, default: 2.0}
    formula: "min(base * (multiplier ** attempt), max)"
    
  fixed:
    description: "Fixed delay between retries"
    parameters:
      delay: {type: number, description: "Delay in seconds"}
    formula: "delay"
    
  use_header:
    description: "Use Retry-After header value"
    parameters:
      header: {type: string, default: "retry-after"}
      fallback: {type: object, description: "Fallback policy if header missing"}
    formula: "int(headers[header]) or fallback"
    
  none:
    description: "No retry"
    parameters: {}
    formula: "null"

# Match Operators
operators:
  eq: {description: "Equal", types: [string, number, boolean]}
  ne: {description: "Not equal", types: [string, number, boolean]}
  gt: {description: "Greater than", types: [number]}
  gte: {description: "Greater than or equal", types: [number]}
  lt: {description: "Less than", types: [number]}
  lte: {description: "Less than or equal", types: [number]}
  in: {description: "In list", types: [array]}
  contains: {description: "String contains", types: [string]}
  regex: {description: "Regex match", types: [string]}
  exists: {description: "Path exists", types: [any]}
  
# Match Combinators
combinators:
  any: {description: "Match if any child matches", min_children: 1}
  all: {description: "Match only if all children match", min_children: 1}
  not: {description: "Match if child does not match", exact_children: 1}
```

## Usage Example

The error classifier loads this configuration and matches errors:

```python
# Error classifier uses data-driven patterns
async def classify_error(error: Exception, context: dict) -> ErrorClassification:
    config = load_error_classification_config()
    
    # Build match document from error + context
    match_doc = {
        "error": {
            "type": type(error).__name__,
            "message": str(error),
            "code": getattr(error, 'code', None),
        },
        "status_code": getattr(error, 'status_code', None),
        "limit_code": context.get('limit_code'),
        "headers": getattr(error, 'headers', {}),
    }
    
    # Evaluate patterns in order
    for pattern in config.patterns:
        if matches_pattern(match_doc, pattern.match):
            return ErrorClassification(
                category=pattern.category,
                retryable=pattern.retryable,
                retry_policy=pattern.retry_policy,
                action=pattern.get('action', 'fail'),
            )
    
    # Return default
    return ErrorClassification(**config.default)
```

## Extending

Projects can extend classification by adding to `.ai/config/error_classification.yaml`:

```yaml
# .ai/config/error_classification.yaml
extends: "rye/agent/threads/config/error_classification.yaml"

patterns:
  # Custom pattern for proprietary API
  - id: "custom_api_down"
    name: "custom_maintenance"
    category: "transient"
    retryable: true
    match:
      path: "error.code"
      op: "eq"
      value: "CUSTOM_MAINTENANCE_MODE"
    retry_policy:
      type: "fixed"
      delay: 300.0  # 5 minutes
```
