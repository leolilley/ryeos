# RAG (Retrieval-Augmented Generation) Tools

**Category:** `rye/rag`  
**Purpose:** Optional semantic search and document embedding capabilities

---

## Overview

RAG tools provide semantic search and document embedding capabilities as **data-driven tools**. These tools are completely optional and independent from RYE's core functionality.

**Key Principle:** RAG is opt-in. RYE works perfectly without RAG using keyword search. Users enable RAG by:
1. Configuring RAG settings in `.ai/config/rag.yaml`
2. Installing RAG tools (bundled with RYE in `.ai/tools/rye/rag/`)
3. Optionally enabling auto-indexing hooks

---

## Architecture

### RAG as Data-Driven Tools

RAG is implemented as data-driven tools, NOT hardcoded into RYE:

```
.ai/tools/rye/rag/
├── index.yml           # Index documents into vector store
├── search.yml          # Semantic search in vector store
├── embed.yml           # Embed single document
└── delete.yml          # Remove from vector store
```

**Benefits:**
- ✅ **Optional** - RYE works without RAG
- ✅ **Swappable** - Users can replace with their own implementations
- ✅ **Extensible** - Easy to add new RAG backends
- ✅ **Data-driven** - Follows pure data-driven philosophy

---

## RAG Tools

### 1. `rag_index` - Index Documents

**Purpose:** Index documents into vector store for semantic search.

**Tool Definition:** `.ai/tools/rye/rag/index.yml`

```yaml
name: rag_index
version: 1.0.0
tool_type: rag
executor_id: http_client
category: rag

description: Index documents into vector store for semantic search

config_schema:
  type: object
  properties:
    documents:
      type: array
      description: Array of documents to index
      items:
        type: object
        properties:
          id:
            type: string
            description: Unique document ID
          content:
            type: string
            description: Document content to embed
          metadata:
            type: object
            description: Additional metadata
        required: [id, content]
    collection:
      type: string
      description: Collection name (e.g., 'knowledge', 'pdfs', 'code')
  required: [documents, collection]

config:
  url: "${EMBEDDING_URL}/embed"
  method: POST
  headers:
    Authorization: "Bearer ${EMBEDDING_API_KEY}"
    Content-Type: "application/json"
  body:
    model: "${EMBEDDING_MODEL:-text-embedding-3-small}"
    input: "${documents[*].content}"
  storage_path: "${RAG_STORAGE_PATH:-.ai/rag_store}"
  timeout: 30000
```

**Usage:**

```python
# Index knowledge entries
await execute("rag_index", {
    "documents": [
        {
            "id": "k1",
            "content": "REST APIs should use HTTP verbs...",
            "metadata": {"category": "api", "type": "pattern"}
        },
        {
            "id": "k2",
            "content": "Python virtual environments...",
            "metadata": {"category": "python", "type": "practice"}
        }
    ],
    "collection": "knowledge"
})
```

---

### 2. `rag_search` - Semantic Search

**Purpose:** Search documents by semantic similarity.

**Tool Definition:** `.ai/tools/rye/rag/search.yml`

```yaml
name: rag_search
version: 1.0.0
tool_type: rag
executor_id: http_client
category: rag

description: Semantic search in vector store

config_schema:
  type: object
  properties:
    query:
      type: string
      description: Search query
    collection:
      type: string
      description: Collection to search in
    limit:
      type: integer
      description: Maximum results to return
      default: 10
    min_score:
      type: number
      description: Minimum similarity score (0.0 to 1.0)
      default: 0.0
  required: [query, collection]

config:
  url: "${EMBEDDING_URL}/embed"
  method: POST
  headers:
    Authorization: "Bearer ${EMBEDDING_API_KEY}"
    Content-Type: "application/json"
  body:
    model: "${EMBEDDING_MODEL:-text-embedding-3-small}"
    input: "${query}"
  storage_path: "${RAG_STORAGE_PATH:-.ai/rag_store}"
  timeout: 10000
```

**Usage:**

```python
# Search for relevant knowledge
results = await execute("rag_search", {
    "query": "How do I structure API endpoints?",
    "collection": "knowledge",
    "limit": 5
})

# Results: [
#   {"id": "k1", "score": 0.92, "content": "REST APIs...", "metadata": {...}},
#   {"id": "k3", "score": 0.85, "content": "API design...", "metadata": {...}}
# ]
```

---

### 3. `rag_embed` - Embed Single Document

**Purpose:** Get embedding vector for a single document.

