use std::io::{self, Read};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, Context};
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

const DEFAULT_NUM_RESULTS: usize = 10;
const MAX_RESULTS: usize = 20;

#[derive(Debug, Deserialize)]
struct SearchParams {
    query: String,
    #[serde(default)]
    num_results: Option<usize>,
}

#[derive(Debug, Serialize)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

#[derive(Debug, Serialize)]
struct SearchEnvelope {
    success: bool,
    provider: &'static str,
    query: String,
    count: usize,
    results: Vec<SearchResult>,
    output: String,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    success: bool,
    error: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            let envelope = ErrorEnvelope {
                success: false,
                error: format!("{err:#}"),
            };
            println!(
                "{}",
                serde_json::to_string(&envelope).unwrap_or_else(|_| {
                    "{\"success\":false,\"error\":\"failed to serialize error\"}".to_string()
                })
            );
            ExitCode::SUCCESS
        }
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        return Err(anyhow!("missing command; expected `search --stdin-json`"));
    };
    if command != "search" {
        return Err(anyhow!("unknown command `{command}`; expected `search`"));
    }
    let stdin_json = args.any(|arg| arg == "--stdin-json");
    if !stdin_json {
        return Err(anyhow!("search requires --stdin-json"));
    }

    let params = read_search_params()?;
    let query = params.query.trim();
    if query.is_empty() {
        return Err(anyhow!("query is required"));
    }
    let num_results = params
        .num_results
        .unwrap_or(DEFAULT_NUM_RESULTS)
        .clamp(1, MAX_RESULTS);

    let results = search_duckduckgo(query, num_results)?;
    let envelope = SearchEnvelope {
        success: true,
        provider: "duckduckgo",
        query: query.to_string(),
        count: results.len(),
        output: format_results(&results),
        results,
    };
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}

fn read_search_params() -> anyhow::Result<SearchParams> {
    let mut raw = String::new();
    io::stdin().read_to_string(&mut raw).context("read stdin")?;
    serde_json::from_str(&raw).context("parse search params JSON")
}

fn search_duckduckgo(query: &str, num_results: usize) -> anyhow::Result<Vec<SearchResult>> {
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (compatible; RyeOS/1.0; +https://ryeos.dev)")
        .build()
        .context("build HTTP client")?;

    let html = client
        .get("https://html.duckduckgo.com/html/")
        .query(&[("q", query)])
        .send()
        .context("DuckDuckGo request failed")?
        .error_for_status()
        .context("DuckDuckGo returned an error status")?
        .text()
        .context("read DuckDuckGo response body")?;

    if is_duckduckgo_challenge(&html) {
        return Err(anyhow!("DuckDuckGo returned an anti-bot challenge"));
    }

    Ok(parse_duckduckgo_html(&html, num_results))
}

fn is_duckduckgo_challenge(html: &str) -> bool {
    html.contains("anomaly-modal")
        || html.contains("anomaly-modal__image")
        || html.contains("/assets/anomaly/images/challenge/")
}

fn parse_duckduckgo_html(html: &str, num_results: usize) -> Vec<SearchResult> {
    let result_re = Regex::new(
        r#"(?is)<a\s+rel="nofollow"\s+class="result__a"\s+href="([^"]+)"[^>]*>(.*?)</a>"#,
    )
    .expect("valid result regex");
    let snippet_re = Regex::new(r#"(?is)<a\s+class="result__snippet"[^>]*>(.*?)</a>"#)
        .expect("valid snippet regex");

    let snippets: Vec<String> = snippet_re
        .captures_iter(html)
        .map(|cap| clean_html_fragment(&cap[1]))
        .collect();

    result_re
        .captures_iter(html)
        .take(num_results)
        .enumerate()
        .map(|(index, cap)| SearchResult {
            title: clean_html_fragment(&cap[2]),
            url: normalize_duckduckgo_url(&cap[1]),
            snippet: snippets.get(index).cloned().unwrap_or_default(),
        })
        .collect()
}

fn normalize_duckduckgo_url(raw: &str) -> String {
    let mut url = html_unescape(raw);
    if let Some(rest) = url.strip_prefix("//") {
        url = format!("https://{rest}");
    }
    if let Some(uddg) = extract_query_param(&url, "uddg") {
        return percent_decode(&uddg);
    }
    url
}

fn extract_query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == name {
            return Some(value.to_string());
        }
    }
    None
}

fn clean_html_fragment(value: &str) -> String {
    let tag_re = Regex::new(r#"(?is)<[^>]+>"#).expect("valid tag regex");
    let without_tags = tag_re.replace_all(value, "");
    html_unescape(without_tags.trim())
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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
    fn parses_duckduckgo_result_links_and_snippets() {
        let html = r#"
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fone&amp;rut=abc">One &amp; Done</a>
          <a class="result__snippet" href="//duckduckgo.com/l/?uddg=x">First <b>snippet</b>.</a>
          <a rel="nofollow" class="result__a" href="https://example.com/two">Two</a>
        "#;
        let results = parse_duckduckgo_html(html, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "One & Done");
        assert_eq!(results[0].url, "https://example.com/one");
        assert_eq!(results[0].snippet, "First snippet.");
        assert_eq!(results[1].url, "https://example.com/two");
    }

    #[test]
    fn percent_decode_handles_utf8() {
        assert_eq!(percent_decode("hello+%E2%9C%93"), "hello ✓");
    }

    #[test]
    fn detects_duckduckgo_anomaly_challenge() {
        let html = r#"
          <div class="anomaly-modal__box">
            <img src="../assets/anomaly/images/challenge/example.jpg">
          </div>
        "#;

        assert!(is_duckduckgo_challenge(html));
    }
}
