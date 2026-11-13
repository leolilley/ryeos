//! `web` — mints a RyeOS RyeOs launch token and opens the browser.
//!
//! This binary is the `cli_exec` target for `client:ryeos/web`. It:
//! 1. Parses an explicit Config or Surface selection and project.
//! 2. Resolves the daemon URL from `daemon.json` or `RYEOSD_URL`.
//! 3. Discovers the daemon's public key for request signing.
//! 4. Calls `POST /ui/api/launch/mint` with the launch context.
//! 5. Opens the browser at the daemon-returned `launch_url`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use base64::Engine;
use clap::Parser;
use ryeos_client_transport::discovery::discover_audience;
use ryeos_client_transport::signing::operator_signing_key_path;
use serde::Deserialize;

#[derive(Parser)]
#[command(name = "web", about = "Launch RyeOS in the browser")]
struct Cli {
    /// Signed shared presentation Config. Command descriptors select it.
    #[arg(
        long = "presentation",
        required_unless_present = "surface",
        conflicts_with = "surface"
    )]
    presentation: Option<String>,

    /// Signed Surface root to open directly, without a shared Config.
    #[arg(
        long = "surface",
        required_unless_present = "presentation",
        conflicts_with = "presentation"
    )]
    surface: Option<String>,

    /// Project path
    #[arg(long = "project")]
    project: Option<PathBuf>,

    /// Print the minted short-lived activation URL to stdout.
    #[arg(long = "print-url")]
    print_url: bool,

    /// Mint a launch URL but do not open a browser.
    #[arg(long = "no-open")]
    no_open: bool,

    /// Bind RyeOs principal storage to this launcher's signing-key principal.
    #[arg(long = "hosted-principal")]
    hosted_principal: bool,

    /// Capability pattern to narrow the session to (repeatable). The session
    /// holds the intersection of your scopes, the node's browser-session
    /// ceiling and these patterns; it can never hold more than the ceiling.
    #[arg(long = "scope")]
    scopes: Vec<String>,
}

/// The one canonical root the launch selects: a shared presentation Config or a
/// Surface. The daemon chooses the composition by the ref's kind.
fn selected_root_ref(presentation: Option<String>, surface: Option<String>) -> Result<String> {
    match (presentation, surface) {
        (Some(config_ref), None) => Ok(config_ref),
        (None, Some(surface_ref)) => Ok(surface_ref),
        _ => bail!("select exactly one of --presentation or --surface"),
    }
}

/// Response from `ui.launch.mint`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MintResponse {
    ui_binding_contract_revision: String,
    launch_url: String,
    session_id: String,
    token: String,
    /// The authority the session holds: the minter's scopes within the node's
    /// browser-session ceiling and any requested narrowing.
    granted_caps: Vec<String>,
    /// Exact public coordinate of the surface binding admitted for this
    /// launch. The browser obtains the same attachment through session open;
    /// retaining it in this strict response type keeps the launcher and daemon
    /// on one current mint contract without treating the coordinate as a
    /// client-side grant.
    attachment: ryeos_client_base::ui::UiBindingAttachment,
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

    // Discovery and the signed mint use one normalized base and both refuse
    // redirects, so the configured origin cannot move between them.
    let discovered = discover_audience(&daemon_url).await?;

    let project_path = cli.project.or_else(project_from_cli_env);

    let project_path = project_path
        .map(|path| {
            path.to_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("project path is not valid UTF-8"))
        })
        .transpose()?;
    let root_ref = selected_root_ref(cli.presentation, cli.surface)?;
    let user_principal_id = cli
        .hosted_principal
        .then(|| format!("fp:{}", signer.fingerprint));
    let mut mint_body = ryeos_client_base::presentation::launch_mint_body(
        &root_ref,
        project_path.as_deref(),
        user_principal_id.as_deref(),
    );
    if !cli.scopes.is_empty() {
        mint_body["scopes"] = cli.scopes.clone().into();
    }

    let body = serde_json::to_vec(&mint_body).context("serialize mint request")?;

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
        .post(format!("{}{}", discovered.base_url, MINT_PATH))
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
    if mint_resp.ui_binding_contract_revision != ryeos_client_base::UI_BINDING_CONTRACT_REVISION {
        anyhow::bail!(
            "UI binding contract mismatch: daemon advertised '{}', launcher requires '{}'",
            mint_resp.ui_binding_contract_revision,
            ryeos_client_base::UI_BINDING_CONTRACT_REVISION
        );
    }
    mint_resp
        .attachment
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid mint binding attachment: {error}"))?;
    if mint_resp.session_id.is_empty() || mint_resp.token.is_empty() {
        anyhow::bail!("mint response carries no session credentials");
    }

    ryeos_client_base::diagnostic::info(&format!(
        "session authority · {}",
        mint_resp.granted_caps.join(", ")
    ));

    if cli.print_url || cli.no_open {
        println!("{}", mint_resp.launch_url);
    }

    if cli.no_open {
        return Ok(());
    }

    // Open the browser at the daemon-returned launch URL.
    ryeos_client_base::diagnostic::info(&format!("opening browser · {}", mint_resp.launch_url));
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
        let operator_key = operator_signing_key_path(app_root);

        if operator_key.exists() {
            return Self::load_from(operator_key);
        }

        bail!("no signing key found at {}", operator_key.display())
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

        let nonce_bytes = rand::Rng::r#gen::<[u8; 16]>(&mut rand::thread_rng());
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

