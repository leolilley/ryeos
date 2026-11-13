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

use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use ryeosd::test_support as common;

use std::path::{Path, PathBuf};

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, write_authorized_key_with_scopes};
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

/// Service families the operator is granted whole, with the bundle-relative
/// directory their items live in.
const SERVICE_FAMILIES: [&str; 3] = ["credential-profiles", "worker-executions", "objects"];

/// Single services granted by exact reference.
const SINGLE_SERVICES: [&str; 2] = ["system/push-head", "external-execution/admit-product-program"];

/// Capability an item needs on top of its own execute capability. The
/// publication service writes the live project, which no service-family
/// wildcard covers.
const PUBLISH_SERVICE: &str = "worker-executions/publish";
const LIVE_PROJECT_WRITE: &str = "ryeos.write.project.live";

/// Live-project access the test launches need only because the harness binds a
/// directory on disk as the project. A launch pinned to a pushed snapshot does
/// not use it.
const PROJECT_SCAFFOLD: [&str; 2] = [
    "ryeos.read.project.live",
    "ryeos.write.project.live",
];

fn service_cap(service: &str) -> String {
    format!("ryeos.execute.service.{service}")
}

fn family_cap(family: &str) -> String {
    format!("ryeos.execute.service.{family}/*")
}

/// Every service item shipped in a family, as a `service:` reference.
fn family_services(family: &str) -> Vec<String> {
    let root: PathBuf = ryeos_engine::test_support::workspace_root()
        .join("bundles/core/.ai/services")
        .join(family);
    let mut found = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read service family") {
            let path = entry.expect("service entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "yaml") {
                let relative = path
                    .strip_prefix(&root)
                    .expect("under family")
                    .with_extension("");
                found.push(format!(
                    "service:{family}/{}",
                    relative.to_string_lossy().replace('\\', "/")
                ));
            }
        }
    }
    found.sort();
    assert!(!found.is_empty(), "family {family} ships no services");
    found
}

/// The complete derived grant.
fn derived_grant() -> Vec<String> {
    let mut grant = vec![
        TURN_EXECUTE_CAP.to_owned(),
        TURN_ENVIRONMENT_CAP.to_owned(),
        LOGIN_EXECUTE_CAP.to_owned(),
    ];
    grant.extend(SESSION_CAPS.map(str::to_owned));
    grant.extend(SERVICE_FAMILIES.map(family_cap));
    grant.extend(SINGLE_SERVICES.map(service_cap));
    grant.push(LIVE_PROJECT_WRITE.to_owned());
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
    ) || body
        .to_string()
        .contains("missing required capability")
}

struct Controller {
    harness: DaemonHarness,
    fixture: FastFixture,
    project: tempfile::TempDir,
}

impl Controller {
    async fn start() -> Self {
        // Plant the derived grant as the operator's authorized key; the
        // harness leaves a planted key for the fixture's user untouched.
        let initial: Vec<String> = derived_grant()
            .into_iter()
            .chain(PROJECT_SCAFFOLD.map(str::to_owned))
            .collect();
        let plant = move |state_path: &Path,
                          _user: &Path,
                          fixture: &FastFixture|
              -> anyhow::Result<()> {
            let scopes: Vec<&str> = initial.iter().map(String::as_str).collect();
            write_authorized_key_with_scopes(state_path, &fixture.user, &fixture.node, &scopes)
        };
        let (harness, fixture) = DaemonHarness::start_fast_with(plant, |_| {})
            .await
            .expect("start the controller daemon");
        Self {
            harness,
            fixture,
            project: tempfile::tempdir().expect("project tempdir"),
        }
    }

    /// Replace the operator's grant. Project scaffolding rides along so a
    /// launch is never refused for it instead of for the capability under test.
    fn grant(&self, scopes: &[String]) {
        let all: Vec<&str> = scopes
            .iter()
            .map(String::as_str)
            .chain(PROJECT_SCAFFOLD)
            .collect();
        write_authorized_key_with_scopes(
            &self.harness.state_path,
            &self.fixture.user,
            &self.fixture.node,
            &all,
        )
        .expect("rewrite the operator grant");
    }

    async fn launch_worker(&self, item_ref: &str, bindings: Value) -> (reqwest::StatusCode, Value) {
        let body = json!({
            "item_ref": item_ref,
            "ref_bindings": bindings,
            "project_path": self.project.path().to_str().unwrap(),
            "parameters": {
                "goal": {},
                "credential_profile_id": "controller-grant-inventory",
            },
            "execution_policy": ExecutionPolicy::local_live(ExecutionResponse::Wait),
        });
        self.harness
            .post_json("/execute", body)
            .await
            .expect("post worker execution")
    }

    async fn call_service(&self, service_ref: &str) -> (reqwest::StatusCode, Value) {
        let body = json!({
            "item_ref": service_ref,
            "ref_bindings": {},
            "project_path": null,
            "parameters": {},
            "execution_policy": ExecutionPolicy::projectless(ExecutionResponse::Wait),
        });
        self.harness
            .post_json("/execute", body)
            .await
            .expect("post service")
    }
}

fn turn_bindings() -> Value {
    json!({ "environment": TRUSTED_LANE_ENVIRONMENT })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hosted_turn_needs_exactly_its_execute_environment_and_session_caps() {
    let controller = Controller::start().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn the_trusted_worker_is_resolved_data_and_needs_no_execute_cap() {
    let controller = Controller::start().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn trusted_login_needs_exactly_its_execute_and_session_caps() {
    let controller = Controller::start().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn each_service_family_wildcard_covers_every_service_in_it() {
    let controller = Controller::start().await;
    let grant = derived_grant();

    for family in SERVICE_FAMILIES {
        let services = family_services(family);
        controller.grant(&grant);
        for service in &services {
            let (status, body) = controller.call_service(service).await;
            assert!(
                !is_capability_refusal(status, &body),
                "{service} must pass admission under {}: {status} {body:#}",
                family_cap(family)
            );
        }

        controller.grant(&without(&grant, &family_cap(family)));
        for service in &services {
            let (status, body) = controller.call_service(service).await;
            assert!(
                is_capability_refusal(status, &body),
                "{service} must be refused without {}: {status} {body:#}",
                family_cap(family)
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_publication_service_also_needs_live_project_write() {
    let controller = Controller::start().await;
    let service = format!("service:{PUBLISH_SERVICE}");
    let grant = derived_grant();

    controller.grant(&grant);
    let (status, body) = controller.call_service(&service).await;
    assert!(
        !is_capability_refusal(status, &body),
        "the derived grant must pass admission for {service}: {status} {body:#}"
    );

    // The scaffold also lends live-project write, so the grant under test is
    // rewritten without it by bypassing the scaffold.
    let without_write: Vec<&str> = grant
        .iter()
        .map(String::as_str)
        .filter(|cap| *cap != LIVE_PROJECT_WRITE)
        .collect();
    write_authorized_key_with_scopes(
        &controller.harness.state_path,
        &controller.fixture.user,
        &controller.fixture.node,
        &without_write,
    )
    .expect("rewrite the operator grant");
    let (status, body) = controller.call_service(&service).await;
    assert!(
        is_capability_refusal(status, &body) && body.to_string().contains(LIVE_PROJECT_WRITE),
        "the family wildcard alone must not publish: {status} {body:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn each_single_service_needs_exactly_its_own_execute_cap() {
    let controller = Controller::start().await;
    let grant = derived_grant();

    for service in SINGLE_SERVICES {
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
