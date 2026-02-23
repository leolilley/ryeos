# rye:signed:2026-02-23T01:12:00Z:56f3dba6f88c72b76a4bff1f834c8663060da32d1e36ff8b1b77f4100bac4f42:tKo9-Yk3cJlna_fJpr7enYEhVhjUz_5OqcHqTG8viSHMjgWbkY90aanKSWe1xOWh2IT4iX9ztYO9BpCzElGiBw==:9fbfabe975fa5a7f
"""Fetch URL content with optional format conversion."""

import argparse
import json
import sys
import urllib.request
import urllib.error
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/web/fetch"
__tool_description__ = "Fetch URL content"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "url": {
            "type": "string",
            "description": "URL to fetch",
        },
        "format": {
            "type": "string",
            "enum": ["text", "markdown", "html"],
            "default": "markdown",
            "description": "Output format",
        },
        "timeout": {
            "type": "integer",
            "default": 30,
            "description": "Timeout in seconds",
        },
    },
    "required": ["url"],
}

DEFAULT_TIMEOUT = 30
MAX_CONTENT_BYTES = 512000
USER_AGENT = "Mozilla/5.0 (compatible; RyeBot/1.0)"


def html_to_markdown(html: str) -> str:
    """Simple HTML to markdown conversion."""
    import re

    text = html

    text = re.sub(
        r"<script[^>]*>.*?</script>", "", text, flags=re.DOTALL | re.IGNORECASE
    )
    text = re.sub(r"<style[^>]*>.*?</style>", "", text, flags=re.DOTALL | re.IGNORECASE)
    text = re.sub(r"<!--.*?-->", "", text, flags=re.DOTALL)

    text = re.sub(r"<h1[^>]*>(.*?)</h1>", r"# \1\n", text, flags=re.IGNORECASE)
    text = re.sub(r"<h2[^>]*>(.*?)</h2>", r"## \1\n", text, flags=re.IGNORECASE)
    text = re.sub(r"<h3[^>]*>(.*?)</h3>", r"### \1\n", text, flags=re.IGNORECASE)
    text = re.sub(r"<h4[^>]*>(.*?)</h4>", r"#### \1\n", text, flags=re.IGNORECASE)
    text = re.sub(r"<h5[^>]*>(.*?)</h5>", r"##### \1\n", text, flags=re.IGNORECASE)
    text = re.sub(r"<h6[^>]*>(.*?)</h6>", r"###### \1\n", text, flags=re.IGNORECASE)

    text = re.sub(
        r'<a[^>]*href=["\']([^"\']*)["\'][^>]*>(.*?)</a>',
        r"[\2](\1)",
        text,
        flags=re.IGNORECASE,
    )

    text = re.sub(r"<strong[^>]*>(.*?)</strong>", r"**\1**", text, flags=re.IGNORECASE)
    text = re.sub(r"<b[^>]*>(.*?)</b>", r"**\1**", text, flags=re.IGNORECASE)
    text = re.sub(r"<em[^>]*>(.*?)</em>", r"*\1*", text, flags=re.IGNORECASE)
    text = re.sub(r"<i[^>]*>(.*?)</i>", r"*\1*", text, flags=re.IGNORECASE)

    text = re.sub(r"<code[^>]*>(.*?)</code>", r"`\1`", text, flags=re.IGNORECASE)
    text = re.sub(
        r"<pre[^>]*>(.*?)</pre>",
        r"\n```\n\1\n```\n",
        text,
        flags=re.DOTALL | re.IGNORECASE,
    )

    text = re.sub(r"<li[^>]*>(.*?)</li>", r"- \1\n", text, flags=re.IGNORECASE)
    text = re.sub(r"<br\s*/?>", "\n", text, flags=re.IGNORECASE)
    text = re.sub(
        r"<p[^>]*>(.*?)</p>", r"\1\n\n", text, flags=re.DOTALL | re.IGNORECASE
    )

    text = re.sub(r"<[^>]+>", "", text)

    text = re.sub(r"&nbsp;", " ", text)
    text = re.sub(r"&amp;", "&", text)
    text = re.sub(r"&lt;", "<", text)
    text = re.sub(r"&gt;", ">", text)
    text = re.sub(r"&quot;", '"', text)
    text = re.sub(r"&#39;", "'", text)

    text = re.sub(r"\n{3,}", "\n\n", text)
    text = text.strip()

    return text


def strip_html_tags(html: str) -> str:
    """Strip all HTML tags and return plain text."""
    import re

    text = html

    text = re.sub(
        r"<script[^>]*>.*?</script>", "", text, flags=re.DOTALL | re.IGNORECASE
    )
    text = re.sub(r"<style[^>]*>.*?</style>", "", text, flags=re.DOTALL | re.IGNORECASE)
    text = re.sub(r"<!--.*?-->", "", text, flags=re.DOTALL)

    text = re.sub(r"<br\s*/?>", "\n", text, flags=re.IGNORECASE)
    text = re.sub(r"<p[^>]*>", "\n", text, flags=re.IGNORECASE)
    text = re.sub(r"</p>", "\n", text, flags=re.IGNORECASE)
    text = re.sub(r"<li[^>]*>", "\n- ", text, flags=re.IGNORECASE)

    text = re.sub(r"<[^>]+>", "", text)

    text = re.sub(r"&nbsp;", " ", text)
    text = re.sub(r"&amp;", "&", text)
    text = re.sub(r"&lt;", "<", text)
    text = re.sub(r"&gt;", ">", text)
    text = re.sub(r"&quot;", '"', text)
    text = re.sub(r"&#39;", "'", text)

    text = re.sub(r"\n{3,}", "\n\n", text)
    text = text.strip()

    return text


def fetch_url(url: str, timeout: int) -> tuple[str, str]:
    """Fetch URL content using urllib.

    Returns:
        (content, content_type)
    """
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})

    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            content_type = response.headers.get("Content-Type", "text/html")

            raw = response.read(MAX_CONTENT_BYTES + 1)
            if len(raw) > MAX_CONTENT_BYTES:
                raw = raw[:MAX_CONTENT_BYTES]

            encoding = "utf-8"
            if "charset=" in content_type:
                for part in content_type.split(";"):
                    part = part.strip()
                    if part.startswith("charset="):
                        encoding = part[8:]
                        break

            try:
                content = raw.decode(encoding)
            except (UnicodeDecodeError, LookupError):
                content = raw.decode("utf-8", errors="replace")

            return content, content_type
    except urllib.error.HTTPError as e:
        raise Exception(f"HTTP {e.code}: {e.reason}")
    except urllib.error.URLError as e:
        raise Exception(f"URL error: {e.reason}")


def execute(params: dict, project_path: str) -> dict:
    url = params["url"]
    output_format = params.get("format", "markdown")
    timeout = params.get("timeout", DEFAULT_TIMEOUT)

    if not url.startswith(("http://", "https://")):
        return {"success": False, "error": "URL must start with http:// or https://"}

    try:
        content, content_type = fetch_url(url, timeout)

        is_html = (
            "text/html" in content_type or "<!DOCTYPE html" in content.lower()[:500]
        )

        if output_format == "markdown" and is_html:
            output = html_to_markdown(content)
        elif output_format == "text" and is_html:
            output = strip_html_tags(content)
        else:
            output = content

        return {
            "success": True,
            "output": output,
            "url": url,
            "format": output_format,
            "bytes": len(output.encode("utf-8")),
            "content_type": content_type,
        }
    except Exception as e:
        return {"success": False, "error": str(e)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result))