fn project_from_cli_env() -> Option<PathBuf> {
    let value = std::env::var_os("RYEOS_CLIENT_PROJECT_PATH")?;
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
    fn launcher_selects_surface_root_without_inventing_a_config() {
        let cli = Cli::try_parse_from(["web", "--surface", "surface:ryeos/ui/assistant-observe"])
            .unwrap();
        assert!(cli.presentation.is_none());
        assert_eq!(
            selected_root_ref(cli.presentation, cli.surface).unwrap(),
            "surface:ryeos/ui/assistant-observe"
        );
    }

    #[test]
    fn launcher_requires_exactly_one_composition_root() {
        assert!(Cli::try_parse_from(["web"]).is_err());
        assert!(
            Cli::try_parse_from([
                "web",
                "--presentation",
                "config:ryeos/ui/presentation",
                "--surface",
                "surface:ryeos/ui/assistant-observe",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["web", "--presentation", "config:ryeos/ui/presentation",]).is_ok()
        );
    }

    #[test]
    fn mint_response_parses() {
        let raw = serde_json::json!({
            "ui_binding_contract_revision": ryeos_client_base::UI_BINDING_CONTRACT_REVISION,
            "token": "abc-123",
            "launch_url": "http://localhost:8080/custom/launch/abc-123",
            "session_id": "sess-456",
            "granted_caps": ["ryeos.execute.service.ui/*"],
            "attachment": {
                "binding_attachment_id": "attachment-789",
                "binding_generation": 1,
                "binding_digest": "digest-789",
                "root_ref": "surface:ryeos/ui/assistant-observe",
                "root_generation": "surface-generation-789",
                "presentation": {"definition": {"kind": "surface", "surface": {"name": "assistant-observe"}}, "views": {}},
                "project_path": null,
                "subject": {"kind": "projectless"},
                "posture": "observation_only",
                "binding_request_bounds": {
                    "max_request_bytes": 4096,
                    "max_input_bytes": 1024,
                    "max_seat_append_bytes": 65536
                }
            }
        });
        let resp: MintResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(
            resp.launch_url,
            "http://localhost:8080/custom/launch/abc-123"
        );
        assert_eq!(resp.session_id, "sess-456");
        assert_eq!(resp.granted_caps, ["ryeos.execute.service.ui/*"]);
        assert_eq!(resp.attachment.binding_attachment_id, "attachment-789");
        assert!(resp.attachment.validate().is_ok());
    }

    #[test]
    fn launcher_rejects_authority_posture_flags() {
        for posture_flag in ["--read-only", "--allow-intents", "--mode"] {
            let mut args = vec![
                "web",
                "--presentation",
                "config:ryeos/ui/presentation",
                posture_flag,
            ];
            if posture_flag == "--mode" {
                args.push("operate");
            }
            assert!(
                Cli::try_parse_from(args).is_err(),
                "launcher posture flag {posture_flag} must fail closed"
            );
        }
    }
}