**Tool Definition:** `.ai/tools/rye/rag/embed.yml`

```yaml
name: rag_embed
version: 1.0.0
tool_type: rag
executor_id: http_client
category: rag

description: Get embedding vector for a document

config_schema:
  type: object
  properties:
    content:
      type: string
      description: Content to embed
  required: [content]

config:
  url: "${EMBEDDING_URL}/embed"
  method: POST
  headers:
    Authorization: "Bearer ${EMBEDDING_API_KEY}"
    Content-Type: "application/json"
  body:
    model: "${EMBEDDING_MODEL:-text-embedding-3-small}"
    input: "${content}"
  timeout: 10000
```

**Usage:**

```python
# Get embedding for content
embedding = await execute("rag_embed", {
    "content": "Python is a programming language"
})

# Result: {"embedding": [0.1, 0.2, 0.3, ...], "dimensions": 1536}
```

---

### 4. `rag_delete` - Remove from Vector Store

**Purpose:** Delete documents from vector store.

**Tool Definition:** `.ai/tools/rye/rag/delete.yml`

```yaml
name: rag_delete
version: 1.0.0
tool_type: rag
executor_id: http_client
category: rag

description: Delete documents from vector store

config_schema:
  type: object
  properties:
    ids:
      type: array
      description: Document IDs to delete
      items:
        type: string
    collection:
      type: string
      description: Collection name
  required: [ids, collection]

config:
  storage_path: "${RAG_STORAGE_PATH:-.ai/rag_store}"
```

**Usage:**

```python
# Delete knowledge entries
await execute("rag_delete", {
    "ids": ["k1", "k2"],
    "collection": "knowledge"
})
```

---

## Configuration

### User Configuration

Users configure RAG in their project or user space:

**File:** `.ai/config/rag.yaml` (project) or `~/.ai/config/rag.yaml` (user)

```yaml
# Embedding service configuration
embedding_url: "https://api.openai.com/v1/embeddings"
embedding_api_key: "${OPENAI_API_KEY}"
embedding_model: "text-embedding-3-small"

# Storage configuration
storage_path: ".ai/rag_store"  # Where to store embeddings
storage_backend: "local"       # local, qdrant, pinecone, etc.

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

### Environment Variables

RAG tools use these environment variables:

| Variable | Description | Example |
|----------|-------------|---------|
| `EMBEDDING_URL` | Embedding API endpoint | `https://api.openai.com/v1/embeddings` |
| `EMBEDDING_API_KEY` | API key for embedding service | `sk-...` |
| `EMBEDDING_MODEL` | Model to use | `text-embedding-3-small` |
| `RAG_STORAGE_PATH` | Where to store embeddings | `.ai/rag_store` |

---

## RYE Integration (Optional Hooks)

RYE can optionally integrate with RAG tools through hooks. This is **completely optional** and controlled by user configuration.

### Knowledge Handler Hook

When users enable auto-indexing, RYE's knowledge handler calls RAG tools:

```python
# In RYE's knowledge handler
async def create_knowledge(entry):
    # 1. Always save knowledge (RAG or not)
    await save_knowledge(entry)
    
    # 2. Check if user has RAG configured and enabled
    rag_config = load_config(".ai/config/rag.yaml")
    if not rag_config or not rag_config.get("auto_index", {}).get("enabled"):
        return  # No auto-indexing
    
    # 3. Check if knowledge auto-indexing is enabled
    if not rag_config.get("auto_index", {}).get("collections", {}).get("knowledge"):
        return  # Knowledge auto-indexing disabled
    
    # 4. Check if rag_index tool exists
    if not tool_exists("rag_index"):
        logger.debug("rag_index tool not found, skipping auto-indexing")
        return
    
    # 5. Call RAG tool to index
    try:
        await execute("rag_index", {
            "documents": [{
                "id": entry["id"],
                "content": entry["content"],
                "metadata": {
                    "title": entry.get("title"),
                    "category": entry.get("category"),
                    "entry_type": entry.get("entry_type"),
                    "created_at": entry.get("created_at")
                }
            }],
            "collection": "knowledge"
        })
        logger.info(f"Auto-indexed knowledge entry: {entry['id']}")
    except Exception as e:
        # RAG failed, but knowledge is still saved
        logger.warning(f"RAG auto-indexing failed: {e}")
```

**Key Points:**
- ✅ Knowledge is **always saved** regardless of RAG
- ✅ RAG indexing is **optional** and **non-blocking**
- ✅ Failures in RAG don't affect knowledge creation
- ✅ User controls via configuration

