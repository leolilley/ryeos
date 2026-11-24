//! A client that holds its own key under a narrowed grant (the MCP server's
//! situation) is refused by the daemon, by capability, for every service its
//! grant does not name, and served for the ones it does. The refusal comes from
//! the key and its grant alone: nothing here looks at what is calling.

use ryeosd::test_support as common;

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use lillux::crypto::{Signer as _, SigningKey};
use ryeos_node_runtime::identity::{AuthorizedBy, GrantExpiry, GrantTerms};

const READ_CAP: &str = "ryeos.execute.service.threads/list";

fn fingerprint(key: &SigningKey) -> String {
    lillux::signature::compute_fingerprint(&key.verifying_key())
}

fn public_key_text(key: &SigningKey) -> String {
    format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
    )
}

/// One request signed the way every RyeOS client signs it.
fn signed_post(
    harness: &DaemonHarness,
    audience_key: &SigningKey,
    signer: &SigningKey,
    path: &str,
    body: serde_json::Value,
) -> reqwest::RequestBuilder {
    let body = serde_json::to_vec(&body).unwrap();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let nonce = format!("{:016x}", rand::random::<u64>());
    let audience = format!("fp:{}", fingerprint(audience_key));
    let string_to_sign = format!(
        "ryeos-request-v1\nPOST\n{}\n{}\n{}\n{}\n{}",
        path,
        lillux::cas::sha256_hex(&body),
        timestamp,
        nonce,
        audience,
    );
    let content_hash = lillux::cas::sha256_hex(string_to_sign.as_bytes());
    let signature: lillux::crypto::Signature = signer.sign(content_hash.as_bytes());
    reqwest::Client::new()
        .post(format!("http://{}{}", harness.bind, path))
        .header("content-type", "application/json")
        .header("x-ryeos-key-id", format!("fp:{}", fingerprint(signer)))
        .header("x-ryeos-timestamp", timestamp)
        .header("x-ryeos-nonce", nonce)
        .header(
            "x-ryeos-signature",
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        )
        .body(body)
}

fn plant_grant(
    state_path: &std::path::Path,
    fixture: &FastFixture,
    subject: &SigningKey,
    scopes: &[&str],
) -> anyhow::Result<()> {
    let granter = format!("fp:{}", fingerprint(&fixture.user));
    ryeos_node_runtime::identity::create_authorized_key_toml(
        &state_path.join(".ai/node/auth/authorized_keys"),
        &fingerprint(subject),
        &public_key_text(subject)["ed25519:".len()..],
        &scopes.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        "mcp-key-fixture",
        &granter,
        "2026-01-01T00:00:00Z",
        &GrantTerms {
            expires_at: GrantExpiry::At(4_102_444_800),
            authorized_by: AuthorizedBy::principal_request(&granter, None),
        },
        &fixture.node,
    )?;
    Ok(())
}

async fn execute(
    harness: &DaemonHarness,
    fixture: &FastFixture,
    signer: &SigningKey,
    service: &str,
    parameters: serde_json::Value,
) -> (reqwest::StatusCode, serde_json::Value) {
    let body = serde_json::json!({
        "item_ref": service,
        "ref_bindings": {},
        "project_path": null,
        "parameters": parameters,
        "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    });
    let response = signed_post(harness, &fixture.node, signer, "/execute", body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::json!({ "raw": text })),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_with_a_narrowed_grant_is_refused_what_the_grant_does_not_name() {
    let client = SigningKey::from_bytes(&[91u8; 32]);
    let stranger = SigningKey::from_bytes(&[92u8; 32]);
    let (harness, fixture) = DaemonHarness::start_fast_with(
        |state_path, _user, fixture| {
            register_workflow_bundles(state_path, fixture)?;
            plant_grant(state_path, fixture, &client, &[READ_CAP])
        },
        |_| {},
    )
    .await
    .expect("daemon starts");

    // What the grant names works.
    let (status, json) = execute(
        &harness,
        &fixture,
        &client,
        "service:threads/list",
        serde_json::json!({ "limit": 1 }),
    )
    .await;
    assert!(status.is_success(), "{status} {json}");

    // What it does not name is refused by capability: granting authority,
    // and recording an endorsement.
    let (status, json) = execute(
        &harness,
        &fixture,
        &client,
        "service:identity/authorize-key",
        serde_json::json!({
            "public_key": public_key_text(&stranger),
            "label": "self-promotion",
            "scopes": [READ_CAP],
        }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{json}");
    let (status, json) = execute(
        &harness,
        &fixture,
        &client,
        "service:endorsements/record",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{json}");

    // The same refusals hold for any holder of the key, and the operator, who
    // holds the capabilities, is served.
    let (status, json) = execute(
        &harness,
        &fixture,
        &fixture.user,
        "service:endorsements/record",
        serde_json::json!({}),
    )
    .await;
    assert!(status.is_success(), "{status} {json}");
}
