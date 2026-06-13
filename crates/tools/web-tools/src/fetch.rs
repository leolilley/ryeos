use std::env;
use std::fs;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context};
use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use scraper::Html;
use serde::{Deserialize, Serialize};
use url::{Host, Url};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 50;
const DEFAULT_MAX_BYTES: usize = 1_000_000;
const MAX_MAX_BYTES: usize = 5_000_000;
const DEFAULT_MAX_OUTPUT_CHARS: usize = 50_000;
const MAX_MAX_OUTPUT_CHARS: usize = 200_000;
const MAX_REDIRECTS: usize = 10;
const USER_AGENT: &str = "Mozilla/5.0 (compatible; RyeOS/1.0; +https://ryeos.dev)";

#[derive(Debug, Deserialize)]
struct FetchParams {
    url: String,
    #[serde(default)]
    format: Option<FetchFormat>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    max_output_chars: Option<usize>,
    #[serde(default)]
    block_private_networks: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct FetchConfig {
    #[serde(default)]
    allow_private_networks: bool,
}
impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            allow_private_networks: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum FetchFormat {
    Markdown,
    Text,
    Html,
}
impl Default for FetchFormat {
    fn default() -> Self {
        Self::Markdown
    }
}

#[derive(Debug, Serialize)]
pub struct FetchEnvelope {
    success: bool,
    url: String,
    final_url: String,
    status: u16,
    format: FetchFormat,
    content_type: String,
    bytes: usize,
    truncated: bool,
    output: String,
}

struct FetchResponse {
    final_url: Url,
    status: u16,
    content_type: String,
    body: Vec<u8>,
    body_truncated: bool,
}

pub fn execute_json(raw: &str) -> anyhow::Result<FetchEnvelope> {
    let params: FetchParams = serde_json::from_str(raw).context("parse fetch params JSON")?;
    execute(params)
}

fn execute(params: FetchParams) -> anyhow::Result<FetchEnvelope> {
    let initial_url = parse_http_url(&params.url)?;
    let config = read_fetch_config()?;
    let timeout_secs = params
        .timeout_secs
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);
    let max_bytes = params
        .max_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .clamp(1, MAX_MAX_BYTES);
    let max_output_chars = params
        .max_output_chars
        .unwrap_or(DEFAULT_MAX_OUTPUT_CHARS)
        .clamp(1_000, MAX_MAX_OUTPUT_CHARS);
    let format = params.format.unwrap_or_default();
    let block_private_networks = if config.allow_private_networks {
        params.block_private_networks.unwrap_or(true)
    } else {
        true
    };
    let response = fetch_url(
        initial_url.clone(),
        Duration::from_secs(timeout_secs),
        max_bytes,
        block_private_networks,
    )?;
    let content = String::from_utf8_lossy(&response.body).into_owned();
    let is_html =
        response.content_type.contains("html") || content.to_ascii_lowercase().contains("<html");
    let converted = match (format, is_html) {
        (FetchFormat::Html, _) => content,
        (FetchFormat::Markdown, true) => html_to_markdown(&content),
        (FetchFormat::Text, true) => html_to_text(&content),
        (_, false) => content,
    };
    let (output, output_truncated) = truncate_chars(&converted, max_output_chars);
    Ok(FetchEnvelope {
        success: true,
        url: initial_url.to_string(),
        final_url: response.final_url.to_string(),
        status: response.status,
        format,
        content_type: response.content_type,
        bytes: response.body.len(),
        truncated: response.body_truncated || output_truncated,
        output,
    })
}

fn parse_http_url(raw: &str) -> anyhow::Result<Url> {
    let url = Url::parse(raw.trim()).context("parse url")?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        scheme => Err(anyhow!(
            "URL scheme `{scheme}` is not allowed; use http:// or https://"
        )),
    }
}

