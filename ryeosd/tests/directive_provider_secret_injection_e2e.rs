//! End-to-end: provider config loading + env-var secret injection.
//!
//! Exercises the full chain that turns a YAML model-provider config into
//! a live HTTP request with the operator's secret in the auth header:
//!
//! 1. Daemon loads `.ai/config/rye-runtime/model_providers/<id>.yaml`
//!    via the verified-loader (signature + trust-class checked at boot).
//! 2. Directive declares `model.tier: general`; `model_routing.yaml`
//!    maps that tier to provider `mock`.
//! 3. `ryeos-directive-runtime/src/bootstrap.rs::resolve_provider`
//!    picks the YAML up at spawn time and serialises it into the
//!    runtime's `BootstrapConfig`.
//! 4. `provider_adapter/http.rs::call_provider` reads
//!    `auth.env_var`, calls `std::env::var(...)`, and adds
//!    `<header_name>: <prefix><secret>` to the outbound request.
//! 5. The daemon's runtime spawn (`execution/launch.rs::spawn_runtime`)
//!    inherits the parent process's environment, so an env var set on
//!    the daemon is visible to the runtime subprocess.
//!
//! These tests prove that chain end-to-end by capturing the headers
//! the mock provider receives and asserting both the value AND that
//! the daemon → runtime env-inheritance path works.
//!
//! Two scenarios:
//! - `secret_injection_with_custom_header_and_prefix` —
//!   `auth: { env_var, header_name, prefix }` all set; the header
//!   name and prefix are non-default to prove the YAML wins over the
//!   adapter's `Authorization`/`Bearer ` defaults.
//! - `secret_injection_with_default_authorization_bearer` —
//!   `auth: { env_var: ... }` only; defaults take effect →
//!   `Authorization: Bearer <secret>`.

mod common;

use std::path::Path;
use std::time::Duration;

use common::mock_provider::{MockProvider, MockResponse};
use common::DaemonHarness;
use lillux::crypto::SigningKey;

fn e2e_signing_key() -> SigningKey {
    SigningKey::from_bytes(&[0x77u8; 32])
}

fn write_trusted_signer(
    user_space: &Path,
    vk: &lillux::crypto::VerifyingKey,
) -> anyhow::Result<()> {
    use base64::engine::Engine as _;
    let fp = lillux::signature::compute_fingerprint(vk);
    let trust_dir = user_space.join(".ai/config/keys/trusted");
    std::fs::create_dir_all(&trust_dir)?;
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
    let toml = format!(
        r#"version = "1.0.0"
category = "keys/trusted"
fingerprint = "{fp}"
owner = "self"
attestation = ""

[public_key]
pem = "ed25519:{key_b64}"
"#
    );
    std::fs::write(trust_dir.join(format!("{fp}.toml")), toml)?;
    Ok(())
}

fn register_standard_bundle(state_path: &Path) -> anyhow::Result<()> {
    let standard = common::workspace_root().join("ryeos-bundles/standard");
    if !standard.is_dir() {
        anyhow::bail!(
            "ryeos-bundles/standard does not exist at {}",
            standard.display()
        );
    }
    let abs = standard.canonicalize()?;
    let dir = state_path.join(".ai/node/bundles");
    std::fs::create_dir_all(&dir)?;
    let body = format!("section: bundles\npath: {}\n", abs.display());
    let signed = lillux::signature::sign_content(&body, &e2e_signing_key(), "#", None);
    std::fs::write(dir.join("standard.yaml"), signed)?;
    Ok(())
}

/// Plant a mock model_provider config with a fully-specified `auth`
/// block. `env_var` names the env var the runtime will read; the
/// daemon process must have it set before spawning the runtime.
fn plant_mock_provider_with_auth(
    user_space: &Path,
    mock_base_url: &str,
    env_var: &str,
    header_name: Option<&str>,
    prefix: Option<&str>,
) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/rye-runtime/model_providers");
    std::fs::create_dir_all(&dir)?;
    let mut auth_lines = format!("  env_var: \"{env_var}\"\n");
    if let Some(h) = header_name {
        auth_lines.push_str(&format!("  header_name: \"{h}\"\n"));
    }
    if let Some(p) = prefix {
        auth_lines.push_str(&format!("  prefix: \"{p}\"\n"));
    }
    let body = format!(
        r#"base_url: "{mock_base_url}"
auth:
{auth_lines}headers: {{}}
pricing:
  input_per_million: 0.0
  output_per_million: 0.0
"#
    );
    let signed = lillux::signature::sign_content(&body, &e2e_signing_key(), "#", None);
    std::fs::write(dir.join("mock.yaml"), signed)?;
    Ok(())
}

fn plant_model_routing(user_space: &Path) -> anyhow::Result<()> {
    let dir = user_space.join(".ai/config/rye-runtime");
    std::fs::create_dir_all(&dir)?;
    let body = r#"tiers:
  general:
    provider: mock
    model: mock-model
    context_window: 200000
"#;
    let signed = lillux::signature::sign_content(body, &e2e_signing_key(), "#", None);
    std::fs::write(dir.join("model_routing.yaml"), signed)?;
    Ok(())
}

