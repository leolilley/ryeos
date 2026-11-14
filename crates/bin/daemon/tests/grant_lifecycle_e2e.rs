//! Grant lifecycle e2e: every grant ends, ending is judged only for the key
//! holder, a lifetime above the node's signed class maximum is refused, only
//! the granter or the operator renews, and a grant of a retired class keeps the
//! node from starting with the cure named. A revoked grant leaves a tombstone,
//! cancels the live threads running under it, and stays dead if its file is
//! put back.

use ryeosd::test_support as common;

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_workflow_bundles};
use lillux::crypto::{Signer as _, SigningKey};
use ryeos_node_runtime::identity::{AuthorizedBy, GrantExpiry, GrantTerms};

const AUTHORIZE_KEY_CAP: &str = "ryeos.execute.service.identity/authorize-key";
const RENEW_KEY_CAP: &str = "ryeos.execute.service.identity/renew-key";
const LIST_CAP: &str = "ryeos.execute.service.vault/list";
const LOCAL_CLIENT_MAX_SECS: u64 = 365 * 24 * 60 * 60;

fn fingerprint(key: &SigningKey) -> String {
    lillux::signature::compute_fingerprint(&key.verifying_key())
}

fn public_key_text(key: &SigningKey) -> String {
    format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
    )
}

/// Sign one request the way every RyeOS client does, naming `key_id`'s
/// fingerprint and signing with `signer`.
fn signed_post(
    harness: &DaemonHarness,
    audience_key: &SigningKey,
    key_id: &SigningKey,
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
        .header("x-ryeos-key-id", format!("fp:{}", fingerprint(key_id)))
        .header("x-ryeos-timestamp", timestamp)
        .header("x-ryeos-nonce", nonce)
        .header(
            "x-ryeos-signature",
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        )
        .body(body)
}

fn authorized_keys_dir(state_path: &Path) -> std::path::PathBuf {
    state_path.join(".ai/node/auth/authorized_keys")
}

/// Write a grant for `subject` with explicit expiry, granted by `granted_by`.
fn plant_grant(
    state_path: &Path,
    fixture: &FastFixture,
    subject: &SigningKey,
    scopes: &[&str],
    granted_by: &SigningKey,
    expires_at: GrantExpiry,
) -> anyhow::Result<()> {
    let granter = format!("fp:{}", fingerprint(granted_by));
    ryeos_node_runtime::identity::create_authorized_key_toml(
        &authorized_keys_dir(state_path),
        &fingerprint(subject),
        &public_key_text(subject)["ed25519:".len()..],
        &scopes.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        "lifecycle-fixture",
        &granter,
        "2026-01-01T00:00:00Z",
        &GrantTerms {
            expires_at,
            authorized_by: AuthorizedBy::principal_request(&granter, None),
        },
        &fixture.node,
    )?;
    Ok(())
}

