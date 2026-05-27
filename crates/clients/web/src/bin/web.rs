//! `web` — mints a launch token and opens the browser.
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
    /// Surface ref to open (e.g. surface:ryeos/cockpit/base)
    #[arg(long = "surface")]
    surface: Option<String>,

    /// Project path
    #[arg(long = "project")]
    project: Option<PathBuf>,

    /// Read-only mode
    #[arg(long = "read-only")]
    read_only: bool,
}

/// Request body for `ui.launch.mint`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
struct MintRequest {
    surface_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_path: Option<String>,
    read_only: bool,
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
    let system_space_dir = discover_system_space_dir()?;
    let signer = WebSigner::resolve(&system_space_dir)?;

    // Discover daemon audience (public key fingerprint).
    let audience = discover_audience(&daemon_url).await?;

    // Build the mint request with parsed args.
    let surface_ref = cli
        .surface
        .unwrap_or_else(|| "surface:ryeos/cockpit/base".to_string());

    let mint_req = MintRequest {
        surface_ref,
        project_path: cli.project.map(|p| p.to_string_lossy().to_string()),
        read_only: cli.read_only,
    };

    let body = serde_json::to_vec(&mint_req).context("serialize mint request")?;

    // Sign the request.
    let sign_headers = signer.sign("POST", "/ui/api/launch/mint", &body, &audience)?;

    // Send the mint request.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/ui/api/launch/mint", daemon_url.trim_end_matches('/')))
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

    let mint_resp: MintResponse = resp
        .json()
        .await
        .context("parse mint response")?;

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
    let system_space_dir = discover_system_space_dir()?;
    let daemon_json = system_space_dir.join("daemon.json");
    if daemon_json.exists() {
        let raw = std::fs::read_to_string(&daemon_json).context("read daemon.json")?;
        let v: serde_json::Value = serde_json::from_str(&raw).context("parse daemon.json")?;
        if let Some(bind) = v.get("bind").and_then(|b| b.as_str()) {
            return Ok(format!("http://{bind}"));
        }
    }

    bail!("cannot resolve daemon URL: set RYEOSD_URL or ensure daemon is running")
}

fn discover_system_space_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("RYEOS_SYSTEM_SPACE") {
        return Ok(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("RYEOS_SYSTEM_SPACE_DIR") {
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
    fn resolve(system_space_dir: &std::path::Path) -> Result<Self> {
        // Same resolution as CLI: env override → user key → system key.
        if let Ok(p) = std::env::var("RYEOS_CLI_KEY_PATH") {
            return Self::load_from(PathBuf::from(p));
        }

        let user_root = ryeos_engine::roots::user_root().context("discover user root for signing key")?;
        let user_key = user_root
            .join(ryeos_engine::AI_DIR)
            .join("config")
            .join("keys")
            .join("signing")
            .join("private_key.pem");

        if user_key.exists() {
            return Self::load_from(user_key);
        }

        // Fallback: try the system space signing key (daemon key).
        let system_key = system_space_dir
            .join(ryeos_engine::AI_DIR)
            .join("config")
            .join("keys")
            .join("signing")
            .join("private_key.pem");

        if system_key.exists() {
            return Self::load_from(system_key);
        }

        bail!(
            "no signing key found; set RYEOS_CLI_KEY_PATH or ensure a signing key exists \
             at <user_root>/{}/config/keys/signing/private_key.pem",
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

async fn discover_audience(daemon_url: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/public-key", daemon_url.trim_end_matches('/')))
        .send()
        .await
        .context("fetch daemon public key")?;

    if !resp.status().is_success() {
        bail!("failed to fetch daemon public key: {}", resp.status());
    }

    let v: serde_json::Value = resp.json().await.context("parse public-key response")?;
    v.get("principal_id")
        .and_then(|f| f.as_str())
        .map(|s| s.to_string())
        .context("public-key response missing 'principal_id'")
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
            surface_ref: "surface:ryeos/cockpit/base".to_string(),
            project_path: Some("/tmp/proj".to_string()),
            read_only: false,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["surface_ref"], "surface:ryeos/cockpit/base");
        assert_eq!(json["project_path"], "/tmp/proj");
        assert_eq!(json["read_only"], false);
    }

    #[test]
    fn mint_request_omits_none_project_path() {
        let req = MintRequest {
            surface_ref: "surface:x/y/z".to_string(),
            project_path: None,
            read_only: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("project_path"), "should skip_serializing_if None");
        assert!(json.contains("read_only"));
    }

    #[test]
    fn mint_response_parses() {
        let raw = serde_json::json!({
            "token": "abc-123",
            "launch_url": "http://localhost:8080/custom/launch/abc-123",
            "session_id": "sess-456"
        });
        let resp: MintResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.launch_url, "http://localhost:8080/custom/launch/abc-123");
        assert_eq!(resp.session_id, "sess-456");
    }
}
