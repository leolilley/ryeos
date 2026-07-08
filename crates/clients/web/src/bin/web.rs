//! `web` — mints a RyeOS RyeOs launch token and opens the browser.
//!
//! This binary is the `cli_exec` target for `client:ryeos/web`. It:
//! 1. Parses launch args (surface, project, read_only).
//! 2. Resolves the daemon URL from `daemon.json` or `RYEOSD_URL`.
//! 3. Discovers the daemon's public key for request signing.
//! 4. Calls `POST /ui/api/launch/mint` with the launch context.
//! 5. Opens the browser at the daemon-returned `launch_url`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use base64::Engine;
use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "web", about = "Launch RyeOS in the browser")]
struct Cli {
    /// Surface ref backing RyeOs. Command descriptors provide the default.
    #[arg(long = "surface", required = true)]
    surface: String,

    /// Project path
    #[arg(long = "project")]
    project: Option<PathBuf>,

    /// Read-only mode
    #[arg(long = "read-only")]
    read_only: bool,

    /// Allow browser actions that can mutate daemon/project state.
    /// RyeOs defaults to read-only unless this is explicit.
    #[arg(long = "allow-actions")]
    allow_actions: bool,

    /// Print the minted one-shot launch URL to stdout.
    #[arg(long = "print-url")]
    print_url: bool,

    /// Mint a launch URL but do not open a browser.
    #[arg(long = "no-open")]
    no_open: bool,

    /// Bind RyeOs principal storage to this launcher's signing-key principal.
    #[arg(long = "hosted-principal")]
    hosted_principal: bool,
}

/// Request body for `ui.launch.mint`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct MintRequest {
    surface_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_path: Option<String>,
    read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_principal_id: Option<String>,
}

/// Response from `ui.launch.mint`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MintResponse {
    launch_url: String,
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    token: String,
}

/// The daemon route the launcher mints against. Used for BOTH the signed
/// canonical path and the request URL so they can never drift — the signature
/// is path-bound, so a mismatch would silently fail verification.
const MINT_PATH: &str = "/ui/api/launch/mint";

/// Connect timeout for daemon HTTP calls. Without it a dead or filtered host
/// hangs the launcher for the OS connect timeout (minutes). Mirrors the CLI.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Total-request cap for the control-plane discovery + mint calls.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    // Resolve daemon URL.
    let daemon_url = resolve_daemon_url()?;

    // Resolve signing key.
    let app_root = discover_app_root()?;
    let signer = WebSigner::resolve(&app_root)?;

    // Discover the daemon audience (principal_id) and the effective base URL
    // after any http→https edge redirect. The signed request targets that base
    // directly — a redirected POST would be downgraded to GET and break the
    // signature.
    let discovered = discover_audience(&daemon_url).await?;

    let project_path = cli.project.or_else(project_from_cli_env);

    let mint_req = MintRequest {
        surface_ref: cli.surface,
        project_path: project_path.map(|p| p.to_string_lossy().to_string()),
        read_only: cli.read_only || !cli.allow_actions,
        user_principal_id: cli
            .hosted_principal
            .then(|| format!("fp:{}", signer.fingerprint)),
    };

    let body = serde_json::to_vec(&mint_req).context("serialize mint request")?;

    // Sign the request. `MINT_PATH` drives BOTH the signed canonical path and
    // the URL below, so the path the signature is bound to can never drift.
    let sign_headers = signer.sign("POST", MINT_PATH, &body, &discovered.principal_id)?;

    // Send the mint request with a client that does NOT auto-follow redirects:
    // a 3xx on a signed POST must fail loud, never be silently downgraded to a
    // GET (which would invalidate the method/path/body-bound signature).
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build signed HTTP client")?;
    let resp = client
        .post(format!("{}{}", discovered.effective_base_url, MINT_PATH))
        .header("Content-Type", "application/json")
        .header("x-ryeos-key-id", &sign_headers.key_id)
        .header("x-ryeos-timestamp", &sign_headers.timestamp)
        .header("x-ryeos-nonce", &sign_headers.nonce)
        .header("x-ryeos-signature", &sign_headers.signature)
        .body(body)
        .send()
        .await
        .context("send mint request to daemon")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("mint request failed: {status} {text}");
    }

    let mint_resp: MintResponse = resp.json().await.context("parse mint response")?;

    if cli.print_url || cli.no_open {
        println!("{}", mint_resp.launch_url);
    }

    if cli.no_open {
        return Ok(());
    }

    // Open the browser at the daemon-returned launch URL.
    eprintln!("Opening browser: {}", mint_resp.launch_url);
    open_browser(&mint_resp.launch_url)?;

    Ok(())
}

// ── Daemon URL discovery ───────────────────────────────────────────────

