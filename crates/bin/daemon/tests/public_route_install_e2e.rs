//! A bundle whose routes are signed public installs only when the node's
//! current `public_routes` policy names every one of them.
//!
//! On a stopped node whose policy names no public route (every shipped
//! profile), installing a bundle that carries a public route is refused with
//! `public_route_not_named`, the route named in the refusal, and nothing is
//! registered.

use ryeosd::test_support as common;

use common::StandaloneHarness;

const BUNDLE: &str = "publicpage";

fn write_public_bundle_source(harness: &StandaloneHarness) -> tempfile::TempDir {
    write_public_bundle_source_with_auth(harness, "none")
}

fn write_public_bundle_source_with_auth(
    harness: &StandaloneHarness,
    auth: &str,
) -> tempfile::TempDir {
    use base64::Engine as _;
    let sign = |body: &str| {
        lillux::signature::sign_content_at(
            body,
            &harness.fixture.publisher,
            "#",
            None,
            common::fast_fixture::FAST_FIXTURE_TIME,
        )
    };
    let src = tempfile::tempdir().expect("src tempdir");
    let routes = src.path().join(".ai/node/routes/public");
    std::fs::create_dir_all(&routes).unwrap();
    let route = format!(
        "id: public/front\npath: /front\nmethods:\n  - GET\nauth: {auth}\nlimits:\n  body_bytes_max: 0\nresponse:\n  mode: static\n  status: 200\n  content_type: \"text/plain; charset=utf-8\"\n  body_b64: {}\npublic: true\n",
        base64::engine::general_purpose::STANDARD.encode("public page")
    );
    std::fs::write(routes.join("front.yaml"), sign(&route)).unwrap();
    let manifest = format!(
        "name: {BUNDLE}\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds:\n  - config\nuses_kinds: []\n"
    );
    std::fs::write(src.path().join(".ai/manifest.yaml"), sign(&manifest)).unwrap();
    src
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_with_a_public_route_the_policy_does_not_name_is_refused() {
    let harness = StandaloneHarness::new_initialized_authorizing(&[BUNDLE])
        .await
        .expect("standalone harness init");
    let src = write_public_bundle_source(&harness);
    let src_path = src.path().to_str().unwrap().to_string();

    let refused = harness
        .run_service(
            "service:bundle/install",
            Some(&format!(
                r#"{{"name":"{BUNDLE}","source_path":"{src_path}"}}"#
            )),
        )
        .await
        .expect("install spawn");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success(),
        "the install was not refused:\n{stderr}"
    );
    assert!(
        stderr.contains("public_route_not_named"),
        "typed refusal:\n{stderr}"
    );
    assert!(stderr.contains("public/front"), "route named:\n{stderr}");
    assert!(
        !harness
            .app_root
            .join(".ai/node/bundles")
            .join(format!("{BUNDLE}.yaml"))
            .exists(),
        "a refused install registered the bundle"
    );
    assert!(
        !harness.app_root.join(".ai/bundles").join(BUNDLE).exists(),
        "a refused install left the bundle in the bundle root"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_whose_public_route_is_keyed_is_refused_by_name() {
    let harness = StandaloneHarness::new_initialized_authorizing(&[BUNDLE])
        .await
        .expect("standalone harness init");
    let src = write_public_bundle_source_with_auth(&harness, "browser_session");
    let src_path = src.path().to_str().unwrap().to_string();

    let refused = harness
        .run_service(
            "service:bundle/install",
            Some(&format!(
                r#"{{"name":"{BUNDLE}","source_path":"{src_path}"}}"#
            )),
        )
        .await
        .expect("install spawn");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success(),
        "the install was not refused:\n{stderr}"
    );
    assert!(
        stderr.contains("public_route_not_keyless_static"),
        "typed refusal:\n{stderr}"
    );
    assert!(stderr.contains("public/front"), "route named:\n{stderr}");
    assert!(
        !harness
            .app_root
            .join(".ai/node/bundles")
            .join(format!("{BUNDLE}.yaml"))
            .exists(),
        "a refused install registered the bundle"
    );
}
