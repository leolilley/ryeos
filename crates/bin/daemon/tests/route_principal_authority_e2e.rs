//! Strict root admission for launches that have no human caller, end to end
//! through the daemon.
//!
//! An `auth: none` or `auth: hmac` launch route has no caller to lend
//! authority, so the route is the caller: it declares the capabilities it lends
//! in its signed `authority.capabilities`, and its bundle's signed manifest
//! bounds them. The launched item's declared capabilities must be covered by
//! that authority:
//!
//!   * a route whose authority covers them launches;
//!   * a route whose authority leaves any uncovered is refused with the typed
//!     `caller_capability_not_covered`, naming the route principal and every
//!     uncovered capability, before any thread exists;
//!   * a route that declares authority its manifest does not back is refused
//!     with `route_authority_not_backed`;
//!   * a route that declares nothing lends nothing.

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{
    FAST_FIXTURE_TIME, FastFixture, register_presigned_fixture_bundle, register_workflow_bundles,
};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

const BUNDLE: &str = "route-authority-fixture";
const HMAC_SECRET_ENV: &str = "ROUTE_AUTHORITY_FIXTURE_HMAC_SECRET";
const HMAC_SECRET: &str = "route-authority-fixture-secret-3c71";

const DECLARED_CAP: &str = "ryeos.execute.tool.echo/echo";
const SECOND_DECLARED_CAP: &str = "ryeos.execute.service.health/status";
const UNBACKED_CAP: &str = "ryeos.execute.directive.forbidden/everything";

/// The live-project profile every launch route needs, the operator-vault
/// capability its standard environment includes, and the execute capability of
/// the launched graph. Lent by a route, each must be backed by its manifest.
const LAUNCH_PROFILE: &[&str] = &[
    "ryeos.read.project.live",
    "ryeos.write.project.live",
    "ryeos.use.vault.operator",
    "ryeos.execute.graph.declares",
];

fn sign(content: &str, fixture: &FastFixture) -> String {
    lillux::signature::sign_content_at(content, &fixture.publisher, "#", None, FAST_FIXTURE_TIME)
}

fn write_signed(path: &Path, content: &str, fixture: &FastFixture) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().expect("fixture file has a parent"))?;
    std::fs::write(path, sign(content, fixture))?;
    Ok(())
}

/// A return-only graph that declares two capabilities it never exercises.
fn plant_declaring_graph(project: &Path, fixture: &FastFixture) -> anyhow::Result<()> {
    let body = format!(
        r#"category: test
version: "1.0.0"
requires:
  capabilities:
    declared:
      - {DECLARED_CAP}
      - {SECOND_DECLARED_CAP}
config:
  start: done
  nodes:
    done:
      node_type: return
"#
    );
    write_signed(&project.join(".ai/graphs/declares.yaml"), &body, fixture)
}

/// One launch route for `graph:declares` against `project`, with the declared
/// `authority` capabilities (none for an authority-less route).
fn launch_route(name: &str, auth: &str, project: &Path, authority: Option<&[&str]>) -> String {
    let auth_config = if auth == "hmac" {
        format!(
            "auth_config:\n  secret_env: {HMAC_SECRET_ENV}\n  signed_payload:\n    - from: body\n  signature:\n    header: x-signature\n    encoding: hex_lower\n    select:\n      kind: exact\n  delivery_id:\n    extract:\n      from: header\n      header: x-delivery-id\n  dedupe:\n    ttl_secs: 300\n    max_entries: 100\n"
        )
    } else {
        String::new()
    };
    let authority = authority.map_or_else(String::new, |capabilities| {
        let list = capabilities
            .iter()
            .map(|capability| format!("    - {capability}\n"))
            .collect::<String>();
        format!("authority:\n  capabilities:\n{list}")
    });
    format!(
        "id: {BUNDLE}/{name}\npath: /{BUNDLE}/{name}\nmethods:\n  - POST\nauth: {auth}\n{auth_config}request:\n  body: json\nlimits:\n  body_bytes_max: 4096\n  timeout_ms: 30000\n  concurrent_max: 4\nresponse:\n  mode: launch\n  source_config:\n    item_ref: graph:declares\n    ref_bindings: {{}}\n    project_path: {}\n{authority}",
        project.display()
    )
}

fn covering_authority() -> Vec<&'static str> {
    let mut capabilities = LAUNCH_PROFILE.to_vec();
    capabilities.extend([DECLARED_CAP, SECOND_DECLARED_CAP]);
    capabilities
}

/// Plant a registered bundle with one covering, one partial, one unbacked and
/// one authority-less route per auth kind. Its signed manifest bounds route
/// authority to the launch profile and the execute capabilities of the tools
/// and services the graph declares.
fn plant_route_bundle(state: &Path, project: &Path, fixture: &FastFixture) -> anyhow::Result<()> {
    let bundle_root = state.join(".ai/bundles").join(BUNDLE);
    let routes = bundle_root.join(".ai/node/routes");
    for auth in ["none", "hmac"] {
        let covering = covering_authority();
        let mut unbacked = covering_authority();
        unbacked.push(UNBACKED_CAP);
        for (suffix, authority) in [
            ("covered", Some(covering)),
            ("partial", Some(LAUNCH_PROFILE.to_vec())),
            ("unbacked", Some(unbacked)),
            ("bare", None),
        ] {
            let name = format!("{auth}-{suffix}");
            write_signed(
                &routes.join(format!("{name}.yaml")),
                &launch_route(&name, auth, project, authority.as_deref()),
                fixture,
            )?;
        }
    }
    write_signed(
        &bundle_root.join(".ai/manifest.yaml"),
        &format!(
            "name: {BUNDLE}\nversion: 1.0.0\ndescription: synthetic daemon integration fixture\nprovides_kinds: []\nrequires_kinds:\n  - handler\n  - parser\n  - runtime\nuses_kinds: []\nruntime_authority:\n  operator_vault: true\n  route_capabilities:\n    - ryeos.read.project.live\n    - ryeos.write.project.live\n    - ryeos.execute.graph.*\n    - ryeos.execute.tool.*\n    - ryeos.execute.service.*\n"
        ),
        fixture,
    )?;
    register_presigned_fixture_bundle(state, BUNDLE, &bundle_root, fixture)
}

