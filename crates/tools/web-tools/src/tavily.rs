//! Tavily web search — a dedicated, reliable search provider for LLM use.
//!
//! Deliberately separate from the scraper-based `search` tool: this one calls
//! the Tavily API (https://docs.tavily.com) exclusively, so the legacy tool is
//! left exactly as it was. Exposed as the `rye/web/tavily-search` bundle tool,
//! which declares `TAVILY_API_KEY` as a required secret (the daemon injects it
//! into the subprocess env).

use std::time::Duration;

use anyhow::{anyhow, Context};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

const DEFAULT_NUM_RESULTS: usize = 8;
const MAX_RESULTS: usize = 10;
const TAVILY_TIMEOUT_SECS: u64 = 12;
const TAVILY_ENDPOINT: &str = "https://api.tavily.com/search";
// Per-result snippet cap. Without this a single search can dump full scraped
// page content into the model context; several agentic searches then exhaust the
// chat token budget before the model can synthesize an answer.
const SNIPPET_MAX_CHARS: usize = 400;

#[derive(Debug, Deserialize)]
struct SearchParams {
    query: String,
    #[serde(default)]
    num_results: Option<usize>,
    // Accepted for backward-compat but IGNORED — this tool always uses "basic"
    // depth (see search_tavily). "advanced" returns noisy full-page content.
    #[serde(default)]
    #[allow(dead_code)]
    search_depth: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

#[derive(Debug, Serialize)]
pub struct SearchEnvelope {
    success: bool,
    provider: String,
    query: String,
    count: usize,
    results: Vec<SearchResult>,
    output: String,
}

#[derive(Debug, Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Debug, Deserialize)]
struct TavilyResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

pub fn execute_json(raw: &str) -> anyhow::Result<SearchEnvelope> {
    let params: SearchParams =
        serde_json::from_str(raw).context("parse tavily search params JSON")?;
    let query = params.query.trim();
    if query.is_empty() {
        return Err(anyhow!("query is required"));
    }
    let num_results = params
        .num_results
        .unwrap_or(DEFAULT_NUM_RESULTS)
        .clamp(1, MAX_RESULTS);
    let results = search_tavily(query, num_results)?;
    Ok(SearchEnvelope {
        success: true,
        provider: "tavily".to_string(),
        query: query.to_string(),
        count: results.len(),
        output: format_results(&results),
        results,
    })
}

/// The Tavily API key, read from the environment (injected by the daemon from
/// the item's `required_secrets`). Hard error when unset so misconfiguration
/// fails loudly rather than silently returning nothing.
fn tavily_api_key() -> anyhow::Result<String> {
    std::env::var("TAVILY_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .ok_or_else(|| anyhow!("TAVILY_API_KEY is not set"))
}

fn search_tavily(query: &str, num_results: usize) -> anyhow::Result<Vec<SearchResult>> {
    let api_key = tavily_api_key()?;
    let client = Client::builder()
        .timeout(Duration::from_secs(TAVILY_TIMEOUT_SECS))
        .user_agent("Mozilla/5.0 (compatible; RyeOS/1.0; +https://ryeos.dev)")
        .build()
        .context("build HTTP client")?;

    // Always "basic": clean, relevant snippets. "advanced" returns full scraped
    // page content (~15-20k chars/call) that is both noisy (nav/footers) and large
    // enough that a few agentic searches blow the chat token budget before the
    // model answers.
    let body = serde_json::json!({
        "query": query,
        "max_results": num_results,
        "search_depth": "basic",
        "topic": "general",
    });

    let resp = client
        .post(TAVILY_ENDPOINT)
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send()
        .context("Tavily request failed")?
        .error_for_status()
        .context("Tavily returned an error status")?;

    let parsed: TavilyResponse = resp.json().context("parse Tavily response body")?;

    Ok(parsed
        .results
        .into_iter()
        .filter(|r| !r.url.is_empty())
        .take(num_results)
        .map(|r| SearchResult {
            title: r.title,
            url: r.url,
            snippet: truncate_snippet(&r.content),
        })
        .collect())
}

/// Bound a single result snippet (char-safe) so one search can't flood the model
/// context with full page content.
fn truncate_snippet(s: &str) -> String {
    if s.chars().count() <= SNIPPET_MAX_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(SNIPPET_MAX_CHARS).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}

fn format_results(results: &[SearchResult]) -> String {
    let mut lines = Vec::new();
    for (index, result) in results.iter().enumerate() {
        lines.push(format!("{}. {}", index + 1, result.title));
        lines.push(format!("   {}", result.url));
        if !result.snippet.is_empty() {
            let snippet = if result.snippet.chars().count() > 220 {
                format!(
                    "{}...",
                    result.snippet.chars().take(220).collect::<String>()
                )
            } else {
                result.snippet.clone()
            };
            lines.push(format!("   {snippet}"));
        }
        lines.push(String::new());
    }
    lines.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tavily_results_and_drops_urlless() {
        let json = r#"{
          "query": "tvb dramas",
          "results": [
            {"title": "TVB Drama", "url": "https://example.com/a", "content": "A snippet.", "score": 0.9},
            {"title": "No URL", "url": "", "content": "dropped"}
          ],
          "answer": "..."
        }"#;
        let parsed: TavilyResponse = serde_json::from_str(json).expect("parse");
        let results: Vec<SearchResult> = parsed
            .results
            .into_iter()
            .filter(|r| !r.url.is_empty())
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                snippet: r.content,
            })
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "TVB Drama");
        assert_eq!(results[0].url, "https://example.com/a");
        assert_eq!(results[0].snippet, "A snippet.");
    }

    #[test]
    fn truncate_snippet_bounds_long_content() {
        let long = "word ".repeat(500); // 2500 chars
        let t = truncate_snippet(&long);
        assert!(t.chars().count() <= SNIPPET_MAX_CHARS + 1, "len={}", t.chars().count());
        assert!(t.ends_with('…'));
        assert_eq!(truncate_snippet("short snippet"), "short snippet");
    }

    #[test]
    fn formats_results_as_numbered_lines() {
        let results = vec![SearchResult {
            title: "Title".into(),
            url: "https://example.com".into(),
            snippet: "Snippet".into(),
        }];
        let out = format_results(&results);
        assert!(out.contains("1. Title"));
        assert!(out.contains("https://example.com"));
        assert!(out.contains("Snippet"));
    }
}
