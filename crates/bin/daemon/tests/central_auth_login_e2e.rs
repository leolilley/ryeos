//! A central-auth login, end to end through the daemon, on the isolation tier a
//! hosted container gets.
//!
//! A hosted container (Railway, Render) cannot create a mount namespace, so a
//! node there runs the `local_unconfined` host class: execution isolation is
//! disabled and the bundle state home is a daemon-checked convention, not a
//! mount. The fast fixture initializes a node under the same class, so this
//! test runs the real prepared central-auth bundle on that tier:
//!
//!   * with the caller's `ryeos.use.bundle-state.central-auth/<realm>` cap, a
//!     login succeeds and every byte of realm state lands under
//!     `<app_root>/.ai/state/bundle-state/central-auth/<realm>/`, with nothing
//!     under the earlier `central-auth/realms` location;
//!   * a caller without that cap is refused with `bundle_state_namespace_denied`
//!     naming the cap that cures it, and no realm directory is created;
//!   * a cap for another realm does not open this one.

use ryeosd::test_support as common;

use std::path::{Path, PathBuf};

use common::DaemonHarness;
use common::fast_fixture::{
    FastFixture, register_central_auth_bundle, register_workflow_bundles,
    write_authorized_key_with_scopes,
};
use lillux::crypto::SigningKey;
use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use serde_json::{Value, json};

const REALM: &str = "tv";
const PASSPHRASE: &str = "correct horse battery";

fn caller(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn scopes(use_caps: &[&str]) -> Vec<String> {
    let mut scopes = vec!["ryeos.execute.*".to_owned()];
    scopes.extend(use_caps.iter().map(|cap| (*cap).to_owned()));
    scopes
}

async fn start(callers: Vec<(SigningKey, Vec<String>)>) -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_standard_central_auth_with(
        move |state: &Path, _user: &Path, fixture: &FastFixture| {
            register_workflow_bundles(state, fixture)?;
            register_central_auth_bundle(state, fixture)?;
            for (key, scopes) in &callers {
                let scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
                write_authorized_key_with_scopes(state, key, &fixture.node, &scopes)?;
            }
            Ok(())
        },
        |_| {},
    )
    .await
    .expect("start daemon with the central-auth bundle")
}

async fn execute_as(
    h: &DaemonHarness,
    caller: &SigningKey,
    item_ref: &str,
    parameters: Value,
) -> (reqwest::StatusCode, Value) {
    let body = json!({
        "item_ref": item_ref,
        "ref_bindings": {},
        "project_path": null,
        "parameters": parameters,
        "execution_policy": ExecutionPolicy::projectless(ExecutionResponse::Wait),
    });
    let bytes = serde_json::to_vec(&body).expect("encode execute body");
    let node_key = h.node_key.as_ref().expect("fast fixture node key");
    let mut request = reqwest::Client::new()
        .post(format!("http://{}/execute", h.bind))
        .header("content-type", "application/json")
        .body(bytes.clone());
    for (name, value) in
        common::build_signed_headers_for_bytes(caller, node_key, "POST", "/execute", &bytes)
    {
        request = request.header(name, value);
    }
    let response = request.send().await.expect("post /execute");
    let status = response.status();
    let value = response.json().await.unwrap_or_else(|_| json!({}));
    (status, value)
}

fn realm_dir(h: &DaemonHarness, realm: &str) -> PathBuf {
    h.state_path
        .join(".ai/state/bundle-state/central-auth")
        .join(realm)
}

fn login_parameters(realm: &str) -> Value {
    json!({
        "realm_id": realm,
        "method": "passphrase",
        "principal_id": "alice",
        "passphrase": PASSPHRASE,
    })
}

/// Set the realm's policy and create its first principal as `operator`.
async fn seed_realm(h: &DaemonHarness, operator: &SigningKey) {
    let (status, body) = execute_as(
        h,
        operator,
        "tool:central-auth/set-policy",
        json!({
            "realm_id": REALM,
            "policy": {
                "roles": { "viewer": { "capabilities": ["tv.read"] } },
                "allowed_capabilities": ["tv.read"],
                "login_throttle": { "max_failures": 3, "window_secs": 600, "lockout_secs": 300 },
            },
        }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "set-policy: {body:#}");
    let (status, body) = execute_as(
        h,
        operator,
        "tool:central-auth/create-principal",
        json!({
            "realm_id": REALM,
            "principal_id": "alice",
            "display_name": "Alice",
            "roles": ["viewer"],
            "passphrase": PASSPHRASE,
            "bootstrap": true,
        }),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "create-principal: {body:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_login_with_the_realm_cap_keeps_its_state_in_the_bundle_state_home() {
    let operator = caller(0x71);
    let (h, _fixture) = start(vec![(
        operator.clone(),
        scopes(&["ryeos.use.bundle-state.central-auth/tv"]),
    )])
    .await;
    // The tier a hosted container gets: no sandbox, so no mount.
    assert_eq!(
        common::fast_fixture::node_isolation_mode(&h.state_path)
            .expect("read the node's isolation mode"),
        "disabled",
        "the central-auth e2e covers the unconfined tier a hosted container runs"
    );
    seed_realm(&h, &operator).await;

    let (status, body) = execute_as(
        &h,
        &operator,
        "tool:central-auth/login",
        login_parameters(REALM),
    )
    .await;

    assert_eq!(status, reqwest::StatusCode::OK, "login: {body:#}");
    let text = body.to_string();
    assert!(
        text.contains("session_token"),
        "login issues a token: {text}"
    );
    let home = realm_dir(&h, REALM);
    assert!(home.join("policy.json").is_file(), "{}", home.display());
    assert!(home.join("principals/alice.json").is_file());
    let sessions = std::fs::read_dir(home.join("sessions"))
        .expect("the login stores its session in the state home")
        .count();
    assert_eq!(sessions, 1);
    assert!(
        home.parent().unwrap().join(".home.json").is_file(),
        "the home carries the node's signed record"
    );
    assert!(
        !h.state_path.join(".ai/state/central-auth").exists(),
        "nothing is written under the earlier central-auth/realms location"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_login_without_the_realm_cap_is_refused_and_creates_nothing() {
    let operator = caller(0x72);
    let outsider = caller(0x73);
    let other_realm = caller(0x74);
    let (h, _fixture) = start(vec![
        (
            operator.clone(),
            scopes(&["ryeos.use.bundle-state.central-auth/tv"]),
        ),
        (outsider.clone(), scopes(&[])),
        (
            other_realm.clone(),
            scopes(&["ryeos.use.bundle-state.central-auth/other"]),
        ),
    ])
    .await;
    seed_realm(&h, &operator).await;
    let sessions_before = std::fs::read_dir(realm_dir(&h, REALM).join("sessions"))
        .map(|dir| dir.count())
        .unwrap_or(0);

    for who in [&outsider, &other_realm] {
        let (status, body) =
            execute_as(&h, who, "tool:central-auth/login", login_parameters(REALM)).await;
        assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "body={body:#}");
        let text = body.to_string();
        assert!(text.contains("bundle_state_namespace_denied"), "{text}");
        assert!(
            text.contains("ryeos.use.bundle-state.central-auth/tv")
                && text.contains("authorize-key"),
            "the refusal must name the cap that cures it: {text}"
        );
    }
    let sessions_after = std::fs::read_dir(realm_dir(&h, REALM).join("sessions"))
        .map(|dir| dir.count())
        .unwrap_or(0);
    assert_eq!(
        sessions_before, sessions_after,
        "a refused login issues no session"
    );
}
