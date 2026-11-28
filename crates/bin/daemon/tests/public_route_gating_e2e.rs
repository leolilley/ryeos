//! A route signed `public: true` is served only while the node's signed
//! `public_routes` policy names it.
//!
//! A fixture bundle carries one public static route. With the policy naming
//! it, the node serves it. The policy is then replaced on the stopped node
//! with one that does not name it, as `ryeos node policy-apply` would, and the
//! node restarts: the route's path answers exactly as an unknown path does,
//! the rest of the node (the UI) still serves, the daemon reports the route as
//! left out, and `ryeos node doctor` shows it in its `public_routes` row.

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_config_fixture_bundle};
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

const ROUTE_ID: &str = "public/front";
const ROUTE_PATH: &str = "/front";
const BODY: &str = "public page";

fn plant_public_route(root: &Path, signer: &SigningKey) -> anyhow::Result<()> {
    use base64::Engine as _;
    let dir = root.join(".ai/node/routes/public");
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        "id: {ROUTE_ID}\npath: {ROUTE_PATH}\nmethods:\n  - GET\nauth: none\nlimits:\n  body_bytes_max: 0\nresponse:\n  mode: static\n  status: 200\n  content_type: \"text/plain; charset=utf-8\"\n  body_b64: {}\npublic: true\n",
        base64::engine::general_purpose::STANDARD.encode(BODY)
    );
    std::fs::write(
        dir.join("front.yaml"),
        lillux::signature::sign_content(&body, signer, "#", None),
    )?;
    Ok(())
}

/// Replace the node-signed `public_routes` section, as
/// `ryeos node policy-apply public_routes` would on a stopped node.
fn write_public_routes(state_path: &Path, node: &SigningKey, names: &[&str]) -> anyhow::Result<()> {
    write_public_routes_with_cap(state_path, node, names, 1_048_576)
}

fn write_public_routes_with_cap(
    state_path: &Path,
    node: &SigningKey,
    names: &[&str],
    daily_bytes: u64,
) -> anyhow::Result<()> {
    let routes: serde_json::Map<String, Value> = names
        .iter()
        .map(|id| {
            (
                (*id).to_owned(),
                json!({ "daily_bytes": daily_bytes, "concurrent_max": 4 }),
            )
        })
        .collect();
    let body = serde_yaml::to_string(&json!({ "schema": 1, "routes": routes }))?;
    std::fs::write(
        state_path.join(".ai/node/policies/public_routes.yaml"),
        lillux::signature::sign_content(&body, node, "#", None),
    )?;
    Ok(())
}

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn fetch(h: &DaemonHarness, path: &str) -> Answer {
    let response = reqwest::get(format!("http://{}{path}", h.bind))
        .await
        .expect("GET");
    let status = response.status().as_u16();
    let mut headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter(|(name, _)| *name != reqwest::header::DATE)
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value.to_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    headers.sort();
    Answer {
        status,
        headers,
        body: response.bytes().await.expect("body").to_vec(),
    }
}

fn doctor_row(h: &DaemonHarness) -> Value {
    let output = std::process::Command::new(common::ryeos_binary())
        .args(["node", "doctor", "--no-bundles", "--json", "--app-root"])
        .arg(&h.state_path)
        .env("RYEOS_APP_ROOT", &h.state_path)
        .env("RYEOSD_BIN", common::ryeosd_binary())
        .env("HOME", h.user_space.path())
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run ryeos node doctor");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "doctor printed no JSON ({error})\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    report["checks"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["name"] == "public_routes"))
        .cloned()
        .unwrap_or_else(|| panic!("doctor has no public_routes row: {report:#}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_public_route_the_policy_stops_naming_is_not_served_and_the_node_says_so() {
    let (mut h, fixture): (DaemonHarness, FastFixture) = DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            register_config_fixture_bundle(state_path, "fixture-public-page", fixture, |root| {
                plant_public_route(root, &fixture.publisher)
            })?;
            write_public_routes(state_path, &fixture.node, &[ROUTE_ID])
        },
        |_| {},
    )
    .await
    .expect("start the node with the public route named");

    // Named: served.
    let served = fetch(&h, ROUTE_PATH).await;
    assert_eq!(served.status, 200);
    assert_eq!(served.body, BODY.as_bytes());

    // The policy stops naming it; the stopped node is resealed over the new
    // policy and restarts.
    h.kill_daemon().await.expect("stop the daemon");
    write_public_routes(&h.state_path, &fixture.node, &[]).expect("apply the new policy");
    common::fast_fixture::seal_initialized_state(&h.state_path)
        .expect("seal the node over the new policy");
    h.respawn_with(|_| {})
        .await
        .expect("the node starts without the route");

    // Its path answers exactly as an unknown path does.
    let left_out = fetch(&h, ROUTE_PATH).await;
    let unknown = fetch(&h, "/no-such-path-anywhere").await;
    assert_eq!(left_out.status, 404);
    assert_eq!(
        (left_out.status, &left_out.headers, &left_out.body),
        (unknown.status, &unknown.headers, &unknown.body),
        "a left-out public route must answer exactly as an unknown path"
    );

    // The rest of the node serves.
    assert_eq!(fetch(&h, "/ui").await.status, 200);

    // The daemon reports it, and doctor shows it.
    let (status, routes) = h
        .post_execute("service:system/routes", ".", json!({}))
        .await
        .expect("system/routes");
    assert_eq!(status.as_u16(), 200, "{routes:#}");
    let skipped = &routes["result"]["skipped_public_routes"];
    assert_eq!(skipped[0]["id"], ROUTE_ID, "{routes:#}");
    assert_eq!(skipped[0]["code"], "public_route_not_named", "{routes:#}");

    let row = doctor_row(&h);
    assert_eq!(row["status"], "warning", "{row:#}");
    assert_eq!(row["detail"]["code"], "public_route_not_named", "{row:#}");
    assert_eq!(row["detail"]["skipped"][0]["id"], ROUTE_ID, "{row:#}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_public_route_answers_429_with_its_code_alone_once_its_day_is_spent() {
    // Room for one page and not two.
    let cap = BODY.len() as u64 + BODY.len() as u64 / 2;
    let (h, _fixture): (DaemonHarness, FastFixture) = DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            register_config_fixture_bundle(state_path, "fixture-public-page", fixture, |root| {
                plant_public_route(root, &fixture.publisher)
            })?;
            write_public_routes_with_cap(state_path, &fixture.node, &[ROUTE_ID], cap)
        },
        |_| {},
    )
    .await
    .expect("start the node with a small public-route cap");

    let first = fetch(&h, ROUTE_PATH).await;
    assert_eq!(first.status, 200);
    assert_eq!(first.body, BODY.as_bytes());

    let refused = fetch(&h, ROUTE_PATH).await;
    assert_eq!(refused.status, 429);
    let body: Value = serde_json::from_slice(&refused.body).expect("a JSON refusal");
    assert_eq!(body, json!({ "code": "public_route_quota_exhausted" }));
    assert!(
        refused
            .headers
            .iter()
            .any(|(name, _)| name == "retry-after"),
        "{:?}",
        refused.headers
    );
    assert!(
        !refused.headers.iter().any(|(name, _)| name == "set-cookie"),
        "{:?}",
        refused.headers
    );

    // Other routes are not limited by the public route's day.
    assert_eq!(fetch(&h, "/ui").await.status, 200);
}
