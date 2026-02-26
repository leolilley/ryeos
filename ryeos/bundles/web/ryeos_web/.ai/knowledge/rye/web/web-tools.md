<!-- rye:signed:2026-02-26T05:02:48Z:b917c7b561373e9cc35d4fa19a828da628f7e092dddd3b7214ad7a049d66fc69:AFGiDuOk6FbSEu8_RDvy6DF6Zp7ng08j3HPrYnwCzncfrqmSRnPKD1_SCmwyJ_wOmYccJ_289ci6UliE78uoCg==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: web-tools
title: Web Tools — Search, Fetch & Browser
entry_type: reference
category: rye/web
version: "1.1.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - web
  - search
  - fetch
  - scraping
  - browser
  - playwright
references:
  - "docs/standard-library/tools/web.md"
```

# Web Tools — Search, Fetch & Browser

Three tools for web interaction — search, fetch, and browser automation.

## Namespace & Runtime

| Field       | Value        |
| ----------- | ------------ |
| Namespace   | `rye/web/`   |

| Tool    | Runtime             | Executor ID                          |
| ------- | ------------------- | ------------------------------------ |
| search  | `python/function`   | `rye/core/runtimes/python/function`  |
| fetch   | `python/function`   | `rye/core/runtimes/python/function`  |
| browser | `javascript`        | `rye/core/runtimes/node/node`        |

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

---

## `browser`

**Item ID:** `rye/web/browser/browser`

Browser automation via `playwright-cli`. Open pages, take screenshots, interact with elements, and manage sessions.

### Parameters

| Name      | Type    | Required | Default | Description                                                                 |
| --------- | ------- | -------- | ------- | --------------------------------------------------------------------------- |
| `command` | string  | ✅       | —       | playwright-cli command (see below)                                          |
| `args`    | array   | ❌       | `[]`    | Positional arguments (URL for open/goto, element ref for click, etc.)       |
| `flags`   | object  | ❌       | `{}`    | Named flags (e.g. `{ "headed": true, "filename": "page.png" }`)            |
| `session` | string  | ❌       | `"rye"` | Named session for browser isolation between directive threads               |
| `timeout` | integer | ❌       | `30`    | Command timeout in seconds                                                  |

### Browser Configuration

Config is resolved project → user → system, from `.ai/config/web/browser.json`. The default config uses Playwright's bundled Chromium:

```json
{
  "browser": {
    "browserName": "chromium",
    "launchOptions": {
      "channel": "chromium",
      "headless": true
    }
  }
}
```

The `channel: "chromium"` is required — without it, playwright-cli defaults to Google Chrome.

### Commands

| Command        | Description                          | Args                        |
| -------------- | ------------------------------------ | --------------------------- |
| `open`         | Open a new browser with URL          | URL                         |
| `goto`         | Navigate current tab to URL          | URL                         |
| `screenshot`   | Capture page screenshot              | —                           |
| `snapshot`     | Capture accessibility snapshot       | —                           |
| `click`        | Click an element                     | element ref                 |
| `fill`         | Fill an input field                  | element ref, value          |
| `type`         | Type into focused element            | text                        |
| `select`       | Select dropdown option               | element ref, value          |
| `hover`        | Hover over element                   | element ref                 |
| `press`        | Press a key                          | key name                    |
| `resize`       | Resize viewport                      | width, height               |
| `eval`         | Evaluate JavaScript                  | expression                  |
| `console`      | Get console logs                     | —                           |
| `network`      | Get network log                      | —                           |
| `tab-list`     | List open tabs                       | —                           |
| `tab-new`      | Open new tab                         | URL (optional)              |
| `tab-select`   | Switch to tab                        | tab index                   |
| `tab-close`    | Close current tab                    | tab index (optional)        |
| `close`        | Close browser session                | —                           |
| `close-all`    | Close all sessions                   | —                           |

### Artifacts

Screenshots are saved to `.ai/cache/tools/rye/web/browser/screenshots/`. Snapshots are saved to `.ai/cache/tools/rye/web/browser/snapshots/`. Auto-generated filenames include timestamps.

### Return

```json
{
  "success": true,
  "output": "...",
  "stdout": "...",
  "stderr": "",
  "exit_code": 0,
  "truncated": false,
  "command": "playwright-cli -s=rye open https://example.com",
  "session": "rye",
  "screenshot_path": ".ai/cache/tools/rye/web/browser/screenshots/screenshot-1740268800.png"
}
```

### Invocation

```python
rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "open", "args": ["https://example.com"]})

rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "screenshot"})

rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "click", "args": ["e15"]})

rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "fill", "args": ["e22", "user@example.com"]})

rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "eval", "args": ["document.title"]})

rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "close"})
```

---

## Error Conditions

| Error                     | Tool       | Cause                              |
| ------------------------- | ---------- | ---------------------------------- |
| Unknown command           | browser    | Invalid playwright-cli command     |
| Timeout                   | browser    | Command exceeds timeout            |
| Ref not found             | browser    | Element ref not in current snapshot |

## Usage Patterns

```python
# Search then fetch — common two-step pattern
search = rye_execute(item_type="tool", item_id="rye/web/search/search",
    parameters={"query": "python dataclasses docs"})

fetch = rye_execute(item_type="tool", item_id="rye/web/fetch/fetch",
    parameters={"url": search["results"][0]["url"]})

# Browser automation — open, interact, screenshot
rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "open", "args": ["https://example.com"]})

rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "screenshot"})

rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "close"})
```
