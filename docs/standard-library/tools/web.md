---
id: tools-web
title: "Web Tools"
description: Search the web and fetch page content with format conversion
category: standard-library/tools
tags: [tools, web, search, fetch, websearch, webfetch]
version: "1.0.0"
---

# Web Tools

**Namespace:** `rye/web/`
**Runtime:** `python_function_runtime`

Two tools for web interaction — search and fetch. Both use `urllib` from the standard library (no external HTTP dependency required).

---

## `websearch`

**Item ID:** `rye/web/websearch`

Search the web using a configurable provider. Defaults to **DuckDuckGo** (no API key required). Optionally supports **Exa** for higher-quality results.

### Parameters

| Name          | Type    | Required | Default            | Description                            |
| ------------- | ------- | -------- | ------------------ | -------------------------------------- |
| `query`       | string  | ✅       | —                  | Search query                           |
| `num_results` | integer | ❌       | `10`               | Number of results (max: 20)            |
| `provider`    | string  | ❌       | configured default | Search provider: `duckduckgo` or `exa` |

### Provider Configuration

Providers are configured via YAML at `.ai/config/websearch.yaml` (project) or `~/.ai/config/websearch.yaml` (user). Project config takes precedence.

```yaml
default_provider:
  type: duckduckgo

providers:
  exa:
    type: exa
    api_key: "${EXA_API_KEY}"
```

### Output

```json
{
  "success": true,
  "output": "1. Result Title\n   https://example.com\n   Snippet text...\n",
  "results": [
    { "title": "Result Title", "url": "https://example.com", "snippet": "..." }
  ],
  "count": 10,
  "provider": "duckduckgo"
}
```

### Example

```python
rye_execute(item_type="tool", item_id="rye/web/websearch",
    parameters={"query": "python asyncio tutorial", "num_results": 5})
```

---

## `webfetch`

**Item ID:** `rye/web/webfetch`

Fetch a web page and convert it to a readable format. Includes a built-in HTML-to-Markdown converter that strips scripts, styles, and comments while preserving headings, links, lists, and code blocks.

### Parameters

| Name      | Type    | Required | Default    | Description                                            |
| --------- | ------- | -------- | ---------- | ------------------------------------------------------ |
| `url`     | string  | ✅       | —          | URL to fetch (must start with `http://` or `https://`) |
| `format`  | string  | ❌       | `markdown` | Output format: `text`, `markdown`, or `html`           |
| `timeout` | integer | ❌       | `30`       | Timeout in seconds                                     |

### Format Behavior

| Format     | HTML Input                                          | Non-HTML Input      |
| ---------- | --------------------------------------------------- | ------------------- |
| `markdown` | Converts to Markdown (headings, links, lists, code) | Returns raw content |
| `text`     | Strips all HTML tags, keeps plain text              | Returns raw content |
| `html`     | Returns raw HTML                                    | Returns raw content |

### Limits

- **Max content:** 512,000 bytes (500 KB)
- **User-Agent:** `Mozilla/5.0 (compatible; RyeBot/1.0)`

### Output

```json
{
  "success": true,
  "output": "# Page Title\n\nContent converted to markdown...",
  "url": "https://example.com",
  "format": "markdown",
  "bytes": 4567,
  "content_type": "text/html; charset=utf-8"
}
```

### Example

```python
rye_execute(item_type="tool", item_id="rye/web/webfetch",
    parameters={"url": "https://docs.example.com/api", "format": "markdown"})
```
