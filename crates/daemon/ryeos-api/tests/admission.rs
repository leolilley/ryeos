use ryeos_api::test_support as test_state;

use base64::Engine as _;
use lillux::crypto::Signer as _;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ryeos_api::handler_error::HandlerError;
use ryeos_api::handlers::{admission_claim, admission_status, admission_submit};
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;

const TEST_POLICY: &str = "local-node-v2";
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
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let subject_hash = store_subject(&state);
    let state = Arc::new(state);

    let submitted = admission_submit::handle(
        admission_submit::Request {
            subject_hash: subject_hash.clone(),
            policy: TEST_POLICY.to_string(),
            claim: "accepted".to_string(),
            max_objects: Some(16),
            max_blobs: Some(16),
            max_object_bytes: Some(4096),
            max_total_object_bytes: Some(4096),
            max_blob_bytes: Some(4096),
            max_total_blob_bytes: Some(4096),
            max_links_per_object: Some(16),
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
            max_objects: Some(16),
            max_blobs: Some(16),
            max_object_bytes: Some(4096),
            max_total_object_bytes: Some(4096),
            max_blob_bytes: Some(4096),
            max_total_blob_bytes: Some(4096),
            max_links_per_object: Some(16),
        },
        state,
    )
    .await
    .unwrap();
    assert_eq!(repeated["reused_existing"], true);
    assert_eq!(repeated["attestation_hash"], attestation_hash);
}

#[tokio::test]
async fn admission_submit_cannot_widen_node_object_closure_policy() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
    let subject_hash = store_subject(&state);
    let error = admission_submit::handle(
        admission_submit::Request {
            subject_hash,
            policy: TEST_POLICY.to_string(),
            claim: "accepted".to_string(),
            max_objects: Some(32_769),
            max_blobs: None,
            max_object_bytes: None,
            max_total_object_bytes: None,
            max_blob_bytes: None,
            max_total_blob_bytes: None,
            max_links_per_object: None,
        },
        Arc::new(state),
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("exceeds node policy"));
}

#[tokio::test]
async fn admission_status_reports_missing_head() {
    let (_tmp, _state_owner, state) = test_state::build_test_state();
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
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
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
    assert!(grant.contains("schema_version = 3"));
    assert!(grant.contains("principal_class = \"remote_node\""));
    // The admitted key asked for the grant by signing its claim; the grant
    // records that key and the digest of the signed claim, and it ends.
    assert!(grant.contains("expires_at = \""));
    assert!(grant.contains("[authorized_by]"));
    assert!(grant.contains(&format!("principal = \"fp:{fingerprint}\"")));
    assert!(grant.contains("request_digest = \""));
    assert!(grant.contains(&format!("origin_site_id = \"{}\"", state.threads.site_id())));
    assert!(grant.contains(&format!(
        "granted_by = \"admission:{}\"",
        &lillux::cas::sha256_hex(token.as_bytes())[..12]
    )));

    let reused = admission_claim::handle(
        signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("dev-machine")),
        state,
    )
    .await;
    assert!(matches!(reused, Err(HandlerError::Forbidden(_))));
}

#[tokio::test]
async fn admission_claim_rejects_wildcard_requested_scope() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
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
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
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
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
    let token = "wrong-audience-token";
    write_admission_token_file(&state, token, PUSH_SCOPES, None, 600);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let mut req = signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("dev-machine"));
    req.signature = sign_claim(
        ClaimToSign {
            audience: "fp:wrong-audience",
            token,
            class: "remote_node",
            public_key: &req.public_key,
            origin_site_id: req.origin_site_id.as_deref(),
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
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
    let token = "tampered-origin-site-token";
    write_admission_token_file(&state, token, PUSH_SCOPES, None, 600);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let mut req = signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("node"));
    req.origin_site_id = Some("site:attacker".to_string());

    let result = admission_claim::handle(req, Arc::new(state)).await;
    assert!(matches!(result, Err(HandlerError::Forbidden(_))));
}