fn resolve_daemon_url() -> Result<String> {
    // Check RYEOSD_URL env var first.
    if let Ok(url) = std::env::var("RYEOSD_URL") {
        return Ok(url.trim_end_matches('/').to_string());
    }

    // Try daemon.json discovery.
    let app_root = discover_app_root()?;
    let daemon_json = app_root.join("daemon.json");
    if daemon_json.exists() {
        let raw = std::fs::read_to_string(&daemon_json).context("read daemon.json")?;
        let v: serde_json::Value = serde_json::from_str(&raw).context("parse daemon.json")?;
        if let Some(bind) = v.get("bind").and_then(|b| b.as_str()) {
            return Ok(format!("http://{bind}"));
        }
    }

    bail!("cannot resolve daemon URL: set RYEOSD_URL or ensure daemon is running")
}

fn discover_app_root() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("RYEOS_APP_ROOT") {
        return Ok(PathBuf::from(dir));
    }
    let base = dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .context("cannot discover local data dir")?;
    Ok(base.join("ryeos"))
}

// ── Request signing ────────────────────────────────────────────────────

struct WebSigner {
    signing_key: lillux::crypto::SigningKey,
    fingerprint: String,
}

struct SignedHeaders {
    key_id: String,
    timestamp: String,
    nonce: String,
    signature: String,
}

impl WebSigner {
    fn resolve(app_root: &std::path::Path) -> Result<Self> {
        let operator_key = app_root
            .join(ryeos_engine::AI_DIR)
            .join("config")
            .join("keys")
            .join("signing")
            .join("private_key.pem");

        if operator_key.exists() {
            return Self::load_from(operator_key);
        }

        bail!(
            "no signing key found at <app_root>/{}/config/keys/signing/private_key.pem",
            ryeos_engine::AI_DIR
        )
    }

    fn load_from(path: PathBuf) -> Result<Self> {
        let sk = lillux::crypto::load_signing_key(&path)
            .with_context(|| format!("load signing key from {}", path.display()))?;
        let fp = lillux::crypto::fingerprint(&sk.verifying_key());
        Ok(Self {
            signing_key: sk,
            fingerprint: fp,
        })
    }

    /// Build canonical request and produce the four `x-ryeos-*` headers.
    fn sign(
        &self,
        method: &str,
        path_and_query: &str,
        body: &[u8],
        audience: &str,
    ) -> Result<SignedHeaders> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let nonce_bytes = rand::Rng::gen::<[u8; 16]>(&mut rand::thread_rng());
        let nonce = hex::encode(nonce_bytes);

        let body_hash = lillux::cas::sha256_hex(body);
        let canon_path = canonicalize_path(path_and_query);

        let string_to_sign = format!(
            "ryeos-request-v1\n{}\n{}\n{}\n{}\n{}\n{}",
            method.to_uppercase(),
            canon_path,
            body_hash,
            timestamp,
            nonce,
            audience,
        );

        let content_hash = lillux::cas::sha256_hex(string_to_sign.as_bytes());
        let sig = lillux::crypto::Signer::sign(&self.signing_key, content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

        Ok(SignedHeaders {
            key_id: format!("fp:{}", self.fingerprint),
            timestamp: timestamp.to_string(),
            nonce,
            signature: sig_b64,
        })
    }
}

/// Sort query parameters alphabetically and normalise the path.
fn canonicalize_path(path_and_query: &str) -> String {
    if let Some((path, query)) = path_and_query.split_once('?') {
        let mut params: BTreeMap<String, String> = BTreeMap::new();
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                params.insert(k.to_string(), v.to_string());
            } else if !pair.is_empty() {
                params.insert(pair.to_string(), String::new());
            }
        }
        let sorted: Vec<String> = params
            .into_iter()
            .map(|(k, v)| if v.is_empty() { k } else { format!("{k}={v}") })
            .collect();
        format!("{}?{}", path, sorted.join("&"))
    } else {
        path_and_query.to_string()
    }
}

// ── Audience discovery ─────────────────────────────────────────────────

/// Result of audience discovery: the signing audience plus the effective base
/// URL the daemon answered on after any redirects.
struct Discovered {
    principal_id: String,
    effective_base_url: String,
}

async fn discover_audience(daemon_url: &str) -> Result<Discovered> {
    // Discovery is an UNSIGNED GET, so it may safely follow an http→https edge
    // redirect; the client follows redirects (default policy) and negotiates TLS.
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("build discovery HTTP client")?;
    let resp = client
        .get(format!("{}/public-key", daemon_url.trim_end_matches('/')))
        .send()
        .await
        .context("fetch daemon public key")?;

    // The origin the daemon answered on, post-redirect, minus the /public-key
    // probe path — so the signed mint POST targets it directly.
    let effective_base_url = effective_base_from_public_key_url(resp.url().as_str())?;

    if !resp.status().is_success() {
        bail!("failed to fetch daemon public key: {}", resp.status());
    }

    let v: serde_json::Value = resp.json().await.context("parse public-key response")?;
    let principal_id = v
        .get("principal_id")
        .and_then(|f| f.as_str())
        .map(|s| s.to_string())
        .context("public-key response missing 'principal_id'")?;
    Ok(Discovered {
        principal_id,
        effective_base_url,
    })
}

