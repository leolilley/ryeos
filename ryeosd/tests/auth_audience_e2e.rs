//! Cross-machine audience-binding e2e test.
//!
//! Proves the v0.3.0 bug: a CLI that signs requests with its own
//! fingerprint as audience is rejected by a daemon whose principal_id
//! differs. After Phase 3 (CLI audience discovery) lands, a sibling
//! test will prove the fix: signing with the daemon's principal_id
//! succeeds.

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use common::fast_fixture::{write_authorized_key_signed_by, FastFixture};
use common::DaemonHarness;
use lillux::crypto::{Signer as _, SigningKey};

/// Build ryeos-signed auth headers with a specific audience.
fn build_signed_headers(
    sk: &SigningKey,
    method: &str,
    path: &str,
    body: &[u8],
    audience: &str,
) -> Vec<(String, String)> {
    let fp = lillux::signature::compute_fingerprint(&sk.verifying_key());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let nonce = format!("{:016x}", rand::random::<u64>());

    let body_hash = lillux::cas::sha256_hex(body);
    let string_to_sign = format!(
        "ryeos-request-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        path,
        body_hash,
        timestamp,
        nonce,
        audience,
    );
    let content_hash = lillux::cas::sha256_hex(string_to_sign.as_bytes());
    let sig: lillux::crypto::Signature = sk.sign(content_hash.as_bytes());
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

    vec![
        ("x-ryeos-key-id".into(), format!("fp:{fp}")),
        ("x-ryeos-timestamp".into(), timestamp),
        ("x-ryeos-nonce".into(), nonce),
        ("x-ryeos-signature".into(), sig_b64),
    ]
}

/// The daemon's principal_id is derived from its node identity key.
/// The CLI's own fingerprint is different from the daemon's principal_id
/// when they run on different machines (different keys).
///
/// v0.3.0 bug: CLI signs with `audience = caller_fingerprint`.
/// Daemon verifies with `audience = daemon_principal_id`.
/// Mismatch → 401.
#[tokio::test]
async fn rejects_request_signed_with_caller_audience_when_daemon_identity_differs() {
    // Generate a distinct CLIENT key (not node, not user, not publisher).
    let client_key = SigningKey::from_bytes(&[99u8; 32]);
    let client_fp = lillux::signature::compute_fingerprint(&client_key.verifying_key());

    let (harness, fixture) = DaemonHarness::start_fast_with(
        |state_path, _user_space, fixture| {
            // Authorize the client key (signed by the node key, as required).
            write_authorized_key_signed_by(state_path, &client_key, &fixture.node)?;
            Ok(())
        },
        |cmd| {
            // Enable auth enforcement.
            cmd.arg("--require-auth");
        },
    )
    .await
    .expect("daemon should start");

    // Compute the daemon's principal_id (from node key).
    let daemon_principal_id = format!(
        "fp:{}",
        lillux::signature::compute_fingerprint(&fixture.node.verifying_key())
    );

    // Sanity: client fingerprint != daemon principal_id.
    assert_ne!(
        client_fp, daemon_principal_id,
        "test requires distinct client/daemon identities"
    );

    // Build a signed request using the v0.3.0 CLI signing logic:
    // audience = caller's own fingerprint (the bug).
    let body = serde_json::json!({
        "item_ref": "directive:nonexistent/test",
        "project_path": "/tmp",
        "parameters": {},
    });
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let headers = build_signed_headers(
        &client_key,
        "POST",
        "/execute",
        &body_bytes,
        &client_fp, // ← BUG: audience = caller's own fingerprint
    );

    let mut req = reqwest::Client::new()
        .post(format!("http://{}/execute", harness.bind))
        .header("content-type", "application/json")
        .body(body_bytes);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.expect("request should send");

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "request signed with caller-fingerprint audience must be rejected \
         when daemon identity differs — got {}: {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );
}

/// After the CLI fix, signing with the daemon's principal_id as audience
/// should succeed (or at least pass auth — the execute may still fail
/// for other reasons like missing directive, but auth passes).
///
/// This test will pass after Phase 1 (data-driven routes) and Phase 3
/// (CLI audience discovery) land. It proves the cross-machine auth
/// path works end-to-end.
#[tokio::test]
async fn accepts_request_signed_with_daemon_audience() {
    // Generate a distinct CLIENT key.
    let client_key = SigningKey::from_bytes(&[99u8; 32]);

    let (harness, fixture) = DaemonHarness::start_fast_with(
        |state_path, _user_space, fixture| {
            write_authorized_key_signed_by(state_path, &client_key, &fixture.node)?;
            Ok(())
        },
        |cmd| {
            cmd.arg("--require-auth");
        },
    )
    .await
    .expect("daemon should start");

    // Daemon's principal_id (from node key).
    let daemon_principal_id = format!(
        "fp:{}",
        lillux::signature::compute_fingerprint(&fixture.node.verifying_key())
    );

    // Sign with the CORRECT audience: daemon's principal_id.
    let body = serde_json::json!({
        "item_ref": "directive:nonexistent/test",
        "project_path": "/tmp",
        "parameters": {},
    });
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let headers = build_signed_headers(
        &client_key,
        "POST",
        "/execute",
        &body_bytes,
        &daemon_principal_id, // ← CORRECT: audience = daemon's principal_id
    );

    let mut req = reqwest::Client::new()
        .post(format!("http://{}/execute", harness.bind))
        .header("content-type", "application/json")
        .body(body_bytes);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.expect("request should send");

    // Auth should pass. The directive doesn't exist, so we expect 400 or 500,
    // but NOT 401.
    assert_ne!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "request signed with daemon principal_id audience must pass auth \
         — got 401: {}",
        resp.text().await.unwrap_or_default()
    );
}