#[tokio::test]
async fn admission_claim_enforces_hosted_policy_token_ttl() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(60);
    let token = "too-long-hosted-token";
    write_admission_token_file(&state, token, PUSH_SCOPES, None, 600);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let req = signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("dev-machine"));

    let result = admission_claim::handle(req, Arc::new(state)).await;

    assert!(matches!(result, Err(HandlerError::Forbidden(_))));
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("hosted-node policy maximum"),
    );
}

#[tokio::test]
async fn admission_claim_rejects_aged_overlong_hosted_policy_token() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(60);
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
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("hosted-node policy maximum"),
    );
}

#[tokio::test]
async fn admission_claim_refuses_explicitly_disabled_policy_before_token_use() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_disabled_hosted_admission();
    let token = "disabled-hosted-admission-token";
    write_admission_token_file(&state, token, PUSH_SCOPES, None, 600);
    let token_path = admission_token_path(
        &state.config.app_root,
        &lillux::cas::sha256_hex(token.as_bytes()),
    );
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let req = signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("dev-machine"));

    let result = admission_claim::handle(req, Arc::new(state)).await;

    let err = result.expect_err("disabled admission must reject every claim");
    assert!(err.to_string().contains("admission is disabled"));
    assert!(
        token_path.is_file(),
        "disabled admission must not consume tokens"
    );
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
        "version = 2\nclass = \"remote_node\"\ntoken_hash = \"{token_hash}\"\nscopes = [{}]\nissued_at_unix = {issued_at_unix}\nttl_secs = {ttl_secs}\nexpires_at_unix = {expires_at_unix}\n",
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
            class: "remote_node",
            public_key: &public_key,
            origin_site_id: Some(&origin_site_id),
            scopes: &scopes.iter().map(String::as_str).collect::<Vec<_>>(),
            signed_at,
            nonce: &nonce,
        },
        claimant,
    );
    admission_claim::Request {
        protocol_version: 3,
        token: token.to_string(),
        public_key,
        principal_class: AuthorizedKeyPrincipalClass::RemoteNode,
        origin_site_id: Some(origin_site_id),
        forwarding_key_id: None,
        forwarding_signature: None,
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
    class: &'a str,
    public_key: &'a str,
    origin_site_id: Option<&'a str>,
    scopes: &'a [&'a str],
    signed_at: u64,
    nonce: &'a str,
}

