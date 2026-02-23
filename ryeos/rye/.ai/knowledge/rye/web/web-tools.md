<!-- rye:signed:2026-02-23T00:43:10Z:0c586e865c83328299e586b4e15854ac87d39d9be7cd20d7e3a7069eb5349f00:cQArs0Bbh2f2KfkYZVnCZLRdIZxZvsaPrmeYAJVrbi3PUp4cfVC8k1s1WtyHzNJqLI4PmW1JMN8Ra-RnAHVLAw==:9fbfabe975fa5a7f -->

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

## `websearch`

**Item ID:** `rye/web/websearch`

Search the web via configurable provider. Defaults to DuckDuckGo (no API key needed).

### Parameters

| Name          | Type    | Required | Default            | Description                            |
| ------------- | ------- | -------- | ------------------ | -------------------------------------- |
| `query`       | string  | ✅       | —                  | Search query                           |
| `num_results` | integer | ❌       | `10`               | Number of results (max: 20)            |
| `provider`    | string  | ❌       | configured default | `duckduckgo` or `exa`                  |

### Provider Configuration

YAML config at `.ai/config/web/websearch.yaml` (project or user). Project takes precedence.

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
rye_execute(item_type="tool", item_id="rye/web/websearch",
    parameters={"query": "python asyncio tutorial", "num_results": 5})

rye_execute(item_type="tool", item_id="rye/web/websearch",
    parameters={"query": "next.js routing", "provider": "exa"})
```

---

## `webfetch`

**Item ID:** `rye/web/webfetch`

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
rye_execute(item_type="tool", item_id="rye/web/webfetch",
    parameters={"url": "https://docs.example.com/api", "format": "markdown"})

rye_execute(item_type="tool", item_id="rye/web/webfetch",
    parameters={"url": "https://example.com/data.json", "format": "text"})
```

---

## Error Conditions

| Error                     | Tool       | Cause                              |
| ------------------------- | ---------- | ---------------------------------- |
| Invalid URL scheme        | webfetch   | URL doesn't start with http(s)://  |
| HTTP error (4xx/5xx)      | webfetch   | Server returned error status       |
| URL error                 | webfetch   | DNS failure, connection refused    |
| Search failed             | websearch  | Provider request failed            |
| Exa API key missing       | websearch  | Exa provider without configured key|

## Usage Patterns

```python
# Search then fetch — common two-step pattern
search = rye_execute(item_type="tool", item_id="rye/web/websearch",
    parameters={"query": "python dataclasses docs"})

fetch = rye_execute(item_type="tool", item_id="rye/web/webfetch",
    parameters={"url": search["results"][0]["url"]})
```
