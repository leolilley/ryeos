**Source:** `rye/utils/errors.py` - ErrorCode enum and ErrorResponse dataclass

# Error Codes Reference

Complete reference of all RYE error codes, their causes, and resolutions.

---

## Validation Errors

### VALIDATION_FAILED

**Message:** `Validation failed for {item_id}`

**Cause:** Item metadata doesn't match validation schema.

**Common Issues:**
- Missing required fields (name, version, tool_type)
- Wrong field type (string vs semver)
- Invalid format (version "1.0" instead of "1.0.0")
- Non-snake_case naming

**Resolution:**
1. Check error details for specific issues
2. Fix metadata in Python constants or YAML frontmatter
3. Verify required fields match schema

**Example Error:**
```json
{
  "code": "VALIDATION_FAILED",
  "message": "Validation failed for my_tool",
  "details": {
    "issues": [
      "Field 'version' must be semver format (X.Y.Z), got '1.0'",
      "Field 'tool_type' missing required field"
    ]
  }
}
```

### SCHEMA_NOT_FOUND

**Message:** `No validation schema found for {item_type}`

**Cause:** Extractor for item type missing or unreadable.

**Resolution:**
1. Check extractors exist: `.ai/tools/rye/core/extractors/{type}/`
2. Verify extractor file defines `VALIDATION_SCHEMA`
3. Check file permissions and Python syntax
4. Fallback schema will be used automatically

---

## Execution Errors

### TOOL_NOT_FOUND

**Message:** `{item_type} not found: {item_id}`

**Cause:** Tool doesn't exist in any space (project/user/system).

**Resolution:**
1. Verify tool exists: `find .ai/ -name "*{name}*"`
2. Check naming matches metadata
3. Verify `.ai/` directory structure
4. Check file extensions (.py for tools, .md for directives/knowledge)

**Space Precedence:**
- **Project:** `.ai/tools/{category}/{name}.py`
- **User:** `~/.ai/tools/{category}/{name}.py`
- **System:** `{site-packages}/rye/.ai/tools/{category}/{name}.py`

### EXECUTOR_NOT_FOUND

**Message:** `Executor not found: {executor_id}`

**Cause:** Tool's `__executor_id__` is not available.

**Resolution:**
1. Verify executor exists in same space or higher precedence
2. Check `__executor_id__` value in tool metadata
3. Ensure executor chain doesn't skip spaces (project → system invalid)
4. Use `None` for primitives (no delegation)

### CIRCULAR_DEPENDENCY

**Message:** `Circular dependency detected: {item_id}`

**Cause:** Tool's executor chain has circular reference.

**Resolution:**
1. Check executor chain: A → B → C → A
2. Verify `__executor_id__` values don't loop
3. Use `None` to terminate chain at primitive
4. Trace full chain: `tool → runtime → primitive`

**Debugging:**
```python
# Chain should be: tool → runtime → None (primitive)
# Invalid: tool → runtime → tool (cycle)
```

### CHAIN_TOO_DEEP

**Message:** `Chain too deep (max 10): {item_id}. Possible circular dependency or excessive nesting.`

**Cause:** Executor chain exceeds `MAX_CHAIN_DEPTH = 10`.

**Resolution:**
1. Simplify executor delegation chain
2. Reduce nesting levels (max 10)
3. Check for circular dependencies
4. Normal chains are 2-3 levels (tool → runtime → primitive)

---

## Runtime Errors

### EXECUTION_FAILED

**Message:** `Execution failed for {item_id}: {reason}`

**Cause:** Tool execution encountered error (retryable).

**Common Reasons:**
- Timeout
- Resource exhaustion
- Missing dependencies
- Invalid parameters
- Network error

**Resolution:**
1. Check tool logs/output
2. Verify parameters are correct
3. Ensure dependencies installed
4. Retry (error is marked retryable)

### TIMEOUT

**Message:** `Execution timeout for {item_id}`

**Cause:** Tool execution exceeded time limit.

**Resolution:**
1. Increase timeout if appropriate
2. Optimize tool performance
3. Break into smaller tasks
4. Check for infinite loops

### RESOURCE_EXCEEDED

**Message:** `Resource limit exceeded for {item_id}`

**Cause:** Tool exceeded memory/CPU limits.

**Resolution:**
1. Increase resource limits
2. Optimize memory usage in tool
3. Process data in chunks
4. Monitor resource consumption

---

## Authentication Errors

### AUTH_REQUIRED

**Message:** `Authentication required to {action}`

**Cause:** User not authenticated for operation.

**Resolution:**
1. Run: `rye registry login`
2. Provide credentials (GitHub, email, etc.)
3. Verify token stored in keychain
4. Check permissions for action

### AUTH_FAILED

**Message:** `Authentication failed`

**Cause:** Invalid credentials or auth token expired.

**Resolution:**
1. Logout: `rye registry logout`
2. Login again: `rye registry login`
3. Check credential validity
4. Verify network connectivity

### SESSION_EXPIRED

**Message:** `Session expired`

**Cause:** Auth session token no longer valid.

