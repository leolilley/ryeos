use std::io::{self, Read};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, Context};
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

const DEFAULT_NUM_RESULTS: usize = 10;
const MAX_RESULTS: usize = 20;
const PRIMARY_TIMEOUT_SECS: u64 = 10;
const DUCKDUCKGO_TIMEOUT_SECS: u64 = 5;

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
    provider: String,
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

    let (provider, results) = search_web(query, num_results)?;
    let envelope = SearchEnvelope {
        success: true,
        provider,
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

fn search_web(query: &str, num_results: usize) -> anyhow::Result<(String, Vec<SearchResult>)> {
    match search_bing_web_rss(query, num_results) {
        Ok(results) if has_relevant_result(query, &results) => {
            Ok(("bing_web_rss".to_string(), results))
        }
        Ok(_) => search_secondary_fallback(query, num_results)
            .context("Bing Web RSS returned no relevant results and secondary fallback failed"),
        Err(bing_err) => search_secondary_fallback(query, num_results).with_context(|| {
            format!("Bing Web RSS failed ({bing_err:#}) and secondary fallback failed")
        }),
    }
}

fn http_client(timeout_secs: u64) -> anyhow::Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent("Mozilla/5.0 (compatible; RyeOS/1.0; +https://ryeos.dev)")
        .build()
        .context("build HTTP client")
}

fn search_bing_web_rss(query: &str, num_results: usize) -> anyhow::Result<Vec<SearchResult>> {
    let client = http_client(PRIMARY_TIMEOUT_SECS)?;

    let xml = client
        .get("https://www.bing.com/search")
        .query(&[
            ("q", query),
            ("format", "rss"),
            ("setlang", "en-US"),
            ("adlt", "strict"),
        ])
        .send()
        .context("Bing Web RSS request failed")?
        .error_for_status()
        .context("Bing Web RSS returned an error status")?
        .text()
        .context("read Bing Web RSS response body")?;

    let results = parse_rss(&xml, num_results);
    if results.is_empty() {
        anyhow::bail!("Bing Web RSS returned no parseable results");
    }
    Ok(results)
}

fn search_secondary_fallback(
    query: &str,
    num_results: usize,
) -> anyhow::Result<(String, Vec<SearchResult>)> {
    match search_duckduckgo(query, num_results) {
        Ok(results) if !results.is_empty() => Ok(("duckduckgo".to_string(), results)),
        Ok(_) => search_rss_fallback(query, num_results)
            .context("DuckDuckGo returned no results and RSS fallback failed"),
        Err(ddg_err) => search_rss_fallback(query, num_results)
            .with_context(|| format!("DuckDuckGo failed ({ddg_err:#}) and RSS fallback failed")),
    }
}

