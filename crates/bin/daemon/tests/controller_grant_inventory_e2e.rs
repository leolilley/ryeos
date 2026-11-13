//! The exact capability set an origin operator needs on a controller node under
//! strict root admission, proved at the admission decision.
//!
//! The operator drives the controller through one authorized-key grant. Each
//! entry point it uses is posted twice over the real HTTP surface:
//!
//! * with a grant holding exactly the derived list, the request must not be
//!   refused for a capability (it may fail later, for a reason that has nothing
//!   to do with the grant, because no external provider exists in a test);
//! * with a grant that drops any single capability the entry point requires,
//!   the request must be refused with a capability refusal naming it.
//!
//! The grant file is rewritten between requests; the daemon re-reads and
//! re-verifies it on every authenticated request.
//!
//! Every scenario runs on two lanes. The local lane signs as a local client.
//! The forwarded lane is the remote-principal path: the grant has class
//! `remote_principal` bound to an origin site, and each request carries the
//! origin node's forwarding co-signature, exactly as an origin node's
//! configured-operator client sends it, so the request passes through
//! `require_admitted_operator` with its source-node forwarding proof.
//!
//! Any worker execution needs `ryeos.execute.worker_execution.<id>` plus the
//! three dedicated-session caps. The hosted turn is exercised here through
//! `worker_execution:codex/bounded-turn`; other turn items differ only in the
//! id inside the execute cap.

use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use ryeosd::test_support as common;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine as _;
use common::DaemonHarness;
use common::fast_fixture::{FastFixture, write_authorized_key_with_scopes};
use lillux::crypto::SigningKey;
use ryeos_node_runtime::identity::NodeIdentity;
use serde_json::{Value, json};

const TURN_ITEM: &str = "worker_execution:codex/bounded-turn";
const LOGIN_ITEM: &str = "worker_execution:codex/trusted-login";
const TRUSTED_LANE_ENVIRONMENT: &str = "config:codex/environments/external-authoring-trusted-lane";

const TURN_EXECUTE_CAP: &str = "ryeos.execute.worker_execution.codex/bounded-turn";
const TURN_ENVIRONMENT_CAP: &str =
    "ryeos.execute.config.codex/environments/external-authoring-trusted-lane";
const LOGIN_EXECUTE_CAP: &str = "ryeos.execute.worker_execution.codex/trusted-login";

/// The dedicated-session operations the worker-execution runtime and both
/// worker executions declare.
const SESSION_CAPS: [&str; 3] = [
    "ryeos.runtime.dedicated_session.start",
    "ryeos.runtime.dedicated_session.command",
    "ryeos.runtime.dedicated_session.terminate",
];

/// A remote grant carries no wildcard scope, so every service the operator
/// uses is granted by its exact capability.
///
/// Every service the operator calls on the controller, by bundle-relative id.
/// Project push reads `project/status`, then `objects/has` and `objects/put`,
/// then publishes through `system/push-head`. Product push calls
/// `external-content/product-offer`, `objects/stage-chunk` and
/// `external-content/product-accept`. Credential and worker-execution services
/// are listed individually; none outside this list is granted.
const OPERATOR_SERVICES: [&str; 16] = [
    "credential-profiles/abandon-login",
    "credential-profiles/confirm",
    "credential-profiles/create",
    "credential-profiles/get",
    "external-content/product-accept",
    "external-content/product-offer",
    "objects/has",
    "objects/put",
    "objects/stage-chunk",
    "project/status",
    "worker-executions/candidate-result",
    "worker-executions/command",
    "worker-executions/command-observation",
    "worker-executions/discard",
    "worker-executions/status",
    "worker-executions/terminate",
];

/// Services outside the lists above, also granted by exact reference.
const SINGLE_SERVICES: [&str; 2] = [
    "system/push-head",
    "external-execution/admit-product-program",
];