/// Derive the effective base URL from the (post-redirect) `/public-key` URL:
/// drop query/fragment, strip a trailing slash and the `/public-key` probe
/// path, preserving any host path prefix and explicit port. Fails closed when
/// the resolved path is not `/public-key` (an unexpected redirect target),
/// rather than letting a signed request dispatch under a wrong base.
fn effective_base_from_public_key_url(public_key_url: &str) -> Result<String> {
    let without_fragment = public_key_url.split('#').next().unwrap_or(public_key_url);
    let path_part = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let trimmed = path_part.trim_end_matches('/');
    trimmed
        .strip_suffix("/public-key")
        .map(str::to_string)
        .with_context(|| {
            format!(
                "discovery resolved to an unexpected URL whose path is not \
                 /public-key: {public_key_url}"
            )
        })
}

fn project_from_cli_env() -> Option<PathBuf> {
    let value = std::env::var_os("RYEOS_PROJECT_PATH")?;
    if value.is_empty() || value == "." {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

// ── Browser launch ─────────────────────────────────────────────────────

fn open_browser(url: &str) -> Result<()> {
    let result = if cfg!(target_os = "linux") {
        Command::new("xdg-open").arg(url).spawn()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).spawn()
    } else {
        bail!("unsupported OS for browser launch");
    };

    result.context("failed to open browser")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_base_strips_public_key_and_preserves_prefix_port() {
        let base = effective_base_from_public_key_url;
        assert_eq!(
            base("https://node.example.com/public-key").unwrap(),
            "https://node.example.com"
        );
        assert_eq!(
            base("https://host/prefix/public-key").unwrap(),
            "https://host/prefix"
        );
        assert_eq!(
            base("http://127.0.0.1:7400/public-key").unwrap(),
            "http://127.0.0.1:7400"
        );
        assert_eq!(base("https://host/public-key/").unwrap(), "https://host");
        assert_eq!(base("https://host/public-key?x=1").unwrap(), "https://host");
    }

    #[test]
    fn effective_base_rejects_unexpected_target() {
        assert!(effective_base_from_public_key_url("https://host/login").is_err());
        assert!(effective_base_from_public_key_url("https://host/%70ublic-key").is_err());
    }

    #[test]
    fn canonicalize_path_no_query() {
        assert_eq!(canonicalize_path("/execute"), "/execute");
    }

    #[test]
    fn canonicalize_path_sorted_query() {
        assert_eq!(
            canonicalize_path("/threads/abc/events/stream?after=42&limit=10"),
            "/threads/abc/events/stream?after=42&limit=10"
        );
    }

    #[test]
    fn canonicalize_path_unsorted_query() {
        assert_eq!(
            canonicalize_path("/threads/abc?limit=10&after=42"),
            "/threads/abc?after=42&limit=10"
        );
    }

    #[test]
    fn mint_request_serializes_surface() {
        let req = MintRequest {
            surface_ref: "surface:ryeos/ryeos/base".to_string(),
            project_path: Some("/tmp/proj".to_string()),
            read_only: false,
            user_principal_id: None,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["surface_ref"], "surface:ryeos/ryeos/base");
        assert_eq!(json["project_path"], "/tmp/proj");
        assert_eq!(json["read_only"], false);
    }

    #[test]
    fn mint_request_omits_none_project_path() {
        let req = MintRequest {
            surface_ref: "surface:x/y/z".to_string(),
            project_path: None,
            read_only: true,
            user_principal_id: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("project_path"),
            "should skip_serializing_if None"
        );
        assert!(json.contains("read_only"));
    }

    #[test]
    fn mint_request_includes_user_principal_when_bound() {
        let req = MintRequest {
            surface_ref: "surface:x/y/z".to_string(),
            project_path: None,
            read_only: true,
            user_principal_id: Some(format!("fp:{}", "ab".repeat(32))),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["user_principal_id"], format!("fp:{}", "ab".repeat(32)));
    }

    #[test]
    fn mint_response_parses() {
        let raw = serde_json::json!({
            "token": "abc-123",
            "launch_url": "http://localhost:8080/custom/launch/abc-123",
            "session_id": "sess-456"
        });
        let resp: MintResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(
            resp.launch_url,
            "http://localhost:8080/custom/launch/abc-123"
        );
        assert_eq!(resp.session_id, "sess-456");
    }
}
