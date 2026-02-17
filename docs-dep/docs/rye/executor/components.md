# RYE Executor Components

These components live in RYE (not Lilux) because they require content intelligence and orchestration logic.

## PrimitiveExecutor

**Location:** `rye/executor/primitive_executor.py`

**Purpose:** Unified execution orchestration that routes tools to Lilux primitives.

**Responsibilities:**
- Receive tool_id from MCP execute tool
- Load tool file on demand from `.ai/tools/`
- Parse metadata (`__tool_type__`, `__executor_id__`, etc.)
- Resolve executor chain (tool → runtime → primitive)
- Resolve environment via ENV_CONFIG
- Call Lilux primitives for actual execution
- Return unified ExecutionResult

**Why in RYE (not Lilux):**
- Requires metadata understanding
- Requires chain resolution logic
- Requires ENV_CONFIG resolution
- Uses SchemaExtractor for parsing

## ChainValidator

**Location:** `rye/executor/chain_validator.py`

**Purpose:** Validate tool executor chains for integrity and compatibility.

**Responsibilities:**
- Validate parent→child schema compatibility
- Check backward compatibility constraints
- Detect circular dependencies
- Provide detailed validation errors

**Why in RYE (not Lilux):**
- Requires schema understanding
- Requires JSON Schema validation
- Requires version compatibility checking
- Content-aware (not just execution)

## IntegrityVerifier

**Location:** `rye/executor/integrity_verifier.py`

**Purpose:** Verify content integrity with caching for performance.

**Responsibilities:**
- Verify signatures on content
- Cache verification results
- Invalidate cache on file changes
- Coordinate with ChainValidator

**Why in RYE (not Lilux):**
- Uses caching (not a dumb primitive)
- Coordinates with other components
- Part of orchestration logic

## Contrast with Lilux

| Component | RYE (intelligent) | Lilux (dumb) |
|-----------|------------------|--------------|
| PrimitiveExecutor | Chain resolution, ENV_CONFIG | - |
| ChainValidator | Schema validation | - |
| IntegrityVerifier | Cached verification | IntegrityHelpers (pure hash functions) |
| SubprocessPrimitive | - | Just runs commands |
| HttpClientPrimitive | - | Just makes requests |

## Related Documentation

- [overview](overview.md) - Executor architecture
- [routing](routing.md) - Routing examples
- [../principles.md](../principles.md) - On-demand loading principles
- [../mcp-tools/overview](../mcp-tools/overview.md) - MCP tools and how they work