/// Operator vault access. The launch gate requires this fixed capability
/// whenever the request's execution policy overlays the operator vault, which
/// the live-project policy these launches use does. It is a property of the
/// execution policy's environment, not of the trusted-lane environment config.
const OPERATOR_VAULT_CAP: &str = "ryeos.use.vault.operator";
const OPERATOR_VAULT_REFUSAL: &str = "operator_vault_capability_required";

/// The publication service needs live-project write on top of its own execute
/// capability, which no other grant covers. The operator does not
/// publish, so the capability is outside the operator grant.
const PUBLISH_SERVICE: &str = "worker-executions/publish";
const LIVE_PROJECT_WRITE: &str = "ryeos.write.project.live";

/// Live-project access the test launches need only because the harness binds a
/// directory on disk as the project. A launch pinned to a pushed snapshot does
/// not use it.
const PROJECT_SCAFFOLD: [&str; 2] = ["ryeos.read.project.live", "ryeos.write.project.live"];

fn service_cap(service: &str) -> String {
    format!("ryeos.execute.service.{service}")
}

/// The complete derived grant.
fn derived_grant() -> Vec<String> {
    let mut grant = vec![
        TURN_EXECUTE_CAP.to_owned(),
        TURN_ENVIRONMENT_CAP.to_owned(),
        LOGIN_EXECUTE_CAP.to_owned(),
    ];
    grant.extend(SESSION_CAPS.map(str::to_owned));
    grant.push(OPERATOR_VAULT_CAP.to_owned());
    grant.extend(OPERATOR_SERVICES.map(service_cap));
    grant.extend(SINGLE_SERVICES.map(service_cap));
    grant
}

fn without(grant: &[String], dropped: &str) -> Vec<String> {
    let remaining: Vec<String> = grant
        .iter()
        .filter(|cap| cap.as_str() != dropped)
        .cloned()
        .collect();
    assert_eq!(
        remaining.len() + 1,
        grant.len(),
        "{dropped} is not in the grant"
    );
    remaining
}

/// A request refused for a capability, as opposed to any later outcome.
fn is_capability_refusal(status: reqwest::StatusCode, body: &Value) -> bool {
    if status != reqwest::StatusCode::FORBIDDEN {
        return false;
    }
    let code = body.get("code").and_then(Value::as_str).unwrap_or_default();
    matches!(
        code,
        "caller_capability_not_covered"
            | "insufficient_caps"
            | "service_cap_denied"
            | "ref_binding_unauthorized"
            | "operator_vault_capability_required"
    ) || body.to_string().contains("missing required capability")
}

/// How the operator's requests reach the controller.
#[derive(Clone)]
enum Lane {
    /// Signed by a local-client grant.
    Local,
    /// Signed by a remote-operator grant bound to an origin site and
    /// co-signed by the origin node as the forwarding authority.
    Forwarded {
        operator: SigningKey,
        origin_node: SigningKey,
    },
}

impl Lane {
    fn forwarded() -> Self {
        Lane::Forwarded {
            operator: common::fast_fixture::user_signing_key(),
            origin_node: common::fast_fixture::node_signing_key(),
        }
    }
}

fn fingerprint(key: &SigningKey) -> String {
    lillux::signature::compute_fingerprint(&key.verifying_key())
}

fn site_of(node_key: &SigningKey) -> String {
    format!("site:{}", fingerprint(node_key))
}

fn authorized_keys_dir(state_path: &Path) -> PathBuf {
    state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/auth/authorized_keys")
}

fn scope_strings(scopes: &[&str]) -> Vec<String> {
    scopes.iter().map(|scope| (*scope).to_owned()).collect()
}