async fn body_of(response: reqwest::Response) -> (reqwest::StatusCode, serde_json::Value) {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::json!({ "raw": text })),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_grant_is_refused_with_a_typed_401_only_after_its_signature_verifies() {
    let holder = SigningKey::from_bytes(&[71u8; 32]);
    let stranger = SigningKey::from_bytes(&[72u8; 32]);
    let (harness, fixture) = DaemonHarness::start_fast_with(
        |state_path, _user, fixture| {
            register_workflow_bundles(state_path, fixture)?;
            plant_grant(
                state_path,
                fixture,
                &holder,
                &[AUTHORIZE_KEY_CAP],
                &fixture.user,
                GrantExpiry::At(1_000),
            )
        },
        |_| {},
    )
    .await
    .expect("daemon starts: a lapsed grant is not a boot failure");
    let body = serde_json::json!({
        "public_key": public_key_text(&stranger),
        "label": "x",
        "scopes": [LIST_CAP],
    });

    // The key holder learns the grant lapsed, and the cure.
    let (status, json) = body_of(
        signed_post(
            &harness,
            &fixture.node,
            &holder,
            &holder,
            "/authorize-key",
            body.clone(),
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "{json}");
    assert_eq!(json["error_code"], "grant_expired", "{json}");
    assert!(json["error"].as_str().unwrap().contains("renew"), "{json}");

    // Someone who only names the fingerprint, without the key, learns nothing.
    let (status, json) = body_of(
        signed_post(
            &harness,
            &fixture.node,
            &holder,
            &stranger,
            "/authorize-key",
            body,
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "{json}");
    assert!(json.get("error_code").is_none(), "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn issuance_above_the_class_maximum_is_refused_and_writes_no_grant() {
    let granter = SigningKey::from_bytes(&[73u8; 32]);
    let grantee = SigningKey::from_bytes(&[74u8; 32]);
    let (harness, fixture) = DaemonHarness::start_fast_with(
        |state_path, _user, fixture| {
            register_workflow_bundles(state_path, fixture)?;
            plant_grant(
                state_path,
                fixture,
                &granter,
                &[AUTHORIZE_KEY_CAP, LIST_CAP],
                &fixture.user,
                GrantExpiry::At(4_102_444_800),
            )
        },
        |_| {},
    )
    .await
    .expect("daemon starts");
    let (status, json) = body_of(
        signed_post(
            &harness,
            &fixture.node,
            &granter,
            &granter,
            "/authorize-key",
            serde_json::json!({
                "public_key": public_key_text(&grantee),
                "label": "too-long",
                "scopes": [LIST_CAP],
                "lifetime_secs": LOCAL_CLIENT_MAX_SECS + 1,
            }),
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(
        json["error_code"], "grant_lifetime_exceeds_policy",
        "{json}"
    );
    assert!(
        !authorized_keys_dir(&harness.state_path)
            .join(format!("{}.toml", fingerprint(&grantee)))
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_granter_renews_and_the_renewed_grant_authenticates_again() {
    let granter = SigningKey::from_bytes(&[75u8; 32]);
    let other = SigningKey::from_bytes(&[76u8; 32]);
    let grantee = SigningKey::from_bytes(&[77u8; 32]);
    let (harness, fixture) = DaemonHarness::start_fast_with(
        |state_path, _user, fixture| {
            register_workflow_bundles(state_path, fixture)?;
            for key in [&granter, &other] {
                plant_grant(
                    state_path,
                    fixture,
                    key,
                    &[AUTHORIZE_KEY_CAP, RENEW_KEY_CAP, LIST_CAP],
                    &fixture.user,
                    GrantExpiry::At(4_102_444_800),
                )?;
            }
            Ok(())
        },
        |_| {},
    )
    .await
    .expect("daemon starts");

    // The granter issues a one-second grant.
    let (status, json) = body_of(
        signed_post(
            &harness,
            &fixture.node,
            &granter,
            &granter,
            "/authorize-key",
            serde_json::json!({
                "public_key": public_key_text(&grantee),
                "label": "short-lived",
                "scopes": [RENEW_KEY_CAP],
                "lifetime_secs": 1,
            }),
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert!(status.is_success(), "{status} {json}");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let renew_body = serde_json::json!({ "fingerprint": fingerprint(&grantee) });

    // It has lapsed.
    let (status, json) = body_of(
        signed_post(
            &harness,
            &fixture.node,
            &grantee,
            &grantee,
            "/renew-key",
            renew_body.clone(),
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "{json}");
    assert_eq!(json["error_code"], "grant_expired", "{json}");

    // A principal that is not the granter cannot renew it.
    let (status, json) = body_of(
        signed_post(
            &harness,
            &fixture.node,
            &other,
            &other,
            "/renew-key",
            renew_body.clone(),
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["error_code"], "grant_renewal_not_permitted", "{json}");

    // The granter can.
    let (status, json) = body_of(
        signed_post(
            &harness,
            &fixture.node,
            &granter,
            &granter,
            "/renew-key",
            renew_body.clone(),
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert!(status.is_success(), "{status} {json}");

    // The grantee authenticates again, and still cannot renew itself.
    let (status, json) = body_of(
        signed_post(
            &harness,
            &fixture.node,
            &grantee,
            &grantee,
            "/renew-key",
            renew_body,
        )
        .send()
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json["error_code"], "grant_renewal_not_permitted", "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_grant_of_the_retired_class_keeps_the_node_from_starting_and_names_re_admission() {
    let stale = SigningKey::from_bytes(&[78u8; 32]);
    let plant = |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
        let dir = authorized_keys_dir(state_path);
        std::fs::create_dir_all(&dir)?;
        let body = format!(
            "schema_version = 2\nprincipal_class = \"remote_operator\"\n\
             origin_site_id = \"site:source\"\nfingerprint = \"{}\"\n\
             public_key = \"{}\"\nscopes = [\"{LIST_CAP}\"]\n\
             label = \"retired\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n",
            fingerprint(&stale),
            public_key_text(&stale),
        );
        std::fs::write(
            dir.join(format!("{}.toml", fingerprint(&stale))),
            lillux::signature::sign_content(&body, &fixture.node, "#", None),
        )?;
        Ok(())
    };
    let error = match DaemonHarness::start_fast_with(plant, |_| {}).await {
        Ok(_) => panic!("a stale grant must keep the daemon from starting"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("re-admit"), "{error}");
    assert!(error.contains("remote_principal"), "{error}");
}

const REVOKE_KEY_CAP: &str = "ryeos.execute.service.identity/revoke-key";

/// POST as `signer` and return the status with the JSON body.
async fn call(
    harness: &DaemonHarness,
    fixture: &FastFixture,
    signer: &SigningKey,
    path: &str,
    body: serde_json::Value,
) -> (reqwest::StatusCode, serde_json::Value) {
    body_of(
        signed_post(harness, &fixture.node, signer, signer, path, body)
            .send()
            .await
            .unwrap(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_key_cannot_call_and_restored_file_stays_dead() {
    let granter = SigningKey::from_bytes(&[81u8; 32]);
    let stranger = SigningKey::from_bytes(&[82u8; 32]);
    let grantee = SigningKey::from_bytes(&[83u8; 32]);
    let (harness, fixture) = DaemonHarness::start_fast_with(
        |state_path, _user, fixture| {
            register_workflow_bundles(state_path, fixture)?;
            for key in [&granter, &stranger] {
                plant_grant(
                    state_path,
                    fixture,
                    key,
                    &[AUTHORIZE_KEY_CAP, RENEW_KEY_CAP, REVOKE_KEY_CAP, LIST_CAP],
                    &fixture.user,
                    GrantExpiry::At(4_102_444_800),
                )?;
            }
            plant_grant(
                state_path,
                fixture,
                &grantee,
                &[RENEW_KEY_CAP, LIST_CAP],
                &granter,
                GrantExpiry::At(4_102_444_800),
            )?;
            Ok(())
        },
        |_| {},
    )
    .await
    .expect("daemon starts");
    let grant_file =
        authorized_keys_dir(&harness.state_path).join(format!("{}.toml", fingerprint(&grantee)));
    let saved = std::fs::read(&grant_file).expect("the planted grant exists");
    let digest = lillux::cas::sha256_hex(&saved);
    let revoke_body = serde_json::json!({
        "fingerprint": fingerprint(&grantee),
        "reason": "laptop lost",
    });
    let probe = serde_json::json!({ "fingerprint": fingerprint(&grantee) });

    // The grantee authenticates: it is refused for what it asks, not who it is.
    let (status, json) = call(&harness, &fixture, &grantee, "/renew-key", probe.clone()).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{json}");

    // A principal that did not grant it cannot revoke it.
    let (status, json) = call(
        &harness,
        &fixture,
        &stranger,
        "/revoke-key",
        revoke_body.clone(),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{json}");
    assert_eq!(
        json["error_code"], "grant_revocation_not_permitted",
        "{json}"
    );
    assert!(grant_file.exists(), "a refused revocation removes nothing");

    // Its granter can.
    let (status, json) = call(&harness, &fixture, &granter, "/revoke-key", revoke_body).await;
    assert!(status.is_success(), "{status} {json}");
    assert_eq!(
        json["revoked"][0]["grant_digest"],
        digest.as_str(),
        "{json}"
    );
    assert!(!grant_file.exists());
    let tombstone = harness
        .state_path
        .join(".ai/node/auth/revoked")
        .join(format!("{digest}.toml"));
    assert!(tombstone.exists(), "the revocation left a tombstone");

    // The key can no longer authenticate, and putting the file back changes
    // nothing: the tombstone names the exact bytes.
    let (status, _) = call(&harness, &fixture, &grantee, "/renew-key", probe.clone()).await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    std::fs::write(&grant_file, &saved).unwrap();
    let (status, _) = call(&harness, &fixture, &grantee, "/renew-key", probe.clone()).await;
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    std::fs::remove_file(&grant_file).unwrap();

    // Authorizing the key afresh is a new generation, which is allowed.
    let (status, json) = call(
        &harness,
        &fixture,
        &granter,
        "/authorize-key",
        serde_json::json!({
            "public_key": public_key_text(&grantee),
            "label": "re-granted",
            "scopes": [RENEW_KEY_CAP],
        }),
    )
    .await;
    assert!(status.is_success(), "{status} {json}");
    let (status, json) = call(&harness, &fixture, &grantee, "/renew-key", probe).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{json}");
}

/// `threads.get` status of `thread_id`, read as `signer`.
async fn thread_status(
    harness: &DaemonHarness,
    fixture: &FastFixture,
    signer: &SigningKey,
    thread_id: &str,
) -> String {
    let body = serde_json::json!({
        "item_ref": "service:threads/get",
        "ref_bindings": {},
        "project_path": null,
        "parameters": { "thread_id": thread_id },
        "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    });
    let (status, json) = call(harness, fixture, signer, "/execute", body).await;
    assert!(status.is_success(), "{status} {json}");
    json.pointer("/result/thread/status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// A project tool that runs until it is cancelled.
fn plant_sleeper_tool(root: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    let dir = root.join(".ai/tools/sleeper");
    std::fs::create_dir_all(&dir)?;
    let body = r#"#!/usr/bin/env python3
# ryeos-tool:
#   category: "sleeper"
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/script"
#   execution_protocol: "protocol:ryeos/core/opaque"
#   description: "Runs until cancelled"

import json
import time

time.sleep(600)
print(json.dumps({"slept": True}))
"#;
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(dir.join("sleeper.py"), signed)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn revoke_cancels_live_threads_of_principal() {
    let granter = SigningKey::from_bytes(&[84u8; 32]);
    let grantee = SigningKey::from_bytes(&[85u8; 32]);
    let sleeper_caps = [
        "ryeos.execute.tool.sleeper/sleeper",
        "ryeos.read.project.live",
        "ryeos.execute.service.threads/get",
    ];
    let project = tempfile::tempdir().expect("project tempdir");
    let (harness, fixture) = DaemonHarness::start_fast_with(
        |state_path, _user, fixture| {
            register_workflow_bundles(state_path, fixture)?;
            plant_grant(
                state_path,
                fixture,
                &granter,
                &[AUTHORIZE_KEY_CAP, REVOKE_KEY_CAP, LIST_CAP],
                &fixture.user,
                GrantExpiry::At(4_102_444_800),
            )?;
            plant_grant(
                state_path,
                fixture,
                &grantee,
                &sleeper_caps,
                &granter,
                GrantExpiry::At(4_102_444_800),
            )?;
            Ok(())
        },
        |_| {},
    )
    .await
    .expect("daemon starts");
    plant_sleeper_tool(project.path(), &fixture.publisher).expect("plant the sleeper tool");
    let project_path = project.path().canonicalize().unwrap();

    // The grantee starts the sleeper without waiting for it.
    let (status, json) = call(
        &harness,
        &fixture,
        &grantee,
        "/execute",
        serde_json::json!({
            "item_ref": "tool:sleeper/sleeper",
            "ref_bindings": {},
            "project_path": project_path.display().to_string(),
            "parameters": {},
            "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::local_live(
                ryeos_app::execution_policy::ExecutionResponse::Accepted,
            ),
        }),
    )
    .await;
    assert!(status.is_success(), "{status} {json}");
    let thread_id = json
        .pointer("/result/thread_id")
        .or_else(|| json.pointer("/thread_id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("the launch names its thread: {json}"))
        .to_owned();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while thread_status(&harness, &fixture, &grantee, &thread_id).await != "running" {
        assert!(
            std::time::Instant::now() < deadline,
            "the sleeper never ran"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Revoking the grantee's grant cancels the thread it is running.
    let (status, json) = call(
        &harness,
        &fixture,
        &granter,
        "/revoke-key",
        serde_json::json!({ "fingerprint": fingerprint(&grantee), "reason": "compromised" }),
    )
    .await;
    assert!(status.is_success(), "{status} {json}");
    let cancelled = json["live_work"]["cancelled"].as_array().unwrap();
    assert!(
        cancelled.iter().any(|id| id == thread_id.as_str()),
        "{json}"
    );
    assert_eq!(json["live_work"]["failed"], serde_json::json!([]), "{json}");

    // The key is dead, so read the outcome as its granter after authorizing
    // it again: the thread was cancelled, not left running.
    let (status, json) = call(
        &harness,
        &fixture,
        &granter,
        "/authorize-key",
        serde_json::json!({
            "public_key": public_key_text(&grantee),
            "label": "re-granted",
            "scopes": sleeper_caps,
        }),
    )
    .await;
    assert!(status.is_success(), "{status} {json}");
    assert_eq!(
        thread_status(&harness, &fixture, &grantee, &thread_id).await,
        "cancelled"
    );
}
