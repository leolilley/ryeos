//! End to end through the CLI: an operator runs `ryeos publish` on one node
//! and `ryeos pull` on another.
//!
//! Every case drives the built `ryeos` binary, the way an operator does, and
//! reads what it prints and how it exits. The success case stages the bundle.
//! Each refusal exits non-zero, names its typed code, and stages nothing:
//! a retired publication, an expired one, a publisher whose daily read cap is
//! spent, a node of another release series, and a non-https URL.
//!
//! The publisher's own refusals (retired, expired, quota) reach the puller as
//! `publication_fetch_failed` carrying the publisher's code, because the pull
//! reports the node's answer to the fetch it made.

use ryeosd::test_support as common;

use std::path::Path;
use std::process::Output;

use common::DaemonHarness;
use common::fast_fixture::{FAST_FIXTURE_TIME, FastFixture, register_workflow_bundles};
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

/// A bundle with no command registration, which the publisher installs.
const BUNDLE: &str = "ryeos-docs";

fn ryeos(h: &DaemonHarness, args: &[&str]) -> Output {
    std::process::Command::new(common::ryeos_binary())
        .args(args)
        .arg("--json")
        .arg("--app-root")
        .arg(&h.state_path)
        .env("RYEOS_APP_ROOT", &h.state_path)
        .env("RYEOSD_BIN", common::ryeosd_binary())
        .env("HOME", h.user_space.path())
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run ryeos")
}