/// Admit `remote_node` as a remote node of its own site, holding only the
/// authority to co-sign forwarded operator requests.
fn authorize_forwarding_node(
    state_path: &Path,
    local: &FastFixture,
    remote_node: &SigningKey,
) -> Result<()> {
    let public_key =
        base64::engine::general_purpose::STANDARD.encode(remote_node.verifying_key().as_bytes());
    ryeos_node_runtime::identity::write_authorized_remote_node_key_toml(
        &authorized_keys_dir(state_path),
        &fingerprint(remote_node),
        &public_key,
        &scope_strings(&[ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE]),
        "controller-grant-inventory-origin-node",
        &local.node_fp(),
        &lillux::time::iso8601_now(),
        &ryeos_node_runtime::identity::GrantTerms::for_tests(),
        &site_of(remote_node),
        &local.node,
    )?;
    Ok(())
}

/// Admit (or re-grant) `remote_principal` as a remote principal bound to
/// `remote_site_id`.
fn authorize_remote_principal(
    state_path: &Path,
    local: &FastFixture,
    remote_principal: &SigningKey,
    remote_site_id: &str,
    scopes: &[&str],
) -> Result<()> {
    let public_key = base64::engine::general_purpose::STANDARD
        .encode(remote_principal.verifying_key().as_bytes());
    let identity_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/private_key.pem");
    let identity = NodeIdentity::load(&identity_path)
        .with_context(|| format!("load daemon identity {}", identity_path.display()))?;
    ryeos_node_runtime::identity::reconcile_authorized_key_toml_scopes(
        &authorized_keys_dir(state_path),
        &fingerprint(remote_principal),
        &public_key,
        &scope_strings(scopes),
        "controller-grant-inventory-operator",
        &local.node_fp(),
        &lillux::time::iso8601_now(),
        &ryeos_node_runtime::identity::GrantTerms::for_tests(),
        &identity,
        ryeos_node_runtime::identity::WildcardPolicy::Reject,
        false,
        Some(remote_site_id),
        false,
    )?;
    Ok(())
}

/// POST as the origin's configured-operator client does: the operator signs
/// the request and the origin node co-signs it as the forwarding authority.
async fn post_forwarded(
    base: &str,
    audience: &str,
    operator: &SigningKey,
    origin_node: &SigningKey,
    path: &str,
    body: &Value,
) -> Result<(reqwest::StatusCode, Value)> {
    let body_bytes = serde_json::to_vec(body)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .to_string();
    let nonce = format!("{:016x}", rand::random::<u64>());
    let body_hash = lillux::cas::sha256_hex(&body_bytes);
    let to_sign =
        format!("ryeos-request-v1\nPOST\n{path}\n{body_hash}\n{timestamp}\n{nonce}\n{audience}");
    let content_hash = lillux::cas::sha256_hex(to_sign.as_bytes());
    let signature = lillux::crypto::Signer::sign(operator, content_hash.as_bytes());
    let signature_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
    let key_id = format!("fp:{}", fingerprint(operator));
    let forwarding_key_id = format!("fp:{}", fingerprint(origin_node));
    let origin_site_id = site_of(origin_node);
    let forwarded = format!(
        "ryeos-forwarded-request-v1\nPOST\n{path}\n{body_hash}\n{timestamp}\n{nonce}\n{audience}\n{key_id}\n{signature_b64}\n{forwarding_key_id}\n{origin_site_id}"
    );
    let forwarded_hash = lillux::cas::sha256_hex(forwarded.as_bytes());
    let forwarding_signature = lillux::crypto::Signer::sign(origin_node, forwarded_hash.as_bytes());
    let response = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header("content-type", "application/json")
        .header("x-ryeos-key-id", key_id)
        .header("x-ryeos-timestamp", timestamp)
        .header("x-ryeos-nonce", nonce)
        .header("x-ryeos-signature", signature_b64)
        .header("x-ryeos-forwarding-key-id", forwarding_key_id)
        .header("x-ryeos-forwarding-site-id", &origin_site_id)
        .header(
            "x-ryeos-forwarding-signature",
            base64::engine::general_purpose::STANDARD.encode(forwarding_signature.to_bytes()),
        )
        .body(body_bytes)
        .send()
        .await?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or_else(|_| json!({}));
    Ok((status, value))
}

