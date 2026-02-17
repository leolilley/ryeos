# RYE Cache System

## Overview

RYE implements caching layers for performance optimization in its loading and execution systems. These caches integrate with Lilux's hash-based file change detection for automatic cache invalidation.

**Key Principles:**
- **On-demand loading** — tools are cached when first accessed via `load()` or `execute()`, not at startup
- **Automatic invalidation** — hash-based file change detection, no manual cache clearing needed
- **Lilux provides pure functions, RYE adds caching** — clean separation of concerns

---

## Cache Architecture

| Cache | Lives In | Purpose | Invalidation |
|-------|----------|---------|--------------|
| **ChainResolver cache** | RYE | Caches resolved execution chains | Hash-based file change detection |
| **IntegrityVerifier cache** | RYE | Caches verified hashes | File modification |
| **SchemaExtractor cache** | RYE | Caches extractor configs | Hash-based |
| **Tool metadata cache** | RYE | Caches tool metadata (on-demand) | Hash-based file change detection |
| **Lockfile state** | Lilux | Basic lock state | Manual update |

### Lilux vs RYE Responsibility

**Lilux `integrity.py`** — Pure functions (no caching):
```python
compute_hash(content: bytes) -> str
sign(content: bytes, private_key: bytes) -> str
verify_signature(content: bytes, signature: str, public_key: bytes) -> bool
```

**RYE `IntegrityVerifier`** — Caching wrapper around Lilux functions:
```python
class IntegrityVerifier:
    _hash_cache: Dict[str, str]  # path -> hash
    
    def verify(self, path: str) -> VerificationResult:
        # Uses Lilux compute_hash() internally
        # Caches results for performance
```

---

## ChainResolver Cache

**Location:** `rye/executor/chain_resolver.py`

**What It Caches:**

```python
_chain_cache: Dict[str, List[Dict]]
    # Key: tool_id (string)
    # Value: Execution chain from leaf to primitive
    # Example: {
    #   "my_tool": [
    #     {"tool_id": "my_tool", "executor_id": "python_runtime", ...},
    #     {"tool_id": "python_runtime", "executor_id": "subprocess", ...},
    #     {"tool_id": "subprocess", "executor_id": None, ...}
    #   ]
    # }
```

### Cache Behavior

```python
# First resolve - builds chain from filesystem
chain = await chain_resolver.resolve("my_tool")
# → Walks executor_id chain
# → Extracts metadata from .ai/tools/
# → Builds full chain from tool to primitive
# → Caches in _chain_cache

# Subsequent resolve - uses cache
chain2 = await chain_resolver.resolve("my_tool")
# → Returns cached chain immediately (no filesystem walk)
# → Fast: ~5ms instead of ~50ms
```

### Automatic Invalidation (Integrated with Lilux)

```python
# On every execution, verify chain integrity
from lilux.primitives.integrity import compute_hash

class ChainResolver:
    _chain_cache: Dict[str, List[Dict]]
    _hash_cache: Dict[str, str]  # tool_id -> content_hash

    async def resolve(self, tool_id: str) -> List[Dict]:
        if tool_id in self._chain_cache:
            cached_chain = self._chain_cache[tool_id]
            
            # Check if file changed via hash
            current_hash = compute_hash(read_tool_file(tool_id))
            cached_hash = self._hash_cache.get(tool_id)
            
            if current_hash != cached_hash:
                # File changed - invalidate and reload
                self._chain_cache.pop(tool_id)
                self._hash_cache.pop(tool_id)
                
                chain = await self._resolve_chain(tool_id)
                self._chain_cache[tool_id] = chain
                self._hash_cache[tool_id] = current_hash
                return chain
            
            # File unchanged - use cache
            return cached_chain
        
        # Not in cache - resolve fresh
        chain = await self._resolve_chain(tool_id)
        current_hash = compute_hash(read_tool_file(tool_id))
        self._chain_cache[tool_id] = chain
        self._hash_cache[tool_id] = current_hash
        return chain
```

**Benefits:**

- ✅ Automatic detection - no manual cache clearing
- ✅ Cryptographic reliability - content-based detection
- ✅ Integrated flow - leverages existing hash verification
- ✅ Performance - 10x-100x speedup from caching

### Manual Cache Management

```python
# Clear all caches
chain_resolver.clear_caches()

# Invalidate specific tool
chain_resolver.invalidate_tool("my_tool")

# Re-resolve from filesystem
chain = await chain_resolver.resolve("my_tool", force_refresh=True)
```

---

## On-Demand Tool Cache

**Location:** `rye/loading/tool_loader.py`

**What It Caches:**

```python
_tool_cache: Dict[str, Dict]
    # Key: tool_id (string)
    # Value: Tool metadata loaded on first access
    # Example: {
    #   "my_tool": {
    #     "name": "my_tool",
    #     "tool_type": "python",
    #     "executor_id": "python_runtime",
    #     "category": "python",
    #     "config_schema": {...},
    #     "file_path": "/path/to/.ai/tools/python/my_tool.py",
    #     "content_hash": "sha256:..."
    #   }
    # }
```

### On-Demand Loading (No Startup Scanning)