fn search_duckduckgo(query: &str, num_results: usize) -> anyhow::Result<Vec<SearchResult>> {
    let client = http_client(DUCKDUCKGO_TIMEOUT_SECS)?;

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

fn search_rss_fallback(
    query: &str,
    num_results: usize,
) -> anyhow::Result<(String, Vec<SearchResult>)> {
    match search_google_news_rss(query, num_results) {
        Ok(results) if !results.is_empty() => Ok(("google_news_rss".to_string(), results)),
        Ok(_) => search_bing_news_rss(query, num_results)
            .map(|results| ("bing_news_rss".to_string(), results))
            .context("Google News RSS returned no results and Bing News RSS fallback failed"),
        Err(google_err) => search_bing_news_rss(query, num_results)
            .map(|results| ("bing_news_rss".to_string(), results))
            .with_context(|| {
                format!("Google News RSS failed ({google_err:#}) and Bing News RSS fallback failed")
            }),
    }
}

fn search_google_news_rss(query: &str, num_results: usize) -> anyhow::Result<Vec<SearchResult>> {
    let client = http_client(PRIMARY_TIMEOUT_SECS)?;

    let xml = client
        .get("https://news.google.com/rss/search")
        .query(&[
            ("q", query),
            ("hl", "en-US"),
            ("gl", "US"),
            ("ceid", "US:en"),
        ])
        .send()
        .context("Google News RSS request failed")?
        .error_for_status()
        .context("Google News RSS returned an error status")?
        .text()
        .context("read Google News RSS response body")?;

    let results = parse_rss(&xml, num_results);
    if results.is_empty() {
        anyhow::bail!("Google News RSS returned no parseable results");
    }
    Ok(results)
}

fn search_bing_news_rss(query: &str, num_results: usize) -> anyhow::Result<Vec<SearchResult>> {
    let client = http_client(PRIMARY_TIMEOUT_SECS)?;

    let xml = client
        .get("https://www.bing.com/news/search")
        .query(&[
            ("q", query),
            ("format", "rss"),
            ("setlang", "en-US"),
            ("adlt", "strict"),
        ])
        .send()
        .context("Bing News RSS request failed")?
        .error_for_status()
        .context("Bing News RSS returned an error status")?
        .text()
        .context("read Bing News RSS response body")?;

    let results = parse_rss(&xml, num_results);
    if results.is_empty() {
        anyhow::bail!("Bing News RSS returned no parseable results");
    }
    Ok(results)
}

fn is_duckduckgo_challenge(html: &str) -> bool {
    html.contains("anomaly-modal")
        || html.contains("anomaly-modal__image")
        || html.contains("/assets/anomaly/images/challenge/")
}

fn has_relevant_result(query: &str, results: &[SearchResult]) -> bool {
    let terms = query_terms(query);
    if terms.is_empty() {
        return !results.is_empty();
    }

    results.iter().any(|result| {
        let haystack = format!("{} {} {}", result.title, result.url, result.snippet).to_lowercase();
        terms.iter().any(|term| haystack.contains(term))
    })
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|term| {
            term.trim_matches(|ch: char| !ch.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|term| term.chars().count() >= 3)
        .collect()
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

fn parse_rss(xml: &str, num_results: usize) -> Vec<SearchResult> {
    let item_re = Regex::new(r#"(?is)<item\b[^>]*>(.*?)</item>"#).expect("valid item regex");
    let title_re = Regex::new(r#"(?is)<title>(.*?)</title>"#).expect("valid title regex");
    let link_re = Regex::new(r#"(?is)<link>(.*?)</link>"#).expect("valid link regex");
    let description_re =
        Regex::new(r#"(?is)<description>(.*?)</description>"#).expect("valid description regex");

    item_re
        .captures_iter(xml)
        .filter_map(|item| {
            let body = &item[1];
            let title = title_re
                .captures(body)
                .map(|cap| clean_xml_text(&cap[1]))
                .unwrap_or_default();
            let url = link_re
                .captures(body)
                .map(|cap| clean_xml_text(&cap[1]))
                .unwrap_or_default();
            if title.is_empty() || url.is_empty() {
                return None;
            }
            let snippet = description_re
                .captures(body)
                .map(|cap| clean_html_fragment(&clean_xml_text(&cap[1])))
                .unwrap_or_default();
            Some(SearchResult {
                title,
                url,
                snippet,
            })
        })
        .take(num_results)
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
        .replace("&apos;", "'")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn clean_xml_text(value: &str) -> String {
    let trimmed = value.trim();
    let without_cdata = trimmed
        .strip_prefix("<![CDATA[")
        .and_then(|value| value.strip_suffix("]]>").map(str::trim))
        .unwrap_or(trimmed);
    html_unescape(without_cdata)
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

    #[test]
    fn parses_rss_items() {
        let xml = r#"
          <rss><channel>
            <item>
              <title>TVB &amp; Hong Kong TV</title>
              <link>https://www.tvb.com/</link>
              <description><![CDATA[Official <b>television</b> site.]]></description>
            </item>
          </channel></rss>
        "#;

        let results = parse_rss(xml, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "TVB & Hong Kong TV");
        assert_eq!(results[0].url, "https://www.tvb.com/");
        assert_eq!(results[0].snippet, "Official television site.");
    }

    #[test]
    fn relevance_check_rejects_unrelated_results() {
        let results = vec![SearchResult {
            title: "Quote of the Day".to_string(),
            url: "https://example.com/quotes".to_string(),
            snippet: "Inspirational sayings".to_string(),
        }];

        assert!(!has_relevant_result("TVB 正義女神", &results));
    }

    #[test]
    fn relevance_check_accepts_matching_results() {
        let results = vec![SearchResult {
            title: "正義女神｜最新 TVB 劇情".to_string(),
            url: "https://example.com/themis".to_string(),
            snippet: String::new(),
        }];

        assert!(has_relevant_result("TVB 正義女神", &results));
    }
}