struct Controller {
    harness: DaemonHarness,
    fixture: FastFixture,
    project: tempfile::TempDir,
    lane: Lane,
}

impl Controller {
    async fn start(lane: Lane) -> Self {
        let initial: Vec<String> = derived_grant()
            .into_iter()
            .chain(PROJECT_SCAFFOLD.map(str::to_owned))
            .collect();
        let plant_lane = lane.clone();
        let plant =
            move |state_path: &Path, _user: &Path, fixture: &FastFixture| -> anyhow::Result<()> {
                let scopes: Vec<&str> = initial.iter().map(String::as_str).collect();
                match &plant_lane {
                    Lane::Local => write_authorized_key_with_scopes(
                        state_path,
                        &fixture.user,
                        &fixture.node,
                        &scopes,
                    ),
                    Lane::Forwarded {
                        operator,
                        origin_node,
                    } => {
                        authorize_forwarding_node(state_path, fixture, origin_node)?;
                        authorize_remote_principal(
                            state_path,
                            fixture,
                            operator,
                            &site_of(origin_node),
                            &scopes,
                        )
                    }
                }
            };
        let started = match &lane {
            Lane::Local => DaemonHarness::start_fast_with(plant, |_| {}).await,
            // The controller has its own node and operator identities, so the
            // origin's operator is a remote principal on it, never its local one.
            Lane::Forwarded { .. } => {
                DaemonHarness::start_fast_with_node_and_user_keys(
                    SigningKey::from_bytes(&[71_u8; 32]),
                    SigningKey::from_bytes(&[72_u8; 32]),
                    plant,
                    |_| {},
                )
                .await
            }
        };
        let (harness, fixture) = started.expect("start the controller daemon");
        Self {
            harness,
            fixture,
            project: tempfile::tempdir().expect("project tempdir"),
            lane,
        }
    }

    /// Replace the operator's grant with exactly `scopes`.
    fn grant_exact(&self, scopes: &[String]) {
        let all: Vec<&str> = scopes.iter().map(String::as_str).collect();
        match &self.lane {
            Lane::Local => write_authorized_key_with_scopes(
                &self.harness.state_path,
                &self.fixture.user,
                &self.fixture.node,
                &all,
            ),
            Lane::Forwarded {
                operator,
                origin_node,
            } => authorize_remote_principal(
                &self.harness.state_path,
                &self.fixture,
                operator,
                &site_of(origin_node),
                &all,
            ),
        }
        .expect("rewrite the operator grant");
    }

    /// Replace the operator's grant. Project scaffolding rides along so a
    /// launch is never refused for it instead of for the capability under test.
    fn grant(&self, scopes: &[String]) {
        let mut all = scopes.to_vec();
        all.extend(PROJECT_SCAFFOLD.map(str::to_owned));
        self.grant_exact(&all);
    }

    async fn post(&self, body: Value) -> (reqwest::StatusCode, Value) {
        match &self.lane {
            Lane::Local => self.harness.post_json("/execute", body).await,
            Lane::Forwarded {
                operator,
                origin_node,
            } => {
                post_forwarded(
                    &format!("http://{}", self.harness.bind),
                    &format!("fp:{}", self.fixture.node_fp()),
                    operator,
                    origin_node,
                    "/execute",
                    &body,
                )
                .await
            }
        }
        .expect("post to the controller")
    }

    async fn launch_worker(&self, item_ref: &str, bindings: Value) -> (reqwest::StatusCode, Value) {
        self.post(json!({
            "item_ref": item_ref,
            "ref_bindings": bindings,
            "project_path": self.project.path().to_str().unwrap(),
            "parameters": {
                "goal": {},
                "credential_profile_id": "controller-grant-inventory",
            },
            "execution_policy": ExecutionPolicy::local_live(ExecutionResponse::Wait),
        }))
        .await
    }