fn plant_directive(user_space: &Path, rel_path: &str, body_text: &str) -> anyhow::Result<()> {
    let path = user_space.join(format!(".ai/directives/{rel_path}.md"));
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
    let body = format!(
        r#"---
__category__: "{rel_path}"
__directive_description__: "Provider secret-injection e2e fixture"
inputs:
  name:
    type: string
    required: true
model:
  tier: general
---
{body_text}
"#
    );
    let signed = lillux::signature::sign_content(&body, &e2e_signing_key(), "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

/// Run a directive end-to-end with a configured env-var secret and
/// return the headers the mock provider observed on its first
/// `/chat/completions` request. The daemon process has `env_var` set
/// to `secret_value`, so the runtime subprocess inherits it via
/// `lillux::set_envs` (parent env is NOT cleared).
async fn run_directive_and_capture_first_request_headers(
    env_var: &str,
    secret_value: &str,
    header_name: Option<&str>,
    prefix: Option<&str>,
) -> std::collections::HashMap<String, String> {
    let mock = MockProvider::start(vec![MockResponse::Text("ok".into())]).await;
    let mock_url = mock.base_url.clone();

    let env_var_owned = env_var.to_string();
    let header_name_owned = header_name.map(|s| s.to_string());
    let prefix_owned = prefix.map(|s| s.to_string());

    let pre_init = move |state_path: &Path, user: &Path| -> anyhow::Result<()> {
        std::fs::create_dir_all(state_path)?;
        let sk = e2e_signing_key();
        write_trusted_signer(user, &sk.verifying_key())?;
        register_standard_bundle(state_path)?;
        plant_mock_provider_with_auth(
            user,
            &mock_url,
            &env_var_owned,
            header_name_owned.as_deref(),
            prefix_owned.as_deref(),
        )?;
        plant_model_routing(user)?;
        plant_directive(user, "test/secret_injection", "Say hello to {{ name }}.")?;
        Ok(())
    };

    let env_var_for_daemon = env_var.to_string();
    let secret_for_daemon = secret_value.to_string();
    let h = DaemonHarness::start_with_pre_init(pre_init, move |cmd| {
        // The daemon spawns the directive-runtime via
        // `lillux::SubprocessRequest`, whose `set_envs` *adds* to the
        // parent's environment without clearing. So setting the env
        // var on the daemon process makes it visible to the runtime.
        cmd.env(&env_var_for_daemon, &secret_for_daemon);
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeos_directive_runtime=debug,ryeosd=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with mock provider + auth env_var");

    let project = tempfile::tempdir().expect("project tempdir");
    let post_fut = h.post_execute(
        "directive:test/secret_injection",
        project.path().to_str().unwrap(),
        serde_json::json!({"name": "World"}),
    );
    let (status, body) = tokio::time::timeout(Duration::from_secs(30), post_fut)
        .await
        .expect("/execute timed out")
        .expect("/execute send failed");
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "/execute should succeed; body={body:#}"
    );

    let captured = mock.captured_headers().await;
    assert!(
        !captured.is_empty(),
        "mock provider received zero requests; daemon→runtime spawn or directive flow is broken"
    );

    drop(project);
    drop(mock);
    captured.into_iter().next().expect("at least one captured request")
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_injection_with_custom_header_and_prefix() {
    let env_var = "RYE_TEST_PROVIDER_SECRET_CUSTOM";
    let secret = "sk-test-custom-9f8e7d6c5b4a3210";
    let header_name = "X-Provider-Auth";
    let prefix = "Token ";

    let headers = run_directive_and_capture_first_request_headers(
        env_var,
        secret,
        Some(header_name),
        Some(prefix),
    )
    .await;

    let lower = header_name.to_ascii_lowercase();
    let actual = headers.get(&lower).unwrap_or_else(|| {
        panic!(
            "expected captured request to carry header `{header_name}`; \
             got headers: {headers:#?}"
        )
    });
    let expected = format!("{prefix}{secret}");
    assert_eq!(
        actual, &expected,
        "auth header value mismatch — daemon-side env_var injection should produce \
         `{header_name}: {expected}`, got `{actual}`",
    );

    // Defense-in-depth: the secret must NOT have leaked into any
    // OTHER header (e.g. via a stale Authorization fallback). The
    // YAML overrode the defaults, so only the custom header should
    // contain the secret.
    for (name, value) in &headers {
        if name == &lower {
            continue;
        }
        assert!(
            !value.contains(secret),
            "secret value leaked into header `{name}: {value}` — only `{header_name}` should carry it"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_injection_with_default_authorization_bearer() {
    let env_var = "RYE_TEST_PROVIDER_SECRET_DEFAULT";
    let secret = "sk-test-default-deadbeefcafebabe";

    // Omit header_name + prefix → adapter defaults to
    // `Authorization: Bearer <secret>`.
    let headers =
        run_directive_and_capture_first_request_headers(env_var, secret, None, None).await;

    let actual = headers.get("authorization").unwrap_or_else(|| {
        panic!(
            "expected default Authorization header; got headers: {headers:#?}"
        )
    });
    let expected = format!("Bearer {secret}");
    assert_eq!(
        actual, &expected,
        "default Authorization header value mismatch — `auth.env_var` only (no \
         header_name/prefix overrides) should produce `Authorization: Bearer <secret>`",
    );
}
