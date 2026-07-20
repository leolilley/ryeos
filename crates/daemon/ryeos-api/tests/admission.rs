mod test_state;

use base64::Engine as _;
use lillux::crypto::Signer as _;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ryeos_api::handler_error::HandlerError;
use ryeos_api::handlers::{admission_claim, admission_status, admission_submit};

const TEST_POLICY: &str = "local-node-v1";
const PUSH_SCOPES: &[&str] = &[
    "ryeos.execute.service.objects/has",
    "ryeos.execute.service.objects/put",
    "ryeos.execute.service.objects/get",
    "ryeos.execute.service.system/push-head",
];

fn store_subject(state: &ryeos_app::state::AppState) -> String {
    let cas = lillux::cas::CasStore::new(state.state_store.cas_root().unwrap());
    cas.store_object(&json!({
        "kind": "source_manifest",
        "item_source_hashes": {}
    }))
    .unwrap()
}

#[tokio::test]
async fn admission_submit_writes_attestation_and_status_reads_it() {
    let (_tmp, state) = test_state::build_test_state();
    let subject_hash = store_subject(&state);
    let state = Arc::new(state);

    let submitted = admission_submit::handle(
        admission_submit::Request {
            subject_hash: subject_hash.clone(),
            policy: TEST_POLICY.to_string(),
            claim: "accepted".to_string(),
            max_objects: 16,
            max_blobs: 16,
            max_object_bytes: 4096,
            max_blob_bytes: 4096,
            max_total_blob_bytes: 4096,
            max_links_per_object: 16,
        },
        state.clone(),
    )
    .await
    .unwrap();

    assert_eq!(submitted["subject_hash"], subject_hash);
    assert_eq!(submitted["policy"], TEST_POLICY);
    assert_eq!(submitted["claim"], "accepted");
    assert_eq!(submitted["reused_existing"], false);
    let attestation_hash = submitted["attestation_hash"].as_str().unwrap().to_string();
    assert!(lillux::valid_hash(&attestation_hash));

    let status = admission_status::handle(
        admission_status::Request {
            subject_hash: subject_hash.clone(),
            policy: TEST_POLICY.to_string(),
        },
        state.clone(),
    )
    .await
    .unwrap();

    assert_eq!(status["status"], "accepted");
    assert_eq!(status["attestation_hash"], attestation_hash);
    assert_eq!(status["attestation"]["subject_hash"], subject_hash);
    assert_eq!(status["attestation"]["policy"], TEST_POLICY);

    let repeated = admission_submit::handle(
        admission_submit::Request {
            subject_hash,
            policy: TEST_POLICY.to_string(),
            claim: "accepted".to_string(),
            max_objects: 16,
            max_blobs: 16,
            max_object_bytes: 4096,
            max_blob_bytes: 4096,
            max_total_blob_bytes: 4096,
            max_links_per_object: 16,
        },
        state,
    )
    .await
    .unwrap();
    assert_eq!(repeated["reused_existing"], true);
    assert_eq!(repeated["attestation_hash"], attestation_hash);
}

#[tokio::test]
async fn admission_status_reports_missing_head() {
    let (_tmp, state) = test_state::build_test_state();
    let status = admission_status::handle(
        admission_status::Request {
            subject_hash: "ab".repeat(32),
            policy: TEST_POLICY.to_string(),
        },
        Arc::new(state),
    )
    .await
    .unwrap();

    assert_eq!(status["status"], "missing");
}