    async fn call_service(&self, service_ref: &str) -> (reqwest::StatusCode, Value) {
        self.post(json!({
            "item_ref": service_ref,
            "ref_bindings": {},
            "project_path": null,
            "parameters": {},
            "execution_policy": ExecutionPolicy::projectless(ExecutionResponse::Wait),
        }))
        .await
    }
}

fn turn_bindings() -> Value {
    json!({ "environment": TRUSTED_LANE_ENVIRONMENT })
}

async fn the_hosted_turn_needs_exactly_its_execute_environment_and_session_caps(lane: Lane) {
    let controller = Controller::start(lane).await;
    let grant = derived_grant();

    controller.grant(&grant);
    let (status, body) = controller.launch_worker(TURN_ITEM, turn_bindings()).await;
    assert!(
        !is_capability_refusal(status, &body),
        "the derived grant must pass admission for the hosted turn: {status} {body:#}"
    );

    let needed = [TURN_EXECUTE_CAP, TURN_ENVIRONMENT_CAP]
        .into_iter()
        .chain(SESSION_CAPS);
    for dropped in needed {
        controller.grant(&without(&grant, dropped));
        let (status, body) = controller.launch_worker(TURN_ITEM, turn_bindings()).await;
        assert!(
            is_capability_refusal(status, &body),
            "dropping {dropped} must refuse the hosted turn: {status} {body:#}"
        );
        assert!(
            body.to_string().contains(dropped),
            "the refusal must name {dropped}: {body:#}"
        );
    }

    controller.grant(&without(&grant, OPERATOR_VAULT_CAP));
    let (status, body) = controller.launch_worker(TURN_ITEM, turn_bindings()).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body:#}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some(OPERATOR_VAULT_REFUSAL),
        "dropping {OPERATOR_VAULT_CAP} must refuse the hosted turn: {body:#}"
    );
}

async fn the_trusted_worker_is_resolved_data_and_needs_no_execute_cap(lane: Lane) {
    let controller = Controller::start(lane).await;
    let grant = derived_grant();
    assert!(
        grant.iter().all(|cap| !cap.contains(".worker.")),
        "the derived grant lends no capability over the worker item"
    );

    controller.grant(&grant);
    let (status, body) = controller.launch_worker(TURN_ITEM, turn_bindings()).await;
    assert!(
        !is_capability_refusal(status, &body),
        "the worker item is selected by the environment, not lent by the caller: {status} {body:#}"
    );
}

async fn trusted_login_needs_exactly_its_execute_and_session_caps(lane: Lane) {
    let controller = Controller::start(lane).await;
    let grant = derived_grant();

    controller.grant(&grant);
    let (status, body) = controller.launch_worker(LOGIN_ITEM, json!({})).await;
    assert!(
        !is_capability_refusal(status, &body),
        "the derived grant must pass admission for trusted login: {status} {body:#}"
    );

    for dropped in [LOGIN_EXECUTE_CAP].into_iter().chain(SESSION_CAPS) {
        controller.grant(&without(&grant, dropped));
        let (status, body) = controller.launch_worker(LOGIN_ITEM, json!({})).await;
        assert!(
            is_capability_refusal(status, &body),
            "dropping {dropped} must refuse trusted login: {status} {body:#}"
        );
        assert!(
            body.to_string().contains(dropped),
            "the refusal must name {dropped}: {body:#}"
        );
    }

    controller.grant(&without(&grant, OPERATOR_VAULT_CAP));
    let (status, body) = controller.launch_worker(LOGIN_ITEM, json!({})).await;
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{body:#}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some(OPERATOR_VAULT_REFUSAL),
        "dropping {OPERATOR_VAULT_CAP} must refuse the trusted login: {body:#}"
    );
}

