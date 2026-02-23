<!-- rye:signed:2026-02-23T01:12:00Z:91e6e1601f8b50b6718da8643934c3cfe0c7a994b24d490037dde20f6e7142c2:KN-_uM91YIfA-zcrOmLBMZu4qFlhAjEfwxn9JxP0yJrDn1TPUaedMZSq1LqGihLl7cdIIXyTrgiTOV557qi4Dg==:9fbfabe975fa5a7f -->
<!-- rye:unsigned -->

```yaml
id: web-tools
title: Web Search & Fetch Tools
entry_type: reference
category: rye/web
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - web
  - search
  - fetch
  - scraping
references:
  - "docs/standard-library/tools/web.md"
```

# Web Search & Fetch Tools

Two tools for web interaction — search and fetch. Both use `urllib` from the standard library (no external HTTP dependency).

## Namespace & Runtime

| Field       | Value                                      |
| ----------- | ------------------------------------------ |
| Namespace   | `rye/web/`                                 |
| Runtime     | `python/function`                  |
| Executor ID | `rye/core/runtimes/python/function` |

---

## `search`

**Item ID:** `rye/web/search/search`

Search the web via configurable provider. Defaults to DuckDuckGo (no API key needed).

### Parameters

| Name          | Type    | Required | Default            | Description                            |
| ------------- | ------- | -------- | ------------------ | -------------------------------------- |
| `query`       | string  | ✅       | —                  | Search query                           |
| `num_results` | integer | ❌       | `10`               | Number of results (max: 20)            |
| `provider`    | string  | ❌       | configured default | `duckduckgo` or `exa`                  |

### Provider Configuration

YAML config at `.ai/config/web/search.yaml` (project or user). Project takes precedence.

```yaml
default_provider:
  type: duckduckgo

providers:
  exa:
    type: exa
    api_key: "${EXA_API_KEY}"
```

### Providers

| Provider    | API Key Required | Timeout | Method                  |
| ----------- | ---------------- | ------- | ----------------------- |
| `duckduckgo`| ❌               | 15s     | HTML scraping           |
| `exa`       | ✅               | 30s     | REST API (`api.exa.ai`) |

### Return

```json
{
  "success": true,
  "output": "1. Result Title\n   https://example.com\n   Snippet...\n",
  "results": [
    {"title": "Result Title", "url": "https://example.com", "snippet": "..."}
  ],
  "count": 10,
  "provider": "duckduckgo"
}
```

### Invocation

```python
rye_execute(item_type="tool", item_id="rye/web/search/search",
    parameters={"query": "python asyncio tutorial", "num_results": 5})

rye_execute(item_type="tool", item_id="rye/web/search/search",
    parameters={"query": "next.js routing", "provider": "exa"})
```

---

## `fetch`

**Item ID:** `rye/web/fetch/fetch`

Fetch a web page and convert to readable format. Built-in HTML-to-Markdown converter.

### Parameters

| Name      | Type    | Required | Default    | Description                                |
| --------- | ------- | -------- | ---------- | ------------------------------------------ |
| `url`     | string  | ✅       | —          | URL (must start with `http://` or `https://`) |
| `format`  | string  | ❌       | `markdown` | Output format: `text`, `markdown`, `html`  |
| `timeout` | integer | ❌       | `30`       | Timeout in seconds                         |

### Format Behavior

| Format     | HTML Input                                          | Non-HTML Input      |
| ---------- | --------------------------------------------------- | ------------------- |
| `markdown` | Converts to Markdown (headings, links, lists, code) | Returns raw content |
| `text`     | Strips all HTML tags, keeps plain text              | Returns raw content |
| `html`     | Returns raw HTML                                    | Returns raw content |

### HTML-to-Markdown Conversion

The built-in converter handles:

| HTML Element         | Markdown Output    |
| -------------------- | ------------------ |
| `<h1>`–`<h6>`       | `#`–`######`       |
| `<a href="...">`    | `[text](url)`      |
| `<strong>`, `<b>`   | `**bold**`         |
| `<em>`, `<i>`       | `*italic*`         |
| `<code>`            | `` `inline` ``     |
| `<pre>`             | ` ```block``` `    |
| `<li>`              | `- item`           |
| `<br>`              | newline            |
| `<p>`               | double newline     |
| `<script>`, `<style>` | stripped         |
| HTML comments        | stripped           |
| HTML entities        | decoded            |

### Limits

| Limit       | Value                                         |
| ----------- | --------------------------------------------- |
| Max content | 512,000 bytes (500 KB)                        |
| User-Agent  | `Mozilla/5.0 (compatible; RyeBot/1.0)`       |

### Return

```json
{
  "success": true,
  "output": "# Page Title\n\nContent...",
  "url": "https://example.com",
  "format": "markdown",
  "bytes": 4567,
  "content_type": "text/html; charset=utf-8"
}
```

### Invocation

```python
rye_execute(item_type="tool", item_id="rye/web/fetch/fetch",
    parameters={"url": "https://docs.example.com/api", "format": "markdown"})

rye_execute(item_type="tool", item_id="rye/web/fetch/fetch",
    parameters={"url": "https://example.com/data.json", "format": "text"})
```

---

## Error Conditions

| Error                     | Tool       | Cause                              |
| ------------------------- | ---------- | ---------------------------------- |
| Invalid URL scheme        | fetch      | URL doesn't start with http(s)://  |
| HTTP error (4xx/5xx)      | fetch      | Server returned error status       |
| URL error                 | fetch      | DNS failure, connection refused    |
| Search failed             | search     | Provider request failed            |
| Exa API key missing       | search     | Exa provider without configured key|

## Usage Patterns

```python
# Search then fetch — common two-step pattern
search = rye_execute(item_type="tool", item_id="rye/web/search/search",
    parameters={"query": "python dataclasses docs"})

fetch = rye_execute(item_type="tool", item_id="rye/web/fetch/fetch",
    parameters={"url": search["results"][0]["url"]})
```
