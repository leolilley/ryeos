```yaml
id: tools-web
title: "Web Tools"
description: Search the web and fetch page content with format conversion
category: standard-library/tools
tags: [tools, web, search, fetch, browser]
version: "1.0.0"
```

# Web Tools

**Namespace:** `rye/web/`
**Runtime:** `python/function` (search, fetch) · `node/node` (browser)

Three tools for web interaction — search, fetch, and browser automation.

---

## `search`

**Item ID:** `rye/web/search/search`

Search the web using a configurable provider. Defaults to **DuckDuckGo** (no API key required). Optionally supports **Exa** for higher-quality results.

### Parameters

| Name          | Type    | Required | Default            | Description                            |
| ------------- | ------- | -------- | ------------------ | -------------------------------------- |
| `query`       | string  | ✅       | —                  | Search query                           |
| `num_results` | integer | ❌       | `10`               | Number of results (max: 20)            |
| `provider`    | string  | ❌       | configured default | Search provider: `duckduckgo` or `exa` |

### Provider Configuration

Providers are configured via YAML at `.ai/config/web/websearch.yaml` (project) or `{USER_SPACE}/.ai/config/web/websearch.yaml` (user). Project config takes precedence.

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
rye_execute(item_type="tool", item_id="rye/web/search/search",
    parameters={"query": "python asyncio tutorial", "num_results": 5})
```

---

## `fetch`

**Item ID:** `rye/web/fetch/fetch`

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
rye_execute(item_type="tool", item_id="rye/web/fetch/fetch",
    parameters={"url": "https://docs.example.com/api", "format": "markdown"})
```

---

## `browser`

**Item ID:** `rye/web/browser/browser`

Browser automation powered by [playwright-cli](https://github.com/nicjackson/playwright-cli). Opens pages, takes screenshots, clicks elements, fills forms, and more — all via a TypeScript tool running on the Node runtime.

### Commands

| Command       | Description                          | Args                    |
| ------------- | ------------------------------------ | ----------------------- |
| `open`        | Open a URL in the browser            | `[url]`                 |
| `goto`        | Navigate to a URL                    | `[url]`                 |
| `screenshot`  | Take a screenshot                    | —                       |
| `snapshot`    | Get accessibility snapshot (DOM)     | —                       |
| `click`       | Click an element by ref              | `[ref]`                 |
| `fill`        | Fill a form field                    | `[ref, value]`          |
| `type`        | Type text into focused element       | `[text]`                |
| `select`      | Select an option                     | `[ref, value]`          |
| `hover`       | Hover over an element                | `[ref]`                 |
| `press`       | Press a key                          | `[key]`                 |
| `resize`      | Resize the browser window            | `[width, height]`       |
| `eval`        | Evaluate JavaScript in the page      | `[expression]`          |
| `console`     | Get console messages                 | —                       |
| `network`     | Get network requests                 | —                       |
| `tab-list`    | List open tabs                       | —                       |
| `tab-new`     | Open a new tab                       | `[url]`                 |
| `tab-select`  | Switch to a tab                      | `[tab_id]`              |
| `tab-close`   | Close a tab                          | `[tab_id]` (optional)   |
| `close`       | Close the browser                    | —                       |
| `close-all`   | Close all browser instances          | —                       |

### Parameters

| Name      | Type    | Required | Default  | Description                                              |
| --------- | ------- | -------- | -------- | -------------------------------------------------------- |
| `command` | string  | ✅       | —        | Browser command (see table above)                        |
| `args`    | array   | ❌       | `[]`     | Positional arguments                                     |
| `flags`   | object  | ❌       | `{}`     | Named flags like `{ "headed": true, "filename": "page.png" }` |
| `session` | string  | ❌       | `"rye"`  | Named session for browser isolation                      |
| `timeout` | integer | ❌       | `30`     | Command timeout in seconds                               |

### Configuration

Browser config is resolved project → user → system from `.ai/config/web/browser.json`:

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

The `channel: "chromium"` setting is required to use Playwright's bundled Chromium. Without it, playwright-cli defaults to Google Chrome.

### Artifacts

Screenshots save to `.ai/cache/tools/rye/web/browser/screenshots/`. Snapshots save to `.ai/cache/tools/rye/web/browser/snapshots/`. Filenames include timestamps.

### Example

```python
# Open a page and take a screenshot
rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "open", "args": ["http://localhost:3000"]})

rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "screenshot"})

# Click an element by accessibility ref
rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "click", "args": ["e15"]})

# Fill a form field
rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "fill", "args": ["e22", "user@example.com"]})

# Open with a named session
rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "open", "args": ["http://localhost:3000"], "session": "my-session"})
```