fn fetch_url(
    initial_url: Url,
    timeout: Duration,
    max_bytes: usize,
    block_private_networks: bool,
) -> anyhow::Result<FetchResponse> {
    let mut current = initial_url;
    for redirect_count in 0..=MAX_REDIRECTS {
        let client = build_client(&current, timeout, block_private_networks)?;
        let mut response = client
            .get(current.clone())
            .send()
            .with_context(|| format!("fetch {current}"))?;
        let status = response.status();
        if status.is_redirection() {
            if redirect_count == MAX_REDIRECTS {
                anyhow::bail!("too many redirects");
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| anyhow!("redirect without Location header"))?
                .to_str()
                .context("redirect Location is not valid UTF-8")?;
            current = current
                .join(location)
                .context("resolve redirect Location")?;
            parse_http_url(current.as_str())?;
            continue;
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let mut body = Vec::new();
        let mut limited = response.by_ref().take((max_bytes + 1) as u64);
        limited
            .read_to_end(&mut body)
            .context("read response body")?;
        let body_truncated = body.len() > max_bytes;
        body.truncate(max_bytes);
        return Ok(FetchResponse {
            final_url: current,
            status: status.as_u16(),
            content_type,
            body,
            body_truncated,
        });
    }
    unreachable!()
}

fn build_client(
    url: &Url,
    timeout: Duration,
    block_private_networks: bool,
) -> anyhow::Result<Client> {
    let mut builder = Client::builder()
        .timeout(timeout)
        .redirect(Policy::none())
        .user_agent(USER_AGENT);
    if block_private_networks {
        let (host, addrs) = checked_resolution(url)?;
        if let Some(host) = host {
            builder = builder.resolve_to_addrs(&host, &addrs);
        }
    }
    builder.build().context("build HTTP client")
}

fn checked_resolution(url: &Url) -> anyhow::Result<(Option<String>, Vec<SocketAddr>)> {
    let host = url.host().ok_or_else(|| anyhow!("url has no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("url has no port"))?;
    let (resolver_host, addrs) = match host {
        Host::Domain(host) => {
            let addrs = (host, port)
                .to_socket_addrs()
                .with_context(|| format!("resolve {host}"))?
                .collect::<Vec<_>>();
            (Some(host.to_string()), addrs)
        }
        Host::Ipv4(ip) => (None, vec![SocketAddr::new(IpAddr::V4(ip), port)]),
        Host::Ipv6(ip) => (None, vec![SocketAddr::new(IpAddr::V6(ip), port)]),
    };
    if addrs.is_empty() {
        anyhow::bail!("resolve {host}: no addresses");
    }
    for addr in &addrs {
        if is_private_ip(addr.ip()) {
            anyhow::bail!("target resolves to private or local address: {}", addr.ip());
        }
    }
    Ok((resolver_host, addrs))
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.octets()[0] == 0
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.segments()[0] & 0xfe00 == 0xfc00
                || ip.segments()[0] & 0xffc0 == 0xfe80
        }
    }
}

fn read_fetch_config() -> anyhow::Result<FetchConfig> {
    let Some(path) = fetch_config_path()? else {
        return Ok(FetchConfig::default());
    };
    if !path.exists() {
        return Ok(FetchConfig::default());
    }
    serde_yaml::from_str(&fs::read_to_string(&path)?)
        .with_context(|| format!("parse config {}", path.display()))
}

fn fetch_config_path() -> anyhow::Result<Option<PathBuf>> {
    let Some(app_root) = env::var_os("RYEOS_APP_ROOT") else {
        return Ok(None);
    };
    let app_root = PathBuf::from(app_root);
    if !app_root.is_absolute() {
        anyhow::bail!("RYEOS_APP_ROOT must be an absolute path");
    }
    Ok(Some(app_root.join(Path::new(".ai/config/web/fetch.yaml"))))
}

fn html_to_text(html: &str) -> String {
    let doc = Html::parse_document(html);
    doc.root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_to_markdown(html: &str) -> String {
    let doc = Html::parse_document(html);
    let mut blocks = Vec::new();
    let Ok(sel) = scraper::Selector::parse("h1, h2, h3, h4, h5, h6, p, li, pre") else {
        return html_to_text(html);
    };
    for node in doc.select(&sel) {
        let text = normalized_text(&node);
        if text.is_empty() {
            continue;
        }
        let block = match node.value().name() {
            "h1" => format!("# {text}"),
            "h2" => format!("## {text}"),
            "h3" => format!("### {text}"),
            "h4" => format!("#### {text}"),
            "h5" => format!("##### {text}"),
            "h6" => format!("###### {text}"),
            "li" => format!("- {text}"),
            "pre" => format!("```\n{}\n```", node.text().collect::<Vec<_>>().join("")),
            _ => render_links(node.html()).unwrap_or(text),
        };
        blocks.push(block);
    }
    if blocks.is_empty() {
        html_to_text(html)
    } else {
        blocks.join("\n\n")
    }
}

fn normalized_text(node: &scraper::ElementRef<'_>) -> String {
    node.text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_links(html: String) -> Option<String> {
    let doc = Html::parse_fragment(&html);
    let link_sel = scraper::Selector::parse("a").ok()?;
    let mut text = doc
        .root_element()
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for link in doc.select(&link_sel) {
        let label = link
            .text()
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        if !label.is_empty() {
            text = text.replace(&label, &format!("[{label}]({href})"));
        }
    }
    Some(text)
}

fn truncate_chars(value: &str, max_chars: usize) -> (String, bool) {
    if value.chars().count() <= max_chars {
        return (value.to_string(), false);
    }
    let mut out = value.chars().take(max_chars).collect::<String>();
    out.push_str("\n... [output truncated]");
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_http_urls() {
        assert!(parse_http_url("file:///tmp/x").is_err());
    }
    #[test]
    fn converts_basic_html_to_markdown() {
        let md = html_to_markdown(
            r#"<html><body><h1>Hello</h1><p>See <a href="https://example.com">example</a>.</p><ul><li>One</li></ul><script>bad()</script></body></html>"#,
        );
        assert!(md.contains("# Hello"));
        assert!(md.contains("[example](https://example.com)"));
        assert!(!md.contains("bad"));
    }
    #[test]
    fn converts_html_to_markdown_in_document_order_with_code() {
        let md = html_to_markdown(
            r#"<html><body><h1>Title</h1><p>Intro</p><h4>Details</h4><pre><code>let x = 1;
println!("{x}");</code></pre><p>Done</p><ul><li>Last</li></ul></body></html>"#,
        );
        assert_eq!(
            md,
            "# Title\n\nIntro\n\n#### Details\n\n```\nlet x = 1;\nprintln!(\"{x}\");\n```\n\nDone\n\n- Last"
        );
    }
    #[test]
    fn truncates_by_chars() {
        let (out, truncated) = truncate_chars("abcdef", 3);
        assert_eq!(out, "abc\n... [output truncated]");
        assert!(truncated);
    }
    #[test]
    fn blocks_private_ip_targets_when_enabled() {
        assert!(is_private_ip("127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("::ffff:10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("::ffff:192.168.0.1".parse().unwrap()));
    }

    #[test]
    fn blocks_ipv6_mapped_url_literals() {
        let url = parse_http_url("http://[::ffff:127.0.0.1]/").unwrap();
        let err = checked_resolution(&url).unwrap_err();
        assert!(err.to_string().contains("private or local address"));
    }

    #[test]
    fn blocks_private_networks_by_default() {
        let err = execute_json(r#"{"url":"http://127.0.0.1/"}"#).unwrap_err();
        assert!(err.to_string().contains("private or local address"));
    }
}