#[tokio::test]
async fn admission_claim_writes_authorized_key_and_rejects_reuse() {
    let (_tmp, state) = test_state::build_test_state();
    let token = "test-admission-token";
    write_admission_token_file(&state, token, PUSH_SCOPES, None, 600);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let req = signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("dev-machine"));
    let state = Arc::new(state);

    let response = admission_claim::handle(req, state.clone()).await.unwrap();

    let fingerprint = response["fingerprint"].as_str().unwrap();
    assert_eq!(response["admitted"], true);
    assert_eq!(response["label"], "dev-machine");
    let grant_path = state
        .config
        .authorized_keys_dir
        .join(format!("{fingerprint}.toml"));
    let grant = std::fs::read_to_string(grant_path).unwrap();
    assert!(grant.contains("schema_version = 2"));
    assert!(grant.contains("principal_class = \"remote_node\""));
    assert!(grant.contains(&format!("origin_site_id = \"{}\"", state.threads.site_id())));

    let reused = admission_claim::handle(
        signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("dev-machine")),
        state,
    )
    .await;
    assert!(matches!(reused, Err(HandlerError::Forbidden(_))));
}

#[tokio::test]
async fn admission_claim_rejects_wildcard_requested_scope() {
    let (_tmp, state) = test_state::build_test_state();
    let token = "wildcard-request-token";
    write_admission_token_file(&state, token, PUSH_SCOPES, None, 600);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let req = signed_claim_request(
        &state,
        token,
        &claimant,
        &["ryeos.execute.service.*"],
        Some("dev-machine"),
    );

    let result = admission_claim::handle(req, Arc::new(state)).await;
    assert!(matches!(result, Err(HandlerError::Forbidden(_))));
}

#[tokio::test]
async fn admission_claim_rejects_wildcard_token_file_scope() {
    let (_tmp, state) = test_state::build_test_state();
    let token = "wildcard-token-file-token";
    write_admission_token_file(&state, token, &["ryeos.execute.service.*"], None, 600);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let req = signed_claim_request(
        &state,
        token,
        &claimant,
        &["ryeos.execute.service.objects/has"],
        Some("dev-machine"),
    );

    let result = admission_claim::handle(req, Arc::new(state)).await;
    assert!(matches!(result, Err(HandlerError::Forbidden(_))));
}

#[tokio::test]
async fn admission_claim_rejects_wrong_audience_signature() {
    let (_tmp, state) = test_state::build_test_state();
    let token = "wrong-audience-token";
    write_admission_token_file(&state, token, PUSH_SCOPES, None, 600);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let mut req = signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("dev-machine"));
    req.signature = sign_claim(
        ClaimToSign {
            audience: "fp:wrong-audience",
            token,
            public_key: &req.public_key,
            origin_site_id: &req.origin_site_id,
            scopes: &req.scopes.iter().map(String::as_str).collect::<Vec<_>>(),
            signed_at: req.signed_at,
            nonce: &req.nonce,
        },
        &claimant,
    );

    let result = admission_claim::handle(req, Arc::new(state)).await;
    assert!(matches!(result, Err(HandlerError::Forbidden(_))));
}

#[tokio::test]
async fn admission_claim_rejects_origin_site_changed_after_signing() {
    let (_tmp, state) = test_state::build_test_state();
    let token = "tampered-origin-site-token";
    write_admission_token_file(&state, token, PUSH_SCOPES, None, 600);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let mut req = signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("node"));
    req.origin_site_id = "site:attacker".to_string();

    let result = admission_claim::handle(req, Arc::new(state)).await;
    assert!(matches!(result, Err(HandlerError::Forbidden(_))));
}

#[tokio::test]
async fn admission_claim_enforces_hosted_policy_token_ttl() {
    let (_tmp, state) = test_state::build_test_state_with_hosted_policy(60);
    let token = "too-long-hosted-token";
    write_admission_token_file(&state, token, PUSH_SCOPES, None, 600);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let req = signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("dev-machine"));

    let result = admission_claim::handle(req, Arc::new(state)).await;

    assert!(matches!(result, Err(HandlerError::Forbidden(_))));
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("hosted-node policy maximum"),);
}

