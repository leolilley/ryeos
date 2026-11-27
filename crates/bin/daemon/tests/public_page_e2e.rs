//! The shipped `ryeos-public` page serves at `/`.
//!
//! `ryeos-public` is in no shipped bundle set, and the test harness refuses to
//! register a published bundle no shipped profile selects, so the page's signed
//! route item is copied byte for byte into a synthetic bundle. On a node whose
//! `public_routes` policy names the page's route, `GET /`
//! with no key answers the signed page, and the response carries exactly the
//! headers a static public page sends: nothing names the node, its operator,
//! its version or a principal, and no cookie is set. `HEAD /` answers the same
//! headers with no body. A node without the bundle answers `/` as an unknown
//! path.

use ryeosd::test_support as common;

use std::path::Path;

use common::DaemonHarness;
use common::fast_fixture::{FAST_FIXTURE_TIME, FastFixture, register_presigned_fixture_bundle};
use lillux::crypto::SigningKey;
use serde_json::json;

const FRONT: &str = "public/front";

const FRONT_ITEM: &str = ".ai/node/routes/public/front.yaml";

/// A synthetic bundle carrying the shipped, signed page route item unchanged.
fn plant_shipped_page(state_path: &Path, fixture: &FastFixture) -> anyhow::Result<()> {
    let bundle = state_path.join(".ai/bundles/public-page-fixture");
    let item = bundle.join(FRONT_ITEM);
    std::fs::create_dir_all(item.parent().expect("the item has a parent"))?;
    std::fs::copy(
        common::workspace_root()
            .join("bundles/ryeos-public")
            .join(FRONT_ITEM),
        &item,
    )?;
    let manifest = "name: public-page-fixture\nversion: 1.0.0\ndescription: the shipped public page route\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n";
    std::fs::write(
        bundle.join(".ai/manifest.yaml"),
        lillux::signature::sign_content_at(
            manifest,
            &fixture.publisher,
            "#",
            None,
            FAST_FIXTURE_TIME,
        ),
    )?;
    register_presigned_fixture_bundle(state_path, "public-page-fixture", &bundle, fixture)
}

fn name_front_route(state_path: &Path, node: &SigningKey) -> anyhow::Result<()> {
    let body = serde_yaml::to_string(&json!({
        "schema": 1,
        "routes": { FRONT: { "daily_bytes": 1_048_576, "concurrent_max": 8 } },
    }))?;
    std::fs::write(
        state_path.join(".ai/node/policies/public_routes.yaml"),
        lillux::signature::sign_content(&body, node, "#", None),
    )?;
    Ok(())
}

async fn request(
    h: &DaemonHarness,
    method: reqwest::Method,
    path: &str,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let response = reqwest::Client::new()
        .request(method, format!("http://{}{path}", h.bind))
        .send()
        .await
        .expect("request");
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
    let body = response.bytes().await.expect("body").to_vec();
    (status, headers, body)
}

fn header_names(headers: &[(String, String)]) -> Vec<&str> {
    headers.iter().map(|(name, _)| name.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_public_page_serves_at_the_root_with_exactly_its_headers() {
    let (h, _fixture): (DaemonHarness, FastFixture) = DaemonHarness::start_fast_with(
        |state_path, _, fixture| {
            plant_shipped_page(state_path, fixture)?;
            name_front_route(state_path, &fixture.node)
        },
        |_| {},
    )
    .await
    .expect("start a node with the public page");

    let (status, headers, body) = request(&h, reqwest::Method::GET, "/").await;
    assert_eq!(status, 200, "{headers:?}");
    let page = String::from_utf8(body).expect("utf-8 page");
    assert!(page.contains("<h1>RyeOS</h1>"), "{page}");

    // Exactly these headers, and nothing else (Date aside).
    assert_eq!(
        header_names(&headers),
        [
            "accept-ranges",
            "cache-control",
            "content-length",
            "content-security-policy",
            "content-type",
            "etag",
            "referrer-policy",
            "x-content-type-options",
        ],
        "{headers:?}"
    );
    let value = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap()
    };
    assert_eq!(value("content-type"), "text/html; charset=utf-8");
    assert_eq!(value("cache-control"), "no-cache");
    assert_eq!(value("x-content-type-options"), "nosniff");
    assert_eq!(value("referrer-policy"), "same-origin");
    assert_eq!(value("accept-ranges"), "bytes");
    assert_eq!(value("content-length"), page.len().to_string());

    // HEAD answers the same headers with no body.
    let (status, head_headers, head_body) = request(&h, reqwest::Method::HEAD, "/").await;
    assert_eq!(status, 200);
    assert!(head_body.is_empty());
    assert_eq!(header_names(&head_headers), header_names(&headers));

    // A node without the bundle answers the root as an unknown path.
    let (bare, _bare_fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {})
        .await
        .expect("start a node without the public page");
    let (status, _, _) = request(&bare, reqwest::Method::GET, "/").await;
    assert_eq!(status, 404);
}