---

## Storage Backends

RAG tools support multiple storage backends:

### Local Storage (Default)

**Configuration:**
```yaml
storage_backend: "local"
storage_path: ".ai/rag_store"
```

**Structure:**
```
.ai/rag_store/
├── knowledge/
│   ├── embeddings.jsonl    # Vector embeddings
│   └── metadata.json       # Collection metadata
├── pdfs/
│   ├── embeddings.jsonl
│   └── metadata.json
└── config.json             # Storage configuration
```

**Pros:**
- ✅ No external dependencies
- ✅ Works offline
- ✅ Simple setup

**Cons:**
- ❌ Limited scalability
- ❌ No distributed search

---

### Qdrant (Production)

**Configuration:**
```yaml
storage_backend: "qdrant"
qdrant_url: "http://localhost:6333"
qdrant_api_key: "${QDRANT_API_KEY}"
```

**Setup:**
```bash
# Run Qdrant locally
docker run -p 6333:6333 qdrant/qdrant

# Or use Qdrant Cloud
# Set QDRANT_URL and QDRANT_API_KEY
```

**Pros:**
- ✅ Production-ready
- ✅ Scalable
- ✅ Fast similarity search

**Cons:**
- ❌ Requires external service
- ❌ More complex setup

---

### Pinecone (Managed)

**Configuration:**
```yaml
storage_backend: "pinecone"
pinecone_api_key: "${PINECONE_API_KEY}"
pinecone_environment: "us-west1-gcp"
pinecone_index: "rye-rag"
```

**Pros:**
- ✅ Fully managed
- ✅ No infrastructure
- ✅ Automatic scaling

**Cons:**
- ❌ Requires API key
- ❌ Cost per usage

---

## Embedding Models

### OpenAI Embeddings

**Configuration:**
```yaml
embedding_url: "https://api.openai.com/v1/embeddings"
embedding_api_key: "${OPENAI_API_KEY}"
embedding_model: "text-embedding-3-small"
```

**Models:**
| Model | Dimensions | Cost | Use Case |
|-------|------------|------|----------|
| `text-embedding-3-small` | 1536 | Low | General purpose |
| `text-embedding-3-large` | 3072 | Medium | High quality |
| `text-embedding-ada-002` | 1536 | Low | Legacy |

---

### Local Embeddings (Offline)

**Configuration:**
```yaml
embedding_url: "local"
embedding_model: "all-MiniLM-L6-v2"
```

**Models:**
| Model | Dimensions | Speed | Use Case |
|-------|------------|-------|----------|
| `all-MiniLM-L6-v2` | 384 | Fast | General purpose |
| `all-mpnet-base-v2` | 768 | Medium | High quality |
| `paraphrase-multilingual` | 768 | Medium | Multilingual |

**Pros:**
- ✅ No API costs
- ✅ Works offline
- ✅ Privacy (no data sent externally)

**Cons:**
- ❌ Requires local model download
- ❌ Slower than API
- ❌ Lower quality than large models

---

## Usage Patterns

### Pattern 1: Manual Indexing

User explicitly indexes content:

```python
# Index PDFs
await execute("rag_index", {
    "documents": [
        {"id": "pdf1", "content": "...", "metadata": {"filename": "doc.pdf"}},
        {"id": "pdf2", "content": "...", "metadata": {"filename": "report.pdf"}}
    ],
    "collection": "pdfs"
})

# Search PDFs
results = await execute("rag_search", {
    "query": "quarterly revenue",
    "collection": "pdfs"
})
```

---

### Pattern 2: Auto-Indexing (Knowledge)

RYE automatically indexes knowledge when created:

```yaml
# .ai/config/rag.yaml
auto_index:
  enabled: true
  collections:
    knowledge: true
```

```python
# User creates knowledge
await execute("create_knowledge", {
    "title": "API Design Patterns",
    "content": "REST APIs should use HTTP verbs...",
    "category": "api"
})

# RYE automatically calls rag_index in background
# User can immediately search
results = await execute("rag_search", {
    "query": "API design",
    "collection": "knowledge"
})
```

---

### Pattern 3: Hybrid Search (Orchestrator)

Orchestrator combines keyword and semantic search:

```python
# RYE orchestrator
async def smart_search(query, collection):
    # Try semantic search first
    try:
        if tool_exists("rag_search"):
            semantic_results = await execute("rag_search", {
                "query": query,
                "collection": collection,
                "limit": 10
            })
            
            if semantic_results:
                return semantic_results
    except:
        pass  # Fall through to keyword
    
    # Fallback to keyword search
    return await execute("search", {
        "query": query,
        "item_type": collection
    })
```

