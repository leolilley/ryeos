# rye:signed:2026-02-26T06:42:42Z:18f9c45fadee51fb5d89734e81ee942214ef9a8bb8028ab4bfb68d61cd4d6de8:NMG6TKi6uV1EFDiU_nwNKdLhd0IvBFgGMfzNOZLTfzgUm5NEbwiWIHRvHRtdbXYDXsXWRvRm-WQfg9M6j7joCQ==:4b987fd4e40303ac
"""Web search via configurable provider."""

import argparse
import json
import sys
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/web/search"
__tool_description__ = "Web search via configurable provider"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "query": {
            "type": "string",
            "description": "Search query",
        },
        "num_results": {
            "type": "integer",
            "default": 10,
            "description": "Number of results to return",
        },
        "provider": {
            "type": "string",
            "description": "Search provider (exa, duckduckgo). If not specified, uses configured default.",
        },
    },
    "required": ["query"],
}

DEFAULT_NUM_RESULTS = 10
MAX_RESULTS = 20


def get_provider_config(project_path: str, provider: str | None) -> dict:
    """Get provider configuration from project/user/system YAML."""
    from rye.utils.resolvers import get_user_space
    from rye.constants import AI_DIR
    import yaml

    config_paths = []

    if project_path:
        config_paths.append(Path(project_path) / AI_DIR / "config" / "web" / "websearch.yaml")

    user_config = Path(get_user_space()) / AI_DIR / "config" / "web" / "websearch.yaml"
    config_paths.append(user_config)

    for config_path in config_paths:
        if config_path.exists():
            try:
                content = config_path.read_text()
                config = yaml.safe_load(content) or {}
                if provider:
                    return config.get("providers", {}).get(provider, {})
                return config.get("default_provider", {})
            except Exception:
                continue

    return {}


def search_duckduckgo(query: str, num_results: int) -> list[dict]:
    """Search using DuckDuckGo (no API key required)."""
    import urllib.request
    import urllib.parse
    import re

    results = []

    try:
        url = f"https://html.duckduckgo.com/html/?q={urllib.parse.quote(query)}"
        request = urllib.request.Request(
            url, headers={"User-Agent": "Mozilla/5.0 (compatible; RyeBot/1.0)"}
        )

        with urllib.request.urlopen(request, timeout=15) as response:
            html = response.read().decode("utf-8", errors="replace")

        pattern = r'<a rel="nofollow" class="result__a" href="([^"]+)"[^>]*>([^<]+)</a>'
        matches = re.findall(pattern, html)

        for href, title in matches[:num_results]:
            if href.startswith("//"):
                href = "https:" + href

            uddg_match = re.search(r"uddg=([^&]+)", href)
            if uddg_match:
                import urllib.parse

                href = urllib.parse.unquote(uddg_match.group(1))

            results.append(
                {
                    "title": title.strip(),
                    "url": href,
                    "snippet": "",
                }
            )

        snippet_pattern = r'<a class="result__snippet"[^>]*>([^<]+)</a>'
        snippet_matches = re.findall(snippet_pattern, html)
        for i, snippet in enumerate(snippet_matches[: len(results)]):
            if i < len(results):
                results[i]["snippet"] = snippet.strip()

    except Exception as e:
        raise Exception(f"DuckDuckGo search failed: {e}")

    return results


def search_exa(query: str, num_results: int, api_key: str) -> list[dict]:
    """Search using Exa API (requires API key)."""
    import urllib.request
    import urllib.error

    results = []

    try:
        data = json.dumps(
            {
                "query": query,
                "numResults": num_results,
                "useAutoprompt": True,
            }
        ).encode("utf-8")

        request = urllib.request.Request(
            "https://api.exa.ai/search",
            data=data,
            headers={
                "Content-Type": "application/json",
                "x-api-key": api_key,
            },
        )

        with urllib.request.urlopen(request, timeout=30) as response:
            resp_data = json.loads(response.read().decode("utf-8"))

        for item in resp_data.get("results", [])[:num_results]:
            results.append(
                {
                    "title": item.get("title", ""),
                    "url": item.get("url", ""),
                    "snippet": item.get("text", "")[:300] if item.get("text") else "",
                }
            )

    except urllib.error.HTTPError as e:
        error_body = e.read().decode("utf-8") if e.fp else ""
        raise Exception(f"Exa API error {e.code}: {error_body}")
    except Exception as e:
        raise Exception(f"Exa search failed: {e}")

    return results


def format_results(results: list[dict]) -> str:
    """Format search results as text."""
    lines = []
    for i, result in enumerate(results, 1):
        lines.append(f"{i}. {result['title']}")
        lines.append(f"   {result['url']}")
        if result.get("snippet"):
            snippet = result["snippet"]
            if len(snippet) > 200:
                snippet = snippet[:200] + "..."
            lines.append(f"   {snippet}")
        lines.append("")
    return "\n".join(lines)


def execute(params: dict, project_path: str) -> dict:
    query = params["query"]
    num_results = min(params.get("num_results", DEFAULT_NUM_RESULTS), MAX_RESULTS)
    requested_provider = params.get("provider")

    provider_config = get_provider_config(project_path, requested_provider)
    provider = provider_config.get("type", "duckduckgo")
    api_key = provider_config.get("api_key")

    try:
        if provider == "exa":
            if not api_key:
                return {
                    "success": False,
                    "error": "Exa provider requires API key. Configure in .ai/config/web/websearch.yaml",
                }
            results = search_exa(query, num_results, api_key)
        else:
            results = search_duckduckgo(query, num_results)

        output = format_results(results)

        return {
            "success": True,
            "output": output,
            "results": results,
            "count": len(results),
            "provider": provider,
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