async fn start() -> (DaemonHarness, tempfile::TempDir) {
    let project = tempfile::tempdir().expect("project tempdir");
    let project_path = project
        .path()
        .canonicalize()
        .expect("canonical project path");
    let (h, _fixture) = DaemonHarness::start_fast_with(
        |state: &Path, _user: &Path, fixture: &FastFixture| {
            register_workflow_bundles(state, fixture)?;
            plant_declaring_graph(&project_path, fixture)?;
            plant_route_bundle(state, &project_path, fixture)
        },
        |cmd| {
            cmd.env(HMAC_SECRET_ENV, HMAC_SECRET);
        },
    )
    .await
    .expect("start daemon with route-authority fixtures");
    (h, project)
}

fn hmac_signature(body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(HMAC_SECRET.as_bytes()).expect("hmac key");
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A delivery id no earlier request used, so webhook replay protection admits
/// every request of the run.
fn next_delivery_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!(
        "delivery-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// POST one request to the named route as its auth kind demands: unsigned for
/// `none`, HMAC-signed with a fresh delivery id for `hmac`.
async fn post(h: &DaemonHarness, auth: &str, name: &str) -> (reqwest::StatusCode, Value) {
    let body = serde_json::to_vec(&json!({ "event": name })).expect("encode body");
    let mut request = reqwest::Client::new()
        .post(format!("http://{}/{BUNDLE}/{auth}-{name}", h.bind))
        .header("content-type", "application/json");
    if auth == "hmac" {
        request = request
            .header("x-signature", hmac_signature(&body))
            .header("x-delivery-id", next_delivery_id());
    }
    let response = request.body(body).send().await.expect("post route");
    let status = response.status();
    let value = response.json().await.unwrap_or_else(|_| json!({}));
    (status, value)
}

fn principal_id(auth: &str, name: &str) -> String {
    match auth {
        "hmac" => format!("webhook:hmac:{BUNDLE}/{auth}-{name}"),
        _ => format!("route:{BUNDLE}/{auth}-{name}"),
    }
}

async fn route_whose_authority_covers_the_item_launches(auth: &str) {
    let (h, _project) = start().await;

    let (status, body) = post(&h, auth, "covered").await;

    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "body={body:#}");
    assert!(
        body.get("thread_id").and_then(Value::as_str).is_some(),
        "an admitted route launch exposes its thread id: {body:#}"
    );
}

async fn route_whose_authority_leaves_a_declared_cap_uncovered_is_refused(auth: &str) {
    let (h, _project) = start().await;

    let (status, body) = post(&h, auth, "partial").await;

    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "body={body:#}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some("caller_capability_not_covered"),
        "body={body:#}"
    );
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        error.contains(&principal_id(auth, "partial")),
        "the refusal must name the route principal: {error}"
    );
    assert!(
        error.contains(DECLARED_CAP) && error.contains(SECOND_DECLARED_CAP),
        "the refusal must name every uncovered cap: {error}"
    );
    assert!(
        error.contains("authority.capabilities"),
        "the refusal must name the route cure: {error}"
    );
    assert!(
        body.get("thread_id").is_none(),
        "a refused route launch must not expose a thread id: {body:#}"
    );
}

async fn route_whose_manifest_does_not_back_its_authority_is_refused(auth: &str) {
    let (h, _project) = start().await;

    let (status, body) = post(&h, auth, "unbacked").await;

    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "body={body:#}");
    assert_eq!(
        body.get("code").and_then(Value::as_str),
        Some("route_authority_not_backed"),
        "body={body:#}"
    );
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        error.contains(UNBACKED_CAP) && error.contains("runtime_authority.route_capabilities"),
        "the refusal must name the unbacked cap and its cure: {error}"
    );
}

async fn route_that_declares_no_authority_lends_nothing(auth: &str) {
    let (h, _project) = start().await;

    let (status, body) = post(&h, auth, "bare").await;

    assert_eq!(status, reqwest::StatusCode::FORBIDDEN, "body={body:#}");
    assert!(
        body.get("thread_id").is_none(),
        "an authority-less route must not launch: {body:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_route_whose_authority_covers_the_item_launches() {
    route_whose_authority_covers_the_item_launches("none").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_route_whose_authority_leaves_a_declared_cap_uncovered_is_refused() {
    route_whose_authority_leaves_a_declared_cap_uncovered_is_refused("none").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_route_whose_manifest_does_not_back_its_authority_is_refused() {
    route_whose_manifest_does_not_back_its_authority_is_refused("none").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_route_that_declares_no_authority_lends_nothing() {
    route_that_declares_no_authority_lends_nothing("none").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hmac_route_whose_authority_covers_the_item_launches() {
    route_whose_authority_covers_the_item_launches("hmac").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hmac_route_whose_authority_leaves_a_declared_cap_uncovered_is_refused() {
    route_whose_authority_leaves_a_declared_cap_uncovered_is_refused("hmac").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hmac_route_whose_manifest_does_not_back_its_authority_is_refused() {
    route_whose_manifest_does_not_back_its_authority_is_refused("hmac").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hmac_route_that_declares_no_authority_lends_nothing() {
    route_that_declares_no_authority_lends_nothing("hmac").await;
}