fn printed(output: &Output) -> String {
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The JSON a successful command printed.
fn succeeded(output: &Output, what: &str) -> Value {
    assert!(
        output.status.success(),
        "{what} failed: {}",
        printed(output)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{what} printed no JSON ({error}): {}", printed(output)))
}

/// A field of the command's answer, whether the CLI printed the service's
/// result alone or the whole `/execute` answer around it.
fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    match value.get(name) {
        Some(found) => found,
        None => &value["result"][name],
    }
}

/// The command was refused: it exits non-zero and names every code given.
fn assert_refused(output: &Output, codes: &[&str]) {
    assert!(
        !output.status.success(),
        "the pull should have been refused: {}",
        printed(output)
    );
    let text = printed(output);
    for code in codes {
        assert!(
            text.contains(code),
            "the refusal does not name {code}: {text}"
        );
    }
}

/// Nothing was staged, installed or registered on the pulling node.
fn assert_nothing_staged(b: &DaemonHarness) {
    let staging = b.state_path.join(".ai/state/bundle-staging");
    let stages: Vec<String> = std::fs::read_dir(&staging)
        .map(|read| {
            read.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(stages.is_empty(), "a refused pull left a stage: {stages:?}");
    assert!(
        !b.state_path.join(".ai/bundles").join(BUNDLE).exists(),
        "a refused pull left a bundle behind"
    );
    assert!(
        !b.state_path
            .join(".ai/node/bundles")
            .join(format!("{BUNDLE}.yaml"))
            .exists(),
        "a refused pull registered a bundle"
    );
}

async fn start_publisher() -> (DaemonHarness, FastFixture) {
    start_publisher_with(|_, _| Ok(())).await
}

async fn start_publisher_with<F>(plant: F) -> (DaemonHarness, FastFixture)
where
    F: FnOnce(&Path, &FastFixture) -> anyhow::Result<()>,
{
    DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            register_workflow_bundles(state_path, fixture)?;
            plant(state_path, fixture)
        },
        |_| {},
    )
    .await
    .expect("start the publishing node")
}

/// A second node with its own node and operator keys and only the
/// Core-knowledge profile: it has never seen the published bundle.
async fn start_puller() -> (DaemonHarness, FastFixture) {
    DaemonHarness::start_fast_core_knowledge_with_node_and_user_keys(
        SigningKey::from_bytes(&[0x6b; 32]),
        SigningKey::from_bytes(&[0x6c; 32]),
    )
    .await
    .expect("start the pulling node")
}

/// What `ryeos publish` hands the operator to share.
struct Shared {
    id: String,
    url: String,
    node: String,
    expect: String,
}

fn publish(a: &DaemonHarness, not_after: &str) -> Shared {
    let published = succeeded(
        &ryeos(a, &["publish", BUNDLE, "--not-after", not_after]),
        "ryeos publish",
    );
    let share = field(&published, "share");
    let text = |value: &Value, name: &str| {
        value[name]
            .as_str()
            .unwrap_or_else(|| panic!("publish printed no {name}: {published:#}"))
            .to_owned()
    };
    Shared {
        id: field(&published, "id")
            .as_str()
            .unwrap_or_else(|| panic!("publish printed no id: {published:#}"))
            .to_owned(),
        url: format!("http://{}{}", a.bind, text(share, "path")),
        node: text(share, "node"),
        expect: text(share, "expect"),
    }
}

fn instant(unix_secs: u64) -> String {
    lillux::time::iso8601_from_unix_secs(unix_secs)
}

fn an_hour_from_now() -> String {
    instant(lillux::time::unix_secs_now() + 3600)
}

fn pull(b: &DaemonHarness, url: &str, shared: &Shared) -> Output {
    ryeos(
        b,
        &[
            "pull",
            url,
            "--node",
            &shared.node,
            "--expect",
            &shared.expect,
        ],
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn ryeos_pull_stages_what_ryeos_publish_shared() {
    let (a, _a_fixture) = start_publisher().await;
    let (b, _b_fixture) = start_puller().await;
    let shared = publish(&a, &an_hour_from_now());

    let staged = succeeded(&pull(&b, &shared.url, &shared), "ryeos pull");
    assert_eq!(field(&staged, "status"), "staged", "{staged:#}");
    assert_eq!(field(&staged, "installed"), false, "{staged:#}");
    assert_eq!(
        field(&staged, "receipt"),
        "fetched and verified; staged; takes effect at the next start by a signed operation",
        "{staged:#}"
    );
    let digest = field(&staged, "tree_digest")
        .as_str()
        .unwrap_or_else(|| panic!("no tree digest: {staged:#}"))
        .to_owned();
    assert!(
        b.state_path
            .join(".ai/state/bundle-staging")
            .join(&digest)
            .join("stage.record")
            .is_file(),
        "the pull printed a stage that is not on disk"
    );

    // `ryeos bundle list` shows it staged and not installed.
    let listed = succeeded(&ryeos(&b, &["bundle", "list"]), "ryeos bundle list");
    let staged_entries = field(&listed, "staged")
        .as_array()
        .unwrap_or_else(|| panic!("bundle list shows no staged list: {listed:#}"));
    let entry = staged_entries
        .iter()
        .find(|entry| entry["name"] == BUNDLE)
        .unwrap_or_else(|| panic!("the stage is not listed: {listed:#}"));
    assert_eq!(entry["status"], "staged", "{listed:#}");
    assert_eq!(entry["tree_digest"], digest, "{listed:#}");
    assert!(
        !field(&listed, "bundles")
            .as_array()
            .into_iter()
            .flatten()
            .any(|entry| entry["name"] == BUNDLE),
        "a staged bundle is listed as installed: {listed:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ryeos_pull_of_a_retired_publication_is_refused_and_stages_nothing() {
    let (a, _a_fixture) = start_publisher().await;
    let (b, _b_fixture) = start_puller().await;
    let shared = publish(&a, &an_hour_from_now());
    succeeded(
        &ryeos(&a, &["publication", "retire", "--id", &shared.id]),
        "ryeos publication retire",
    );

    assert_refused(
        &pull(&b, &shared.url, &shared),
        &["publication_fetch_failed", "publication_retired"],
    );
    assert_nothing_staged(&b);
}

#[tokio::test(flavor = "multi_thread")]
async fn ryeos_pull_of_an_expired_publication_is_refused_and_stages_nothing() {
    let (a, _a_fixture) = start_publisher().await;
    let (b, _b_fixture) = start_puller().await;
    // Expiry is real time: publish with a not_after a little ahead, and pull
    // only once it has passed, so the case has no race.
    let not_after = lillux::time::unix_secs_now() + 15;
    let shared = publish(&a, &instant(not_after));
    while lillux::time::unix_secs_now() <= not_after + 1 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    assert_refused(
        &pull(&b, &shared.url, &shared),
        &["publication_fetch_failed", "publication_expired"],
    );
    assert_nothing_staged(&b);
}

/// Rewrite the node-signed publication policy so the day's read cap is
/// `bytes`, before the init completion seals the generation.
fn set_daily_cap(state_path: &Path, node: &SigningKey, bytes: u64) -> anyhow::Result<()> {
    let path = state_path.join(".ai/node/policies/publication.yaml");
    let raw = std::fs::read_to_string(&path)?;
    let body = lillux::signature::strip_signature_lines(&raw);
    let mut policy: serde_yaml::Value = serde_yaml::from_str(&body)?;
    policy["daily_bytes"] = bytes.into();
    let signed = lillux::signature::sign_content_at(
        &serde_yaml::to_string(&policy)?,
        node,
        "#",
        None,
        FAST_FIXTURE_TIME,
    );
    std::fs::write(&path, signed)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn ryeos_pull_from_a_publisher_whose_day_is_spent_is_refused_and_stages_nothing() {
    // A one-byte cap: every read of `/published` is over it.
    let (a, _a_fixture) =
        start_publisher_with(|state_path, fixture| set_daily_cap(state_path, &fixture.node, 1))
            .await;
    let (b, _b_fixture) = start_puller().await;
    let shared = publish(&a, &an_hour_from_now());

    assert_refused(
        &pull(&b, &shared.url, &shared),
        &["publication_fetch_failed", "publication_quota_exhausted"],
    );
    assert_nothing_staged(&b);
}

/// Serve the publisher's own `/public-key` answer with its version changed to
/// another release series, on a loopback port of its own. Only the version
/// differs from what the real node says.
async fn serve_public_key_of_another_release(a: &DaemonHarness) -> std::net::SocketAddr {
    let mut doc: Value = reqwest::get(format!("http://{}/public-key", a.bind))
        .await
        .expect("GET the publisher's /public-key")
        .json()
        .await
        .expect("the publisher's /public-key is JSON");
    doc["version"] = json!("99.0.0");
    let app = axum::Router::new().route(
        "/public-key",
        axum::routing::get(move || {
            let doc = doc.clone();
            async move { axum::Json(doc) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the stub");
    let addr = listener.local_addr().expect("stub address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve the stub");
    });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn ryeos_pull_from_a_node_of_another_release_series_is_refused_and_stages_nothing() {
    let (a, _a_fixture) = start_publisher().await;
    let (b, _b_fixture) = start_puller().await;
    let shared = publish(&a, &an_hour_from_now());
    let stub = serve_public_key_of_another_release(&a).await;

    assert_refused(
        &pull(
            &b,
            &format!("http://{stub}/published/{}", shared.id),
            &shared,
        ),
        &["publication_version_mismatch", "99.0.0"],
    );
    assert_nothing_staged(&b);
}

#[tokio::test(flavor = "multi_thread")]
async fn ryeos_pull_of_a_url_that_is_not_https_is_refused_and_stages_nothing() {
    let (a, _a_fixture) = start_publisher().await;
    let (b, _b_fixture) = start_puller().await;
    let shared = publish(&a, &an_hour_from_now());

    assert_refused(
        &pull(
            &b,
            &format!("http://example.com/published/{}", shared.id),
            &shared,
        ),
        &["publication_insecure_url"],
    );
    assert_nothing_staged(&b);
}
