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
    plant_directive_with_secrets(user_space, rel_path, body_text, &[])
}

/// Plant a directive that declares `required_secrets` in its frontmatter.
///
/// The dispatcher reads only declared secrets out of the operator vault
/// (`required_secrets` plumbing in `dispatch.rs`). Tests that need the
/// vault path exercised must pass the list of secret env vars they
/// expect to flow through.
fn plant_directive_with_secrets(
    user_space: &Path,
    rel_path: &str,
    body_text: &str,
    required_secrets: &[&str],
) -> anyhow::Result<()> {
    let path = user_space.join(format!(".ai/directives/{rel_path}.md"));
    let dir_relative = Path::new(rel_path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path);
    std::fs::create_dir_all(path.parent().expect("directive parent dir"))?;
    let required_secrets_yaml = if required_secrets.is_empty() {
        String::new()
    } else {
        let mut s = String::from("required_secrets:\n");
        for k in required_secrets {
            s.push_str(&format!("  - {k}\n"));
        }
        s
    };
    let body = format!(
        r#"---
name: {stem}
category: "{dir_relative}"
description: "Provider secret-injection e2e fixture"
inputs:
  name:
    type: string
    required: true
model:
  tier: general
{required_secrets_yaml}---
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

// ── Vault-backed secret injection ─────────────────────────────────────
//
// The tests above set the secret on the daemon process directly via
// `cmd.env(...)`. Production operators don't do that — they put their
// API keys in the sealed-envelope store at
// `<state>/.ai/state/secrets/store.enc` and let the daemon's
// `SealedEnvelopeVault` decrypt them at request-build time. The vault
// populates `vault_bindings`, which
// `services::thread_lifecycle::spawn_item` merges into the spawned
// subprocess's `spec.env`. The subprocess then sees
// `std::env::var("FOO_API_KEY")` as if it had been exported in the
// daemon's own env.
//
// Because the daemon auto-generates the vault X25519 keypair at boot
// (in `bootstrap::init`), the test fixture pre-generates the same
// keypair in `pre_init` (which runs before daemon start) and writes
// the sealed store using its public key. The daemon then loads the
// existing private key on boot and decrypts the planted store.

/// Pre-generate the daemon's vault X25519 keypair and seal `secrets`
/// into `<state>/.ai/state/secrets/store.enc`.
///
/// `pre_init` runs before daemon start, so writing the secret key
/// here means the daemon's `bootstrap::init` will see the existing
/// key and skip generation, then `SealedEnvelopeVault::load` picks
/// up our pre-placed key and decrypts the sealed store we've planted.
fn plant_sealed_vault_secrets(
    state_path: &Path,
    secrets: &std::collections::HashMap<String, String>,
) -> anyhow::Result<()> {
    let secret_key_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("private_key.pem");
    let sk = lillux::vault::VaultSecretKey::generate();
    lillux::vault::write_secret_key(&secret_key_path, &sk)?;
    let pk = sk.public_key();
    // Mirror bootstrap.rs which also writes the public_key.pem alongside.
    let pub_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("public_key.pem");
    lillux::vault::write_public_key(&pub_path, &pk)?;

    let store_path = ryeosd::vault::default_sealed_store_path(state_path);
    ryeosd::vault::write_sealed_secrets(&store_path, &pk, secrets)?;
    Ok(())
}

async fn run_directive_with_vault_secret(
    env_var: &str,
    secret_value: &str,
    header_name: Option<&str>,
    prefix: Option<&str>,
) -> std::collections::HashMap<String, String> {
    let mock = MockProvider::start(vec![MockResponse::Text("ok".into())]).await;
    let mock_url = mock.base_url.clone();

    let env_var_owned = env_var.to_string();
    let secret_owned = secret_value.to_string();
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
        // Declare the env var as a required secret so the dispatcher
        // reads it out of the vault and injects it into the runtime
        // subprocess. Without this declaration, dispatch ignores the
        // vault entirely (post-step-7a scoping).
        plant_directive_with_secrets(
            user,
            "test/vault_secret",
            "Hello {{ name }}.",
            &[&env_var_owned],
        )?;
        // Crucial: secret comes from the sealed vault store, NOT
        // cmd.env(...). Pre-generate the daemon's vault keypair so we
        // can seal the store before daemon boot picks the key up.
        let mut secrets = std::collections::HashMap::new();
        secrets.insert(env_var_owned.clone(), secret_owned.clone());
        plant_sealed_vault_secrets(state_path, &secrets)?;
        let _ = user; // user_space unused for vault now (kept for trust + bundles).
        Ok(())
    };

    let h = DaemonHarness::start_with_pre_init(pre_init, move |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeos_directive_runtime=debug,ryeosd=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with vault-backed secret");

    let project = tempfile::tempdir().expect("project tempdir");
    let post_fut = h.post_execute(
        "directive:test/vault_secret",
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
        "mock provider received zero requests; vault → vault_bindings → spec.env path \
         is broken"
    );

    drop(project);
    drop(mock);
    captured.into_iter().next().expect("at least one captured request")
}

#[tokio::test(flavor = "multi_thread")]
async fn vault_secret_reaches_provider_with_default_bearer() {
    // End-to-end: secret only exists in the sealed vault store at
    // `<state>/.ai/state/secrets/store.enc`. Nothing on the daemon's
    // command line, nothing in `cmd.env()`. If the header arrives at
    // the mock provider with the expected value, the full pipe works:
    //   store.enc → SealedEnvelopeVault::read_all → dispatch.rs →
    //   ExecutionParams.vault_bindings → spawn_item spec.env →
    //   Command::env() → directive-runtime subprocess →
    //   std::env::var(provider.auth.env_var) → outbound auth header.
    let env_var = "RYE_TEST_VAULT_DEFAULT";
    let secret = "sk-vault-default-cafef00dbaadf00d";

    let headers = run_directive_with_vault_secret(env_var, secret, None, None).await;

    let actual = headers.get("authorization").unwrap_or_else(|| {
        panic!(
            "expected default Authorization header from vault-backed secret; \
             got headers: {headers:#?}"
        )
    });
    let expected = format!("Bearer {secret}");
    assert_eq!(
        actual, &expected,
        "vault → subprocess env → auth header mismatch — sealed vault store at \
         `<state>/.ai/state/secrets/store.enc` must reach the provider HTTP \
         request via the existing vault_bindings plumbing",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dotenv_overlay_supplies_declared_secret_to_provider() {
    // End-to-end: secret is provisioned via a project-local `.env`
    // file (no vault entry, no `cmd.env(...)`). This exercises the
    // step 7c overlay: dispatcher walks `[user_home, project_root]`
    // for `.env`, parses KEY=VALUE, layers it under the (empty)
    // vault, then projects to the directive's declared
    // `required_secrets` and threads the result through
    // `vault_bindings` → spec.env → directive-runtime subprocess →
    // outbound auth header.
    let env_var = "RYE_TEST_DOTENV_AUTH";
    let secret = "sk-dotenv-only-feedfacefeedface";

    let mock = MockProvider::start(vec![MockResponse::Text("ok".into())]).await;
    let mock_url = mock.base_url.clone();

    // We need to write the project `.env` under the project tempdir
    // chosen by the test, but the tempdir is created AFTER pre_init
    // runs. So: pre-init plants only signer + bundle + provider +
    // routing + directive (declaring the secret). Then we create
    // the project tempdir, drop the `.env` into it, and dispatch.
    let env_var_owned = env_var.to_string();
    let pre_init = move |state_path: &Path, user: &Path| -> anyhow::Result<()> {
        std::fs::create_dir_all(state_path)?;
        let sk = e2e_signing_key();
        write_trusted_signer(user, &sk.verifying_key())?;
        register_standard_bundle(state_path)?;
        plant_mock_provider_with_auth(user, &mock_url, &env_var_owned, None, None)?;
        plant_model_routing(user)?;
        plant_directive_with_secrets(
            user,
            "test/dotenv_secret",
            "Hello {{ name }}.",
            &[&env_var_owned],
        )?;
        // Pre-generate vault keypair so the daemon boots cleanly,
        // but DO NOT seal any secret — the .env overlay must be the
        // sole source of the declared secret.
        let secrets = std::collections::HashMap::new();
        plant_sealed_vault_secrets(state_path, &secrets)?;
        Ok(())
    };

    let h = DaemonHarness::start_with_pre_init(pre_init, |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeos_directive_runtime=debug,ryeosd=debug".into()
            }),
        );
    })
    .await
    .expect("start daemon with empty vault but .env-backed secret");

    // Plant the `.env` file inside the project root the dispatcher
    // will see. The harness sets HOME to user_space and the project
    // path is what we POST in; we use a fresh project tempdir.
    let project = tempfile::tempdir().expect("project tempdir");
    std::fs::write(
        project.path().join(".env"),
        format!("{env_var}={secret}\n"),
    )
    .expect("write project .env");

    let post_fut = h.post_execute(
        "directive:test/dotenv_secret",
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
        ".env → vault_bindings → spec.env path is broken; mock got 0 requests"
    );
    let headers = captured.into_iter().next().unwrap();
    let actual = headers.get("authorization").unwrap_or_else(|| {
        panic!("expected Authorization from .env-backed secret; got: {headers:#?}")
    });
    let expected = format!("Bearer {secret}");
    assert_eq!(
        actual, &expected,
        ".env → subprocess env → auth header mismatch — project `.env` must \
         supply declared `required_secrets` when the vault is empty",
    );

    drop(project);
    drop(mock);
}

