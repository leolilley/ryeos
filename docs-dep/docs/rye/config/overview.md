# RYE Configuration System

## Purpose

RYE's configuration system manages user preferences, tool settings, and optional feature configuration. Configuration is **user-driven** and stored in `.ai/config/` directories.

---

## Configuration Locations

### Hierarchical Configuration

RYE uses a two-tier configuration system:

```
~/.ai/config/          # User-level (global defaults)
└── rag.yaml           # User's default RAG settings

{project}/.ai/config/  # Project-level (overrides user)
└── rag.yaml           # Project-specific RAG settings
```

**Precedence:** Project config overrides user config

---

## Configuration Files

### RAG Configuration (Optional)

**File:** `.ai/config/rag.yaml`

**Purpose:** Configure semantic search and document embedding

```yaml
# Embedding service
embedding_url: "https://api.openai.com/v1/embeddings"
embedding_api_key: "${OPENAI_API_KEY}"
embedding_model: "text-embedding-3-small"

# Storage
storage_path: ".ai/rag_store"
storage_backend: "local"  # local, qdrant, pinecone

# Auto-indexing (optional)
auto_index:
  enabled: true
  collections:
    knowledge: true   # Auto-index knowledge entries
    tools: false      # Don't auto-index tools
    pdfs: false       # Don't auto-index PDFs

# Search settings
search:
  default_limit: 10
  min_score: 0.0
```

**See:** [categories/rag](../categories/rag.md) for complete RAG documentation

---

## Environment Variables

RYE supports environment variable expansion in configuration:

### Syntax

```yaml
# Shell-style variable expansion
api_key: "${OPENAI_API_KEY}"

# With default value
api_key: "${OPENAI_API_KEY:-sk-default}"

# Nested in paths
storage_path: "${HOME}/.ai/rag_store"
```

### Common Variables

| Variable | Description | Example |
|----------|-------------|---------|
| `OPENAI_API_KEY` | OpenAI API key | `sk-...` |
| `EMBEDDING_URL` | Embedding API endpoint | `https://api.openai.com/v1/embeddings` |
| `EMBEDDING_MODEL` | Model to use | `text-embedding-3-small` |
| `RAG_STORAGE_PATH` | Where to store embeddings | `.ai/rag_store` |
| `HOME` | User home directory | `/home/user` |

---

## Configuration Loading

### Automatic Loading

RYE automatically loads configuration when executing tools:

```python
# User calls RAG tool
await execute("rag_search", {
    "query": "API patterns",
    "collection": "knowledge"
})

# RYE automatically:
# 1. Loads ~/.ai/config/rag.yaml (if exists)
# 2. Loads .ai/config/rag.yaml (if exists)
# 3. Merges (project overrides user)
# 4. Expands environment variables
# 5. Passes to tool execution
```

### Manual Loading

Orchestrators can load configuration explicitly:

```python
from rye.config import load_config

# Load RAG config
rag_config = load_config(".ai/config/rag.yaml")

if rag_config:
    # RAG is configured
    embedding_url = rag_config.get("embedding_url")
```

---

## Configuration Validation

### Schema Validation

Configuration files are validated against schemas:

```yaml
# Invalid config
embedding_url: 123  # Should be string

# Error: "embedding_url must be string, got integer"
```

### Required Fields

Some tools require specific configuration:

```yaml
# RAG tools require embedding_url
embedding_url: "https://api.openai.com/v1/embeddings"  # Required
embedding_api_key: "${OPENAI_API_KEY}"                 # Required
```

**Error if missing:**
```
RAG tool 'rag_search' requires configuration:
  - embedding_url (missing)
  - embedding_api_key (missing)

Create .ai/config/rag.yaml with required fields.
```

---

## Best Practices

### 1. Use User-Level for Defaults

```yaml
# ~/.ai/config/rag.yaml
embedding_url: "https://api.openai.com/v1/embeddings"
embedding_api_key: "${OPENAI_API_KEY}"
embedding_model: "text-embedding-3-small"
storage_backend: "local"
```

**Benefit:** All projects inherit these defaults

---

### 2. Use Project-Level for Overrides

```yaml
# .ai/config/rag.yaml (specific project)
embedding_model: "text-embedding-3-large"  # Use larger model
storage_backend: "qdrant"                  # Use Qdrant for this project
qdrant_url: "http://localhost:6333"
```

**Benefit:** Project-specific settings without affecting other projects

---

### 3. Use Environment Variables for Secrets

```yaml
# Good - secrets in environment
embedding_api_key: "${OPENAI_API_KEY}"

# Bad - secrets in config file
embedding_api_key: "sk-abc123..."  # Don't commit this!
```

**Benefit:** Secrets not committed to version control

---

### 4. Start Minimal

```yaml
# Minimal RAG config
embedding_url: "https://api.openai.com/v1/embeddings"
embedding_api_key: "${OPENAI_API_KEY}"
```

**Benefit:** Defaults work for most cases

---

## Configuration Examples

### Development Setup

```yaml
# ~/.ai/config/rag.yaml (development)
embedding_url: "https://api.openai.com/v1/embeddings"
embedding_api_key: "${OPENAI_API_KEY}"
embedding_model: "text-embedding-3-small"  # Cheaper model
storage_backend: "local"
storage_path: "~/.ai/rag_store"

auto_index:
  enabled: false  # Manual indexing during development
```

---

### Production Setup

```yaml
# .ai/config/rag.yaml (production project)
embedding_url: "https://api.openai.com/v1/embeddings"
embedding_api_key: "${OPENAI_API_KEY}"
embedding_model: "text-embedding-3-large"  # Better quality
storage_backend: "qdrant"
qdrant_url: "${QDRANT_URL}"
qdrant_api_key: "${QDRANT_API_KEY}"

auto_index:
  enabled: true
  collections:
    knowledge: true
```

---

### Offline Setup (No API)

```yaml
# .ai/config/rag.yaml (offline)
embedding_url: "local"
embedding_model: "all-MiniLM-L6-v2"  # Local model
storage_backend: "local"
storage_path: ".ai/rag_store"

auto_index:
  enabled: true
  collections:
    knowledge: true
```

---

## Troubleshooting

### Configuration Not Found

**Problem:** RAG tools fail with "configuration not found"

**Solution:**
```bash
# Create config file
mkdir -p .ai/config
cat > .ai/config/rag.yaml << 'EOF'
embedding_url: "https://api.openai.com/v1/embeddings"
embedding_api_key: "${OPENAI_API_KEY}"
EOF
```

---

### Environment Variables Not Expanded

**Problem:** `${OPENAI_API_KEY}` appears literally in config

**Solution:**
```bash
# Ensure environment variable is set
export OPENAI_API_KEY="sk-..."

# Verify
echo $OPENAI_API_KEY
```

---

### Project Config Not Overriding User Config

**Problem:** Project config ignored

**Solution:**
```bash
# Ensure project config exists
ls .ai/config/rag.yaml

# Check file is valid YAML
cat .ai/config/rag.yaml
```

---

## Summary

RYE configuration provides:

1. ✅ **Hierarchical** - User and project levels
2. ✅ **Optional** - Only configure what you need
3. ✅ **Environment variables** - Secure secret management
4. ✅ **Validation** - Schema-based validation
5. ✅ **Defaults** - Sensible defaults for most cases

---

## Related Documentation

- **RAG Configuration:** `[categories/rag](../categories/rag.md)` - RAG-specific configuration
- **RYE Package:** `[package/structure](../package/structure.md)` - Package organization
- **Tool Categories:** `[categories/overview](../categories/overview.md)` - All tool categories
