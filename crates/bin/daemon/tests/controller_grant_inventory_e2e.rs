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
//! Every scenario runs on three lanes. The local lane signs as a local client.
//! The forwarded lane is the remote-principal path: the grant has class
//! `remote_principal` bound to an origin site, and each request carries the
//! origin node's forwarding co-signature, exactly as an origin node's
//! configured-operator client sends it, so the request passes through
//! `require_admitted_operator` with its source-node forwarding proof.
//! The delegated lane is how an origin node calls the controller as a stored
//! caller delegation: the origin node signs the request itself and attaches the
//! operator's signed delegation. The effective authority is the operator's
//! grant on the controller intersected with what the delegation lends, so the
//! delegation must lend the same list the grant holds.
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
use ryeos_node_runtime::remote_delegation::{
    DELEGATION_HEADER, DelegationBody, DelegationParties, SignedDelegation,
};
use serde_json::{Value, json};

const TURN_ITEM: &str = "worker_execution:codex/bounded-turn";
const LOGIN_ITEM: &str = "worker_execution:codex/trusted-login";
const TURN_ENVIRONMENT: &str = "config:codex/environments/external-authoring";

const TURN_EXECUTE_CAP: &str = "ryeos.execute.worker_execution.codex/bounded-turn";
const TURN_ENVIRONMENT_CAP: &str = "ryeos.execute.config.codex/environments/external-authoring";
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
const OPERATOR_SERVICES: [&str; 22] = [
    "credential-profiles/abandon-login",
    "credential-profiles/confirm",
    "credential-profiles/create",
    "credential-profiles/get",
    "external-content/accept-qualification-testimony",
    "external-content/product-accept",
    "external-content/product-offer",
    "launch/cancel",
    "launch/status",
    "objects/has",
    "objects/put",
    "objects/stage-chunk",
    "offline-operations/facts",
    "offline-operations/status",
    "offline-operations/submit",
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
/// whenever the request's execution policy overlays the operator vault. The
/// operator's launches carry `include_operator_vault: false`, so the capability
/// is outside the operator grant; it is a property of the execution policy's
/// environment, not of the authoring environment config.
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

/// The offline scope that lets the operator compose an external placement
/// binding on the stopped node. An offline scope rides only on the grant: a
/// delegation never lends one.
const OFFLINE_COMPOSE_CAP: &str = "ryeos.offline.node.external-execution.compose";

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
    grant.extend(OPERATOR_SERVICES.map(service_cap));
    grant.extend(SINGLE_SERVICES.map(service_cap));
    grant.push(OFFLINE_COMPOSE_CAP.to_owned());
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
    let code = body
        .get("code")
        .or_else(|| body.get("error_code"))
        .and_then(Value::as_str)
        .unwrap_or_default();
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
    /// Signed by a remote-principal grant bound to an origin site and
    /// co-signed by the origin node as the forwarding authority.
    Forwarded {
        operator: SigningKey,
        origin_node: SigningKey,
    },
    /// Signed by the origin node under the operator's delegation. The
    /// operator holds the same remote-principal grant as on the forwarded lane.
    Delegated {
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

    fn delegated() -> Self {
        Lane::Delegated {
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
/// With `delegation`, the origin node is the primary signer, as when it
/// forwards under a caller's stored delegation, and the delegation rides along.
async fn post_forwarded(
    base: &str,
    audience: &str,
    operator: &SigningKey,
    origin_node: &SigningKey,
    delegation: Option<&SignedDelegation>,
    path: &str,
    body: &Value,
) -> Result<(reqwest::StatusCode, Value)> {
    let operator = if delegation.is_some() {
        origin_node
    } else {
        operator
    };
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
    let mut request = reqwest::Client::new()
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
        .body(body_bytes);
    if let Some(delegation) = delegation {
        request = request.header(DELEGATION_HEADER, delegation.to_header_value()?);
    }
    let response = request.send().await?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or_else(|_| json!({}));
    Ok((status, value))
}

struct Controller {
    harness: DaemonHarness,
    fixture: FastFixture,
    project: tempfile::TempDir,
    lane: Lane,
    /// What the delegated lane's delegation lends. It defaults to every
    /// capability any scenario grants, so the operator's grant is the only
    /// limit unless a scenario narrows the delegation itself.
    lent: std::sync::Mutex<Vec<String>>,
}

/// The delegation's ceiling: the derived grant, the project scaffolding the
/// harness needs, and the two capabilities outside the operator set that some
/// scenarios add to the grant.
fn delegation_ceiling() -> Vec<String> {
    let mut lent = derived_grant();
    lent.retain(|cap| cap != OFFLINE_COMPOSE_CAP);
    lent.extend(PROJECT_SCAFFOLD.map(str::to_owned));
    lent.push(OPERATOR_VAULT_CAP.to_owned());
    lent.push(LIVE_PROJECT_WRITE.to_owned());
    lent.push(service_cap(PUBLISH_SERVICE));
    lent.sort();
    lent.dedup();
    lent
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
                    }
                    | Lane::Delegated {
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
            Lane::Forwarded { .. } | Lane::Delegated { .. } => {
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
        // A live project is a directory holding `.ai`; without it the launch
        // is refused as a malformed request before any capability is judged.
        let project = tempfile::tempdir().expect("project tempdir");
        std::fs::create_dir_all(project.path().join(ryeos_engine::AI_DIR))
            .expect("create the project .ai directory");
        Self {
            harness,
            fixture,
            project,
            lane,
            lent: std::sync::Mutex::new(delegation_ceiling()),
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
            }
            | Lane::Delegated {
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

    async fn post(&self, mut body: Value) -> (reqwest::StatusCode, Value) {
        // A remote principal's request names the origin site it arrives from,
        // exactly as the origin node's remote client stamps every forwarded
        // execute body; the receiver refuses a remote principal without it.
        if let Lane::Forwarded { origin_node, .. } | Lane::Delegated { origin_node, .. } =
            &self.lane
        {
            body["required_origin_site_id"] = Value::String(site_of(origin_node));
        }
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
                    None,
                    "/execute",
                    &body,
                )
                .await
            }
            Lane::Delegated {
                operator,
                origin_node,
            } => {
                let delegation = self.sign_delegation(operator, origin_node);
                post_forwarded(
                    &format!("http://{}", self.harness.bind),
                    &format!("fp:{}", self.fixture.node_fp()),
                    operator,
                    origin_node,
                    Some(&delegation),
                    "/execute",
                    &body,
                )
                .await
            }
        }
        .expect("post to the controller")
    }

    /// Narrow what the operator's delegation lends (delegated lane only).
    fn lend(&self, scopes: &[String]) {
        *self.lent.lock().expect("lent scopes") = scopes.to_vec();
    }

    /// The operator's delegation to the origin node for this controller, as
    /// `ryeos remote delegate` stores it on the origin.
    fn sign_delegation(&self, operator: &SigningKey, origin_node: &SigningKey) -> SignedDelegation {
        let not_after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_secs()
            + 600;
        let body = DelegationBody::new(
            &format!("fp:{}", fingerprint(operator)),
            DelegationParties {
                delegate_site: &site_of(origin_node),
                delegate_node: &format!("fp:{}", fingerprint(origin_node)),
                audience_site: &self.fixture.site_id(),
                audience_node: &format!("fp:{}", self.fixture.node_fp()),
            },
            &self.lent.lock().expect("lent scopes"),
            not_after,
        );
        SignedDelegation::sign(body, operator).expect("sign the operator delegation")
    }

    /// Launch with the operator's execution policy: a live project whose
    /// environment overlay does not expose the operator vault.
    async fn launch_worker(&self, item_ref: &str, bindings: Value) -> (reqwest::StatusCode, Value) {
        self.launch_worker_with_policy(item_ref, bindings, false)
            .await
    }

    async fn launch_worker_with_policy(
        &self,
        item_ref: &str,
        bindings: Value,
        include_operator_vault: bool,
    ) -> (reqwest::StatusCode, Value) {
        let mut policy = serde_json::to_value(ExecutionPolicy::local_live(ExecutionResponse::Wait))
            .expect("serialize the execution policy");
        *policy
            .pointer_mut("/environment/include_operator_vault")
            .expect("the live policy overlays the project environment") =
            json!(include_operator_vault);
        self.post(json!({
            "item_ref": item_ref,
            "ref_bindings": bindings,
            "project_path": self.project.path().to_str().unwrap(),
            "parameters": {
                "goal": {},
                "credential_profile_id": "controller-grant-inventory",
            },
            "execution_policy": policy,
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
    json!({ "environment": TURN_ENVIRONMENT })
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

/// The offline scope is grant-only: a delegation that lends it is refused, and
/// the delegation ceiling never includes it.
#[test]
fn the_offline_compose_scope_is_grant_only() {
    assert!(derived_grant().iter().any(|cap| cap == OFFLINE_COMPOSE_CAP));
    assert!(
        !delegation_ceiling()
            .iter()
            .any(|cap| cap == OFFLINE_COMPOSE_CAP)
    );
    assert!(
        ryeos_node_runtime::identity::reject_offline_scopes(&[OFFLINE_COMPOSE_CAP.to_owned()])
            .is_err(),
        "a delegation lending {OFFLINE_COMPOSE_CAP} must be refused"
    );
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

async fn a_vault_overlay_launch_needs_the_operator_vault_cap_the_operator_policy_avoids(
    lane: Lane,
) {
    let controller = Controller::start(lane).await;
    let grant = derived_grant();
    assert!(
        !grant.iter().any(|cap| cap == OPERATOR_VAULT_CAP),
        "the operator grant does not hold the operator vault capability"
    );

    for item in [TURN_ITEM, LOGIN_ITEM] {
        let bindings = if item == TURN_ITEM {
            turn_bindings()
        } else {
            json!({})
        };

        controller.grant(&grant);
        let (status, body) = controller
            .launch_worker_with_policy(item, bindings.clone(), false)
            .await;
        assert!(
            !is_capability_refusal(status, &body),
            "the operator policy passes the vault gate without the capability for {item}: {status} {body:#}"
        );

        let (status, body) = controller
            .launch_worker_with_policy(item, bindings.clone(), true)
            .await;
        assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "{item}: {body:#}");
        assert_eq!(
            body.get("error_code").and_then(Value::as_str),
            Some(OPERATOR_VAULT_REFUSAL),
            "a vault overlay without {OPERATOR_VAULT_CAP} must be refused for {item}: {body:#}"
        );

        let mut with_vault = grant.clone();
        with_vault.push(OPERATOR_VAULT_CAP.to_owned());
        controller.grant(&with_vault);
        let (status, body) = controller
            .launch_worker_with_policy(item, bindings, true)
            .await;
        assert!(
            body.get("error_code").and_then(Value::as_str) != Some(OPERATOR_VAULT_REFUSAL),
            "holding {OPERATOR_VAULT_CAP} admits the vault overlay for {item}: {status} {body:#}"
        );
    }
}

async fn the_publication_service_also_needs_live_project_write(lane: Lane) {
    let controller = Controller::start(lane).await;
    let service = format!("service:{PUBLISH_SERVICE}");
    let grant = derived_grant();

    // The operator grant does not publish: its own execute cap alone is refused.
    let mut with_execute = grant.clone();
    with_execute.push(service_cap(PUBLISH_SERVICE));
    controller.grant_exact(&with_execute);
    let (status, body) = controller.call_service(&service).await;
    assert!(
        is_capability_refusal(status, &body) && body.to_string().contains(LIVE_PROJECT_WRITE),
        "its own execute cap alone must not publish: {status} {body:#}"
    );

    // Adding live-project write, outside the operator set, is the only change needed.
    let mut with_write = with_execute.clone();
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

#[tokio::test(flavor = "multi_thread")]
async fn a_vault_overlay_launch_needs_the_operator_vault_cap_the_operator_policy_avoids_locally() {
    a_vault_overlay_launch_needs_the_operator_vault_cap_the_operator_policy_avoids(Lane::Local)
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vault_overlay_launch_needs_the_operator_vault_cap_the_operator_policy_avoids_through_the_remote_principal_forwarding_path()
 {
    a_vault_overlay_launch_needs_the_operator_vault_cap_the_operator_policy_avoids(
        Lane::forwarded(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hosted_turn_needs_exactly_its_execute_environment_and_session_caps_under_the_operators_delegation()
 {
    the_hosted_turn_needs_exactly_its_execute_environment_and_session_caps(Lane::delegated()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_trusted_worker_is_resolved_data_and_needs_no_execute_cap_under_the_operators_delegation()
 {
    the_trusted_worker_is_resolved_data_and_needs_no_execute_cap(Lane::delegated()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn trusted_login_needs_exactly_its_execute_and_session_caps_under_the_operators_delegation() {
    trusted_login_needs_exactly_its_execute_and_session_caps(Lane::delegated()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_publication_service_also_needs_live_project_write_under_the_operators_delegation() {
    the_publication_service_also_needs_live_project_write(Lane::delegated()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn each_operator_service_needs_exactly_its_own_execute_cap_under_the_operators_delegation() {
    each_operator_service_needs_exactly_its_own_execute_cap(Lane::delegated()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vault_overlay_launch_needs_the_operator_vault_cap_the_operator_policy_avoids_under_the_operators_delegation()
 {
    a_vault_overlay_launch_needs_the_operator_vault_cap_the_operator_policy_avoids(
        Lane::delegated(),
    )
    .await;
}

/// The delegation is a ceiling on top of the grant: lending less than the
/// grant holds narrows the controller, and lending the full list restores it.
#[tokio::test(flavor = "multi_thread")]
async fn a_delegation_lending_less_than_the_grant_refuses_what_it_omits() {
    let controller = Controller::start(Lane::delegated()).await;
    let grant = derived_grant();
    controller.grant(&grant);

    let mut lent = delegation_ceiling();
    lent.retain(|cap| cap != TURN_EXECUTE_CAP);
    controller.lend(&lent);
    let (status, body) = controller.launch_worker(TURN_ITEM, turn_bindings()).await;
    assert!(
        is_capability_refusal(status, &body) && body.to_string().contains(TURN_EXECUTE_CAP),
        "a delegation that omits {TURN_EXECUTE_CAP} must refuse the turn: {status} {body:#}"
    );

    controller.lend(&delegation_ceiling());
    let (status, body) = controller.launch_worker(TURN_ITEM, turn_bindings()).await;
    assert!(
        !is_capability_refusal(status, &body),
        "a delegation lending the derived list admits the turn: {status} {body:#}"
    );
}