fn sign_claim(claim: ClaimToSign<'_>, claimant: &lillux::crypto::SigningKey) -> String {
    let token_hash = lillux::cas::sha256_hex(claim.token.as_bytes());
    let claim = format!(
        "ryeos-admission-claim-v3\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        claim.audience,
        token_hash,
        claim.class,
        claim.public_key,
        claim.origin_site_id.unwrap_or(""),
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

fn token_file_paths(
    state: &ryeos_app::state::AppState,
    token: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let hash = lillux::cas::sha256_hex(token.as_bytes());
    let toml = admission_token_path(&state.config.app_root, &hash);
    let claimed = toml.with_extension("claimed");
    (toml, claimed)
}

#[tokio::test]
async fn admission_claim_failed_grant_write_leaves_token_claimable() {
    let (tmp, _state_owner, mut state) = test_state::build_test_state_with_hosted_policy(600);
    let token = "test-admission-token-write-failure";
    write_admission_token_file(&state, token, PUSH_SCOPES, None, 600);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);

    // A regular file where the grant directory should be makes the grant
    // write fail after the token has been consumed.
    let blocker = tmp.path().join("auth-blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let mut broken = (*state.config).clone();
    broken.authorized_keys_dir = blocker;
    state.config = Arc::new(broken);

    let (toml, claimed) = token_file_paths(&state, token);
    assert!(toml.is_file());
    let req = signed_claim_request(&state, token, &claimant, PUSH_SCOPES, Some("dev-machine"));
    let result = admission_claim::handle(req, Arc::new(state)).await;

    assert!(
        matches!(result, Err(HandlerError::Internal(_))),
        "{result:?}"
    );
    assert!(toml.is_file(), "token must be claimable again");
    assert!(!claimed.exists(), "consumed marker must be rolled back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_claim_concurrent_claims_with_different_keys_admit_exactly_one() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
    let token = "test-admission-token-race";
    write_admission_token_file(&state, token, PUSH_SCOPES, None, 600);
    let first_key = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let second_key = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let first = signed_claim_request(&state, token, &first_key, PUSH_SCOPES, Some("one"));
    let second = signed_claim_request(&state, token, &second_key, PUSH_SCOPES, Some("two"));
    let state = Arc::new(state);

    let a = tokio::spawn(admission_claim::handle(first, state.clone()));
    let b = tokio::spawn(admission_claim::handle(second, state.clone()));
    let results = [a.await.unwrap(), b.await.unwrap()];

    let admitted = results.iter().filter(|r| r.is_ok()).count();
    let forbidden = results
        .iter()
        .filter(|r| matches!(r, Err(HandlerError::Forbidden(_))))
        .count();
    assert_eq!((admitted, forbidden), (1, 1), "{results:?}");
    let grants = std::fs::read_dir(&state.config.authorized_keys_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
        .count();
    assert_eq!(grants, 1);
}

/// Which shape of token and claim a class test builds.
struct ClassFixture {
    token: &'static str,
    class: &'static str,
    origin: Option<&'static str>,
    grant_lifetime_secs: Option<u64>,
}

fn write_class_token(state: &ryeos_app::state::AppState, fixture: &ClassFixture, version: u32) {
    let token_hash = lillux::cas::sha256_hex(fixture.token.as_bytes());
    let token_dir = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("admission")
        .join("tokens");
    std::fs::create_dir_all(&token_dir).unwrap();
    let issued_at_unix = now_unix();
    let mut doc = format!(
        "version = {version}\ntoken_hash = \"{token_hash}\"\nscopes = [\"{}\"]\nissued_at_unix = {issued_at_unix}\nttl_secs = 600\nexpires_at_unix = {}\n",
        PUSH_SCOPES[0],
        issued_at_unix + 600,
    );
    if version == 2 {
        doc.push_str(&format!("class = \"{}\"\n", fixture.class));
    }
    if let Some(origin) = fixture.origin {
        doc.push_str(&format!("origin_site_id = \"{origin}\"\n"));
    }
    if let Some(lifetime) = fixture.grant_lifetime_secs {
        doc.push_str(&format!("grant_lifetime_secs = {lifetime}\n"));
    }
    std::fs::write(
        admission_token_path(&state.config.app_root, &token_hash),
        doc,
    )
    .unwrap();
}

/// A claim of `class` for `origin`, signed by `claimant`, with no co-signature.
fn class_claim(
    state: &ryeos_app::state::AppState,
    token: &str,
    claimant: &lillux::crypto::SigningKey,
    class: AuthorizedKeyPrincipalClass,
    origin: Option<&str>,
) -> admission_claim::Request {
    let mut req = signed_claim_request(state, token, claimant, &PUSH_SCOPES[..1], None);
    req.principal_class = class;
    req.origin_site_id = origin.map(str::to_string);
    req.signature = sign_claim(
        ClaimToSign {
            audience: &state.identity.principal_id(),
            token,
            class: class.as_str(),
            public_key: &req.public_key,
            origin_site_id: origin,
            scopes: &[PUSH_SCOPES[0]],
            signed_at: req.signed_at,
            nonce: &req.nonce,
        },
        claimant,
    );
    req
}

/// Admit `forwarder` on this node as a `remote_node` of `site` holding the
/// forwarded-operator attestation scope.
fn admit_forwarding_node(
    state: &ryeos_app::state::AppState,
    forwarder: &lillux::crypto::SigningKey,
    site: &str,
    scopes: &[String],
) -> String {
    let vk = forwarder.verifying_key();
    let fp = lillux::signature::compute_fingerprint(&vk);
    ryeos_node_runtime::identity::write_authorized_remote_node_key_toml(
        &state.config.authorized_keys_dir,
        &fp,
        &base64::engine::general_purpose::STANDARD.encode(vk.as_bytes()),
        scopes,
        "forwarding-node",
        "test-granter",
        "2026-01-01T00:00:00Z",
        &ryeos_node_runtime::identity::GrantTerms::for_tests(),
        site,
        state.identity.signing_key(),
    )
    .unwrap();
    fp
}

fn attestation_scopes() -> Vec<String> {
    vec![ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE.to_string()]
}


/// Co-sign `req` as `forwarder`, which speaks for `site`, the way an origin
/// node attests a principal's claim.
fn cosign(
    state: &ryeos_app::state::AppState,
    req: &mut admission_claim::Request,
    token: &str,
    claimant: &lillux::crypto::SigningKey,
    forwarder: &lillux::crypto::SigningKey,
    site: &str,
) {
    let audience = state.identity.principal_id();
    let claim = admission_claim::claim_string(
        &audience,
        &lillux::cas::sha256_hex(token.as_bytes()),
        req.principal_class,
        &req.public_key,
        req.origin_site_id.as_deref(),
        &req.scopes,
        req.signed_at,
        &req.nonce,
    );
    let forwarder_fp = lillux::signature::compute_fingerprint(&forwarder.verifying_key());
    let key_id = format!("fp:{forwarder_fp}");
    let hash = admission_claim::cosignature_content_hash(
        &audience,
        &lillux::cas::sha256_hex(claim.as_bytes()),
        req.signed_at,
        &req.nonce,
        &lillux::signature::compute_fingerprint(&claimant.verifying_key()),
        &req.signature,
        &key_id,
        site,
    );
    let signature = forwarder.sign(hash.as_bytes());
    req.forwarding_key_id = Some(key_id);
    req.forwarding_signature =
        Some(base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()));
}

fn refusal_code(result: &Result<serde_json::Value, HandlerError>) -> String {
    match result {
        Err(HandlerError::Structured { code, .. }) => code.clone(),
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

const PRINCIPAL_TOKEN: ClassFixture = ClassFixture {
    token: "principal-token",
    class: "remote_principal",
    origin: Some("site:home"),
    grant_lifetime_secs: None,
};

#[tokio::test]
async fn principal_token_requires_principal_and_node_cosignature() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
    write_class_token(&state, &PRINCIPAL_TOKEN, 2);
    let token = PRINCIPAL_TOKEN.token;
    let (toml, _) = token_file_paths(&state, token);
    let principal = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let home_node = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let other_node = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let unadmitted_node = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    admit_forwarding_node(&state, &home_node, "site:home", &attestation_scopes());
    admit_forwarding_node(&state, &other_node, "site:elsewhere", &attestation_scopes());
    let state = Arc::new(state);
    let claim = |state: &ryeos_app::state::AppState| {
        class_claim(
            state,
            token,
            &principal,
            AuthorizedKeyPrincipalClass::RemotePrincipal,
            Some("site:home"),
        )
    };

    // The principal's signature alone is not enough.
    let alone = admission_claim::handle(claim(&state), state.clone()).await;
    assert_eq!(refusal_code(&alone), "admission_cosignature_missing");

    // A co-signature from a node that is not admitted here is refused.
    let mut req = claim(&state);
    cosign(&state, &mut req, token, &principal, &unadmitted_node, "site:home");
    let unadmitted = admission_claim::handle(req, state.clone()).await;
    assert_eq!(refusal_code(&unadmitted), "admission_cosignature_invalid");

    // A node admitted for another site cannot attest for the token's site.
    let mut req = claim(&state);
    cosign(&state, &mut req, token, &principal, &other_node, "site:home");
    let wrong_site = admission_claim::handle(req, state.clone()).await;
    assert_eq!(refusal_code(&wrong_site), "admission_cosignature_invalid");

    // A co-signature over a different claim signature does not transfer.
    let mut req = claim(&state);
    cosign(&state, &mut req, token, &principal, &home_node, "site:home");
    req.nonce = "another-nonce".to_string();
    let replayed = admission_claim::handle(req, state.clone()).await;
    assert!(replayed.is_err());

    // None of the refusals burned the token.
    assert!(toml.is_file(), "refused claims must leave the token claimable");

    let mut req = claim(&state);
    cosign(&state, &mut req, token, &principal, &home_node, "site:home");
    let response = admission_claim::handle(req, state.clone()).await.unwrap();
    assert_eq!(response["principal_class"], "remote_principal");
    assert_eq!(response["origin_site_id"], "site:home");
    let fingerprint = response["fingerprint"].as_str().unwrap();
    let grant = std::fs::read_to_string(
        state
            .config
            .authorized_keys_dir
            .join(format!("{fingerprint}.toml")),
    )
    .unwrap();
    assert!(grant.contains("principal_class = \"remote_principal\""));
    assert!(grant.contains("origin_site_id = \"site:home\""));
    assert!(grant.contains(&format!("principal = \"fp:{fingerprint}\"")));

    // Single use.
    let mut again = claim(&state);
    cosign(&state, &mut again, token, &principal, &home_node, "site:home");
    let reused = admission_claim::handle(again, state).await;
    assert!(matches!(reused, Err(HandlerError::Forbidden(_))));
}

#[tokio::test]
async fn node_token_cannot_admit_principal_class_and_the_reverse() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
    let node_token = ClassFixture {
        token: "node-token",
        class: "remote_node",
        origin: None,
        grant_lifetime_secs: None,
    };
    write_class_token(&state, &node_token, 2);
    write_class_token(&state, &PRINCIPAL_TOKEN, 2);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let state = Arc::new(state);

    let principal_claim_on_node_token = admission_claim::handle(
        class_claim(
            &state,
            node_token.token,
            &claimant,
            AuthorizedKeyPrincipalClass::RemotePrincipal,
            Some("site:home"),
        ),
        state.clone(),
    )
    .await;
    assert_eq!(
        refusal_code(&principal_claim_on_node_token),
        "admission_class_mismatch"
    );

    let node_claim_on_principal_token = admission_claim::handle(
        class_claim(
            &state,
            PRINCIPAL_TOKEN.token,
            &claimant,
            AuthorizedKeyPrincipalClass::RemoteNode,
            Some("site:home"),
        ),
        state.clone(),
    )
    .await;
    assert_eq!(
        refusal_code(&node_claim_on_principal_token),
        "admission_class_mismatch"
    );

    // Neither refusal consumed a token.
    assert!(token_file_paths(&state, node_token.token).0.is_file());
    assert!(token_file_paths(&state, PRINCIPAL_TOKEN.token).0.is_file());
}

#[tokio::test]
async fn principal_token_refuses_a_claim_for_another_origin_site() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
    write_class_token(&state, &PRINCIPAL_TOKEN, 2);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let forwarder = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    admit_forwarding_node(&state, &forwarder, "site:elsewhere", &attestation_scopes());
    let mut req = class_claim(
        &state,
        PRINCIPAL_TOKEN.token,
        &claimant,
        AuthorizedKeyPrincipalClass::RemotePrincipal,
        Some("site:elsewhere"),
    );
    cosign(
        &state,
        &mut req,
        PRINCIPAL_TOKEN.token,
        &claimant,
        &forwarder,
        "site:elsewhere",
    );
    let result = admission_claim::handle(req, Arc::new(state)).await;
    assert_eq!(refusal_code(&result), "admission_origin_site_mismatch");
}

#[tokio::test]
async fn forwarding_node_without_attestation_scope_cannot_cosign() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
    write_class_token(&state, &PRINCIPAL_TOKEN, 2);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let forwarder = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    admit_forwarding_node(
        &state,
        &forwarder,
        "site:home",
        &[PUSH_SCOPES[0].to_string()],
    );
    let mut req = class_claim(
        &state,
        PRINCIPAL_TOKEN.token,
        &claimant,
        AuthorizedKeyPrincipalClass::RemotePrincipal,
        Some("site:home"),
    );
    cosign(&state, &mut req, PRINCIPAL_TOKEN.token, &claimant, &forwarder, "site:home");
    let result = admission_claim::handle(req, Arc::new(state)).await;
    assert_eq!(refusal_code(&result), "admission_cosignature_invalid");
}

#[tokio::test]
async fn old_format_token_is_refused_as_stale() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
    let old = ClassFixture {
        token: "old-token",
        class: "remote_node",
        origin: None,
        grant_lifetime_secs: None,
    };
    write_class_token(&state, &old, 1);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let req = class_claim(
        &state,
        old.token,
        &claimant,
        AuthorizedKeyPrincipalClass::RemoteNode,
        Some("site:home"),
    );
    let result = admission_claim::handle(req, Arc::new(state)).await;
    assert_eq!(refusal_code(&result), "admission_token_stale");
    let Err(HandlerError::Structured { body, .. }) = result else {
        unreachable!()
    };
    assert!(body["error"].as_str().unwrap().contains("mint a new one"));
}