/// Seal a poisoned plaintext (containing a key like `PATH` that would
/// normally be rejected) directly via `lillux::vault::seal`,
/// bypassing `write_sealed_secrets`'s pre-encryption blocklist check.
/// This simulates a corrupt or malicious sealed store on disk so the
/// daemon's `validate_decrypted_keys` post-decrypt check fires.
fn plant_poisoned_sealed_store(
    state_path: &Path,
    plaintext_toml: &str,
) -> anyhow::Result<()> {
    let secret_key_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("private_key.pem");
    let sk = lillux::vault::VaultSecretKey::generate();
    lillux::vault::write_secret_key(&secret_key_path, &sk)?;
    let pub_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("public_key.pem");
    lillux::vault::write_public_key(&pub_path, &sk.public_key())?;

    let envelope = lillux::vault::seal(&sk.public_key(), plaintext_toml.as_bytes())?;
    let envelope_toml = toml::to_string(&envelope)?;
    let store_path = ryeosd::vault::default_sealed_store_path(state_path);
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&store_path, envelope_toml.as_bytes())?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn vault_blocked_name_fails_request_loud() {
    // Operator-protection: a sealed vault store containing `PATH=...`
    // (e.g. corrupt / tampered / pre-policy) MUST NOT silently shadow
    // the OS-inherited PATH for spawned subprocesses. The daemon's
    // `validate_decrypted_keys` bails at read time with a typed error;
    // the dispatch path maps that to a 5xx so the operator notices
    // immediately rather than discovering the corruption later.
    let mock = MockProvider::start(vec![MockResponse::Text("ok".into())]).await;
    let mock_url = mock.base_url.clone();

    let pre_init = move |state_path: &Path, user: &Path| -> anyhow::Result<()> {
        std::fs::create_dir_all(state_path)?;
        let sk = e2e_signing_key();
        write_trusted_signer(user, &sk.verifying_key())?;
        register_standard_bundle(state_path)?;
        plant_mock_provider_with_auth(user, &mock_url, "RYE_TEST_VAULT_BLOCKED", None, None)?;
        plant_model_routing(user)?;
        // Directive declares a required secret so the vault is
        // actually read at dispatch time. Without a declared secret
        // the post-step-7a dispatcher skips the vault read entirely
        // and a poisoned PATH key in the store never trips.
        plant_directive_with_secrets(
            user,
            "test/vault_blocked",
            "noop",
            &["RYE_TEST_VAULT_BLOCKED"],
        )?;

        // Poisoned sealed store: PATH is on the blocked list, but we
        // bypass write-time validation by sealing the plaintext
        // directly. The declared secret is included so the test
        // isolates the blocked-name failure rather than a "missing
        // required" error.
        plant_poisoned_sealed_store(
            state_path,
            "RYE_TEST_VAULT_BLOCKED = \"ok\"\nPATH = \"/evil:/path\"\n",
        )?;
        Ok(())
    };

    let h = DaemonHarness::start_with_pre_init(pre_init, move |cmd| {
        cmd.env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,ryeos_directive_runtime=debug,ryeosd=debug".into()
            }),
        );
    })
    .await
    .expect("daemon starts even with poisoned vault — vault is read at request time, not boot");

    let project = tempfile::tempdir().expect("project tempdir");
    let post_fut = h.post_execute(
        "directive:test/vault_blocked",
        project.path().to_str().unwrap(),
        serde_json::json!({}),
    );
    let (status, body) = tokio::time::timeout(Duration::from_secs(30), post_fut)
        .await
        .expect("/execute timed out")
        .expect("/execute send failed");

    assert!(
        status.is_server_error(),
        "poisoned vault must fail the request (5xx); got status={status} body={body:#}"
    );
    let body_str = body.to_string();
    assert!(
        body_str.contains("vault") && body_str.contains("PATH") && body_str.contains("blocked"),
        "5xx response should name the vault, the offending key, and the blocked-list \
         policy; got: {body_str}"
    );

    drop(project);
    drop(mock);
}