async fn each_operator_service_needs_exactly_its_own_execute_cap(lane: Lane) {
    let controller = Controller::start(lane).await;
    let grant = derived_grant();

    for service in OPERATOR_SERVICES.into_iter().chain(SINGLE_SERVICES) {
        let service_ref = format!("service:{service}");
        controller.grant(&grant);
        let (status, body) = controller.call_service(&service_ref).await;
        assert!(
            !is_capability_refusal(status, &body),
            "{service_ref} must pass admission under the derived grant: {status} {body:#}"
        );

        let dropped = service_cap(service);
        controller.grant(&without(&grant, &dropped));
        let (status, body) = controller.call_service(&service_ref).await;
        assert!(
            is_capability_refusal(status, &body),
            "{service_ref} must be refused without {dropped}: {status} {body:#}"
        );
        assert!(
            body.to_string().contains(&dropped),
            "the refusal must name {dropped}: {body:#}"
        );
    }
}

/// Every granted service ships as a core service item.
#[test]
fn every_granted_service_ships_as_a_core_service_item() {
    let root = ryeos_engine::test_support::workspace_root().join("bundles/core/.ai/services");
    for service in OPERATOR_SERVICES.into_iter().chain(SINGLE_SERVICES) {
        assert!(
            root.join(format!("{service}.yaml")).is_file(),
            "{service} is not a shipped core service"
        );
    }
}

async fn the_publication_service_also_needs_live_project_write(lane: Lane) {
    let controller = Controller::start(lane).await;
    let service = format!("service:{PUBLISH_SERVICE}");
    let grant = derived_grant();

    // The operator grant does not publish: its own execute cap alone is refused.
    controller.grant_exact(&grant);
    let (status, body) = controller.call_service(&service).await;
    assert!(
        is_capability_refusal(status, &body) && body.to_string().contains(LIVE_PROJECT_WRITE),
        "its own execute cap alone must not publish: {status} {body:#}"
    );

    // Adding live-project write, outside the operator set, is the only change needed.
    let mut with_write = grant.clone();
    with_write.push(LIVE_PROJECT_WRITE.to_owned());
    controller.grant_exact(&with_write);
    let (status, body) = controller.call_service(&service).await;
    assert!(
        !is_capability_refusal(status, &body),
        "live-project write completes the publication grant: {status} {body:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hosted_turn_needs_exactly_its_execute_environment_and_session_caps_locally() {
    the_hosted_turn_needs_exactly_its_execute_environment_and_session_caps(Lane::Local).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hosted_turn_needs_exactly_its_execute_environment_and_session_caps_through_the_remote_principal_forwarding_path()
 {
    the_hosted_turn_needs_exactly_its_execute_environment_and_session_caps(Lane::forwarded()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_trusted_worker_is_resolved_data_and_needs_no_execute_cap_locally() {
    the_trusted_worker_is_resolved_data_and_needs_no_execute_cap(Lane::Local).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_trusted_worker_is_resolved_data_and_needs_no_execute_cap_through_the_remote_principal_forwarding_path()
 {
    the_trusted_worker_is_resolved_data_and_needs_no_execute_cap(Lane::forwarded()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn trusted_login_needs_exactly_its_execute_and_session_caps_locally() {
    trusted_login_needs_exactly_its_execute_and_session_caps(Lane::Local).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn trusted_login_needs_exactly_its_execute_and_session_caps_through_the_remote_principal_forwarding_path()
 {
    trusted_login_needs_exactly_its_execute_and_session_caps(Lane::forwarded()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_publication_service_also_needs_live_project_write_locally() {
    the_publication_service_also_needs_live_project_write(Lane::Local).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_publication_service_also_needs_live_project_write_through_the_remote_principal_forwarding_path()
 {
    the_publication_service_also_needs_live_project_write(Lane::forwarded()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn each_operator_service_needs_exactly_its_own_execute_cap_locally() {
    each_operator_service_needs_exactly_its_own_execute_cap(Lane::Local).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn each_operator_service_needs_exactly_its_own_execute_cap_through_the_remote_principal_forwarding_path()
 {
    each_operator_service_needs_exactly_its_own_execute_cap(Lane::forwarded()).await;
}