---

## Extensibility

### Adding New Content Types

Users can extend RAG to handle new content types:

**Example: PDF Indexing**

```yaml
# .ai/tools/my_project/rag/index_pdf.yml
name: index_pdf
version: 1.0.0
tool_type: rag
executor_id: rag_index  # Chains to rag_index

config_schema:
  type: object
  properties:
    pdf_path:
      type: string
      description: Path to PDF file

config:
  # Extract text from PDF
  extract_command: "pdftotext ${pdf_path} -"
  
  # Then call rag_index
  documents:
    - id: "${pdf_path}"
      content: "${extract_output}"
      metadata:
        type: "pdf"
        path: "${pdf_path}"
  collection: "pdfs"
```

**Example: Image Embeddings (CLIP)**

```yaml
# .ai/tools/my_project/rag/embed_image.yml
name: embed_image
version: 1.0.0
tool_type: rag
executor_id: http_client

config_schema:
  type: object
  properties:
    image_path:
      type: string

config:
  url: "${CLIP_API_URL}/embed"
  method: POST
  body:
    image: "${image_path}"
```

---

## Best Practices

### 1. Start Without RAG

```yaml
# Don't configure RAG initially
# Use keyword search (built-in)
```

**When to add RAG:**
- Large knowledge base (> 100 entries)
- Semantic search needed
- Concept-based queries

---

### 2. Configure Storage Path

```yaml
# .ai/config/rag.yaml
storage_path: ".ai/rag_store"  # Project-specific
# OR
storage_path: "~/.ai/rag_store"  # User-wide
```

**Recommendation:** Use project-specific storage for better isolation.

---

### 3. Enable Auto-Indexing Selectively

```yaml
auto_index:
  enabled: true
  collections:
    knowledge: true   # Auto-index knowledge
    tools: false      # Don't auto-index tools (too many)
    pdfs: false       # Manually index PDFs
```

---

### 4. Monitor Embedding Costs

```yaml
# Use smaller model for development
embedding_model: "text-embedding-3-small"

# Use larger model for production
embedding_model: "text-embedding-3-large"
```

---

### 5. Test RAG Tools Independently

```bash
# Test indexing
rye execute rag_index --documents '[{"id":"test","content":"hello"}]' --collection test

# Test search
rye execute rag_search --query "hello" --collection test
```

---

## Limitations

### By Design

1. **Optional** - RAG is not required for RYE to function
2. **User-configured** - Users must configure embedding service
3. **Non-blocking** - RAG failures don't affect core operations
4. **Manual or auto** - User chooses indexing strategy

### Technical

1. **Embedding costs** - API calls cost money
2. **Storage size** - Embeddings take disk space
3. **Indexing time** - Large documents take time to embed
4. **Model limitations** - Embedding quality varies by model

---

## Troubleshooting

### RAG Tools Not Found

**Problem:** `rag_index` tool not found

**Solution:**
```bash
# Check if RAG tools are installed
ls .ai/tools/rye/rag/

# If missing, ensure RYE bundle is complete
rye sync
```

---

### Embedding API Errors

**Problem:** `401 Unauthorized` from embedding API

**Solution:**
```yaml
# Check API key in .ai/config/rag.yaml
embedding_api_key: "${OPENAI_API_KEY}"

# Verify environment variable
echo $OPENAI_API_KEY
```

---

### Storage Path Errors

**Problem:** Cannot write to storage path

**Solution:**
```yaml
# Ensure path is writable
storage_path: ".ai/rag_store"

# Create directory if needed
mkdir -p .ai/rag_store
```

---

## Summary

RAG tools provide:

1. ✅ **Optional semantic search** - Not required for RYE
2. ✅ **Data-driven architecture** - Tools, not hardcoded
3. ✅ **User control** - Manual or auto-indexing
4. ✅ **Extensible** - Easy to add new content types
5. ✅ **Multiple backends** - Local, Qdrant, Pinecone
6. ✅ **Multiple models** - OpenAI, local, custom

---

## Related Documentation

- **RYE MCP Tools:** [rye/mcp-tools/search](../rye/mcp-tools/search.md) - Keyword search (no RAG)
- **RYE Categories:** [rye/categories/overview](../rye/categories/overview.md) - All tool categories
- **RYE Configuration:** [rye/config/overview](../rye/config/overview.md) - Configuration system