#[tokio::test]
async fn token_grant_lifetime_is_bounded_by_the_authorization_policy() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
    let long = ClassFixture {
        token: "long-lifetime-token",
        class: "local_client",
        origin: None,
        grant_lifetime_secs: Some(10_000_000_000),
    };
    write_class_token(&state, &long, 2);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let req = class_claim(
        &state,
        long.token,
        &claimant,
        AuthorizedKeyPrincipalClass::LocalClient,
        None,
    );
    let state = Arc::new(state);
    let result = admission_claim::handle(req, state.clone()).await;
    assert_eq!(refusal_code(&result), "grant_lifetime_exceeds_policy");
    assert!(
        token_file_paths(&state, long.token).0.is_file(),
        "a refused lifetime must leave the token claimable"
    );
}

#[tokio::test]
async fn local_client_token_admits_a_local_client_grant_without_origin_site() {
    let (_tmp, _state_owner, state) = test_state::build_test_state_with_hosted_policy(600);
    let local = ClassFixture {
        token: "local-client-token",
        class: "local_client",
        origin: None,
        grant_lifetime_secs: Some(3600),
    };
    write_class_token(&state, &local, 2);
    let claimant = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
    let state = Arc::new(state);

    let with_origin = admission_claim::handle(
        class_claim(
            &state,
            local.token,
            &claimant,
            AuthorizedKeyPrincipalClass::LocalClient,
            Some("site:home"),
        ),
        state.clone(),
    )
    .await;
    assert_eq!(refusal_code(&with_origin), "admission_origin_site_mismatch");

    let response = admission_claim::handle(
        class_claim(
            &state,
            local.token,
            &claimant,
            AuthorizedKeyPrincipalClass::LocalClient,
            None,
        ),
        state.clone(),
    )
    .await
    .unwrap();
    assert_eq!(response["principal_class"], "local_client");
    let fingerprint = response["fingerprint"].as_str().unwrap();
    let grant = std::fs::read_to_string(
        state
            .config
            .authorized_keys_dir
            .join(format!("{fingerprint}.toml")),
    )
    .unwrap();
    assert!(grant.contains("principal_class = \"local_client\""));
    assert!(!grant.contains("origin_site_id"));
}