**Resolution:**
1. Login again to refresh session
2. Check token expiration
3. Verify local keychain access

---

## Registry Errors

### REGISTRY_ERROR

**Message:** `Registry error: {details}`

**Cause:** Registry API error (network, server, etc.).

**Common Issues:**
- Network timeout
- Registry server down
- Invalid API response
- Rate limiting

**Resolution:**
1. Check network connectivity
2. Verify registry URL
3. Retry request
4. Check registry status page
5. Wait if rate limited

### ITEM_NOT_FOUND

**Message:** `Item not found in registry: {item_id}`

**Cause:** Item doesn't exist in registry or not accessible.

**Resolution:**
1. Verify item_id format: `namespace/category/name`
2. Check item is published (visibility=public)
3. Verify you have access permissions
4. Try search first: `rye search {name}`

---

## System Errors

### FILE_SYSTEM_ERROR

**Message:** `File system error: {details}`

**Cause:** File operation failed (permission, not found, disk full, etc.).

**Common Issues:**
- Permission denied
- Disk full
- Path doesn't exist
- Invalid file handle

**Resolution:**
1. Check file permissions: `ls -la {path}`
2. Verify disk space: `df -h`
3. Check path exists
4. Fix permissions if needed

### PARSING_ERROR

**Message:** `Parsing error: {details}`

**Cause:** Failed to parse file content (Python, YAML, XML, etc.).

**Common Issues:**
- Invalid Python syntax
- Malformed YAML
- Encoding issues
- Missing required sections

**Resolution:**
1. Validate file format
2. Check Python syntax: `python -m py_compile {file}`
3. Validate YAML/XML
4. Ensure UTF-8 encoding
5. Use regex fallback for malformed files

### CONFIG_ERROR

**Message:** `Configuration error: {details}`

**Cause:** Invalid configuration values.

**Common Issues:**
- Invalid template syntax
- Undefined variables
- Type mismatch
- Missing required config

**Resolution:**
1. Check config values
2. Verify variable substitution
3. Validate types match schema
4. Provide defaults for optional values

### UNKNOWN_ERROR

**Message:** `Unknown error: {details}`

**Cause:** Unexpected error (bug or unhandled case).

**Resolution:**
1. Check application logs
2. Report with full error details
3. Retry operation
4. Contact support with trace

---

## Error Response Format

All errors follow standard format:

```python
{
  "success": False,
  "error": {
    "code": "ERROR_CODE",           # Machine-readable
    "message": "User message",      # Human-readable
    "details": {...},               # Error-specific details
    "retryable": bool,              # Should client retry?
    "suggestion": "Fix suggestion"  # How to resolve
  }
}
```

---

## Quick Reference Table

| Code | Severity | Retryable | Category | Common Cause |
|------|----------|-----------|----------|-------------|
| VALIDATION_FAILED | High | No | Validation | Bad metadata |
| SCHEMA_NOT_FOUND | Medium | No | Validation | Missing extractor |
| TOOL_NOT_FOUND | High | No | Execution | Item missing |
| EXECUTOR_NOT_FOUND | High | No | Execution | Broken chain |
| CIRCULAR_DEPENDENCY | High | No | Execution | Loop in chain |
| CHAIN_TOO_DEEP | High | No | Execution | Nested > 10 |
| EXECUTION_FAILED | Medium | Yes | Runtime | Tool error |
| TIMEOUT | Medium | Yes | Runtime | Slow/hanging |
| RESOURCE_EXCEEDED | Medium | Yes | Runtime | OOM/CPU |
| AUTH_REQUIRED | High | No | Auth | Not logged in |
| AUTH_FAILED | High | No | Auth | Bad credentials |
| SESSION_EXPIRED | Medium | Yes | Auth | Token stale |
| REGISTRY_ERROR | Medium | Yes | Registry | Server issue |
| ITEM_NOT_FOUND | Medium | No | Registry | Not published |
| FILE_SYSTEM_ERROR | High | No | System | FS operation |
| PARSING_ERROR | High | No | System | Bad file format |
| CONFIG_ERROR | High | No | System | Bad config |
| UNKNOWN_ERROR | Critical | No | System | Bug/unexpected |

---

## Best Practices

### Error Handling

1. **Check error code** before examining details
2. **Follow suggestion** for quick resolution
3. **Note retryable flag** - retry if true with backoff
4. **Report unknown errors** with full context

### Error Prevention

1. **Validate early** - catch errors before execution
2. **Use fallbacks** - regex for malformed files, defaults for env vars
3. **Set limits** - max chain depth, cache size, timeouts
4. **Log everything** - include error context in logs

### User Communication

1. **Provide specific error code** (not just message)
2. **Include suggestion** for resolution
3. **Show details** for debugging
4. **Indicate if retryable** - user can retry

---

## Related Documentation

- [Validation](./content-handlers/validation.md) - Schema validation details
- [Executor](./executor/overview.md) - Chain resolution details
- [Error Response](../ARCHITECTURE_SUMMARY.md#error-handling) - Response format