#[tokio::test]
async fn admission_claim_rejects_aged_overlong_hosted_policy_token() {
    let (_tmp, state) = test_state::build_test_state_with_hosted_policy(60);
    let token = "aged-too-long-hosted-token";
    let issued_at_unix = now_unix() - 540;
    write_admission_token_file_with_issued_at(
        &state,
        token,
        PUSH_SCOPES,
        None,
        issued_at_unix,
        600,
    );
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let req = signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("dev-machine"));

    let result = admission_claim::handle(req, Arc::new(state)).await;

    assert!(matches!(result, Err(HandlerError::Forbidden(_))));
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("hosted-node policy maximum"),);
}

fn write_admission_token_file(
    state: &ryeos_app::state::AppState,
    token: &str,
    scopes: &[&str],
    label: Option<&str>,
    ttl_secs: u64,
) {
    let issued_at_unix = now_unix();
    write_admission_token_file_with_issued_at(
        state,
        token,
        scopes,
        label,
        issued_at_unix,
        ttl_secs,
    );
}

fn write_admission_token_file_with_issued_at(
    state: &ryeos_app::state::AppState,
    token: &str,
    scopes: &[&str],
    label: Option<&str>,
    issued_at_unix: u64,
    ttl_secs: u64,
) {
    let token_hash = lillux::cas::sha256_hex(token.as_bytes());
    let token_dir = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("admission")
        .join("tokens");
    std::fs::create_dir_all(&token_dir).unwrap();
    let expires_at_unix = issued_at_unix + ttl_secs;
    let mut doc = format!(
        "version = 1\ntoken_hash = \"{token_hash}\"\nscopes = [{}]\nissued_at_unix = {issued_at_unix}\nttl_secs = {ttl_secs}\nexpires_at_unix = {expires_at_unix}\n",
        scopes
            .iter()
            .map(|scope| format!("\"{scope}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if let Some(label) = label {
        doc.push_str(&format!("label = \"{label}\"\n"));
    }
    std::fs::write(
        admission_token_path(&state.config.app_root, &token_hash),
        doc,
    )
    .unwrap();
}

fn signed_claim_request(
    state: &ryeos_app::state::AppState,
    token: &str,
    claimant: &lillux::crypto::SigningKey,
    scopes: &[&str],
    label: Option<&str>,
) -> admission_claim::Request {
    let public_key = format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(claimant.verifying_key().as_bytes())
    );
    let mut scopes = scopes.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    let signed_at = now_unix();
    let nonce = "test-nonce".to_string();
    let origin_site_id = state.threads.site_id().to_string();
    let audience = state.identity.principal_id();
    let signature = sign_claim(
        ClaimToSign {
            audience: &audience,
            token,
            public_key: &public_key,
            origin_site_id: &origin_site_id,
            scopes: &scopes.iter().map(String::as_str).collect::<Vec<_>>(),
            signed_at,
            nonce: &nonce,
        },
        claimant,
    );
    admission_claim::Request {
        protocol_version: 2,
        token: token.to_string(),
        public_key,
        origin_site_id,
        label: label.map(str::to_string),
        scopes,
        signed_at,
        nonce,
        signature,
    }
}

struct ClaimToSign<'a> {
    audience: &'a str,
    token: &'a str,
    public_key: &'a str,
    origin_site_id: &'a str,
    scopes: &'a [&'a str],
    signed_at: u64,
    nonce: &'a str,
}

fn sign_claim(claim: ClaimToSign<'_>, claimant: &lillux::crypto::SigningKey) -> String {
    let token_hash = lillux::cas::sha256_hex(claim.token.as_bytes());
    let claim = format!(
        "ryeos-admission-claim-v2\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        claim.audience,
        token_hash,
        claim.public_key,
        claim.origin_site_id,
        claim.scopes.join(","),
        claim.signed_at,
        claim.nonce,
    );
    let content_hash = lillux::cas::sha256_hex(claim.as_bytes());
    let signature = claimant.sign(content_hash.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
}

fn admission_token_path(app_root: &Path, token_hash: &str) -> std::path::PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("admission")
        .join("tokens")
        .join(format!("{token_hash}.toml"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