```python
class ToolLoader:
    _tool_cache: Dict[str, Dict]
    _hash_cache: Dict[str, str]

    async def load(self, tool_id: str) -> Dict:
        """Load tool metadata on first access."""
        
        # Check cache first
        if tool_id in self._tool_cache:
            cached = self._tool_cache[tool_id]
            
            # Verify file hasn't changed
            current_hash = compute_hash(read_tool_file(tool_id))
            if current_hash == self._hash_cache.get(tool_id):
                return cached  # Cache hit
            
            # File changed - invalidate
            self._tool_cache.pop(tool_id)
            self._hash_cache.pop(tool_id)
        
        # Cache miss - load from filesystem
        metadata = await self._load_from_filesystem(tool_id)
        content_hash = compute_hash(read_tool_file(tool_id))
        
        # Cache for future access
        self._tool_cache[tool_id] = metadata
        self._hash_cache[tool_id] = content_hash
        
        return metadata
```

### Cache Population Flow

```
User calls load("my_tool") or execute("my_tool")
    │
    ├─→ Check _tool_cache for "my_tool"
    │
    ├─→ If cached:
    │   ├─→ Compute current file hash
    │   ├─→ Compare with cached hash
    │   ├─→ If match → return cached metadata
    │   └─→ If mismatch → invalidate and reload
    │
    └─→ If not cached:
        ├─→ Load from .ai/tools/
        ├─→ Parse metadata and schema
        ├─→ Store in _tool_cache with hash
        └─→ Return metadata
```

### Cache Invalidation

```python
# Automatic: hash-based on every access
# Manual: explicit invalidation when needed

# Invalidate specific tool
tool_loader.invalidate("my_tool")

# Clear all cached tools
tool_loader.clear_cache()
```

**Performance:**

- First load: ~20-50ms (read file, parse metadata)
- Cached access: ~1-2ms (dict lookup + hash check)
- **10-50x speedup** after initial load

---

## AuthStore Cache

**Location:** `rye/runtime/auth.py`

**What It Caches:**

```python
_token_cache: Dict[str, Dict]
    # Key: registry service name
    # Value: {
    #   "access_token": "...",
    #   "refresh_token": "...",
    #   "expires_at": "...",
    #   "scopes": [...]
    # }

_session_cache: Dict[str, Dict]
    # Caches device auth sessions
```

### Cache Behavior

```python
# Try cache first
token = await auth_store.get_token("registry")

if token and not token_expired(token):
    # Cache hit - return immediately
    return token

# Cache miss - fetch from keyring or network
token = await fetch_from_keyring_or_network()

# Store in cache
await auth_store.cache_token("registry", token)
```

### Cache Invalidation

```python
# Logout clears registry tokens
await auth_store.clear_registry_tokens()

# Token expiry check
if token_expired(cached_token):
    await auth_store.cache_token("registry", None)
```

---

## Integration with Lilux Hash Functions

### How It Works

RYE's caching layers use Lilux's pure hash functions for file change detection:

```python
from lilux.primitives.integrity import compute_hash

class CachingVerifier:
    """RYE wrapper that adds caching around Lilux hash functions."""
    
    def __init__(self):
        self._hash_cache = {}  # path -> hash
    
    def verify_unchanged(self, path: str) -> bool:
        """Check if file content matches cached hash."""
        content = Path(path).read_bytes()
        current_hash = compute_hash(content)  # Lilux pure function
        
        cached_hash = self._hash_cache.get(path)
        
        if cached_hash is None:
            # First access - cache and return True
            self._hash_cache[path] = current_hash
            return True
        
        if current_hash == cached_hash:
            return True  # Unchanged
        
        # File changed - update cache
        self._hash_cache[path] = current_hash
        return False
```

### VerificationResult

```python
@dataclass
class VerificationResult:
    success: bool
    file_changed: bool = False
    new_hash: Optional[str] = None
    old_hash: Optional[str] = None
    error: Optional[str] = None
    verified_count: int = 0
    cached_count: int = 0
    duration_ms: int = 0
```

---

## Performance Benefits

| Operation             | Uncached | Cached | Improvement     |
| --------------------- | -------- | ------ | --------------- |
| **Chain resolution**  | ~50ms    | ~5ms   | **10x faster**  |
| **Tool loading**      | ~20-50ms | ~1-2ms | **10-50x faster** |
| **Auth token lookup** | ~10ms    | ~1ms   | **10x faster**  |

---

## Cache Management Best Practices

### When to Clear Caches

1. **ChainResolver cache**

   ```python
   # When a tool file is modified (automatic via hash check)
   # Or force refresh
   chain = await chain_resolver.resolve("my_tool", force_refresh=True)

   # Clear all
   chain_resolver.clear_caches()
   ```

2. **Tool metadata cache**

   ```python
   # Automatic: hash-based invalidation on every access
   
   # Manual: force reload
   tool_loader.invalidate("my_tool")
   tool_loader.clear_cache()
   ```

3. **AuthStore cache**

   ```python
   # Logout clears tokens
   await auth_store.clear_registry_tokens()

   # Token expiry invalidates automatically
   ```

### Automatic vs Manual

| Aspect               | Manual Invalidation | Automatic (Hash-Based)             |
| -------------------- | ------------------- | ---------------------------------- |
| **When to clear**    | User must remember  | File changes trigger automatically |
| **Error-prone**      | Easy to forget      | Never forgets                      |
| **Detection method** | mtime or manual     | Hash verification                  |
| **Performance**      | Manual only         | Automatic detection                |
| **User friction**    | High                | None                               |

---

## Related Documentation

- **Lilux Integrity Helpers:** `[[lilux/primitives/integrity]]`
- **RYE Principles:** `[[rye/principles]]` - On-demand loading model
- **RYE MCP Tools:** `[[rye/mcp-tools/overview]]` - 5 MCP tools architecture
- **RYE Executor Components:** `[[rye/executor/components]]`
