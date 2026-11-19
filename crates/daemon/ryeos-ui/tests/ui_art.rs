//! The signed-picture route: which hashes a session is served, from which
//! bytes, and what is refused. Pictures are installed into the test engine's
//! fixture bundle (signed records, blobs, signed picture manifest) and served
//! through the route's own decision path.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use ryeos_api::route_error::RouteDispatchError;
use ryeos_api::routes::compile::ResponseMode;
use ryeos_app::state::AppState;
use ryeos_state::objects::{ItemSource, SourceManifest};
use ryeos_ui::art::{ArtMode, ArtRequest, CompiledArtMode};
use ryeos_ui::browser_session::{ArtGrantRefusal, MAX_ART_GRANTS_PER_SESSION};
use ryeos_ui::compiled_binding::{CompiledUiArt, EffectiveUiPosture};
use ryeos_ui::test_support;

/// A real 16x10 lossy WebP.
const WEBP_16X10: [u8; 52] = [
    0x52, 0x49, 0x46, 0x46, 0x2c, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50, 0x56, 0x50, 0x38, 0x20,
    0x20, 0x00, 0x00, 0x00, 0x70, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x10, 0x00, 0x0a, 0x00, 0x01, 0x40,
    0x26, 0x25, 0x94, 0x02, 0x74, 0x01, 0xa0, 0x00, 0x00, 0xf1, 0x3a, 0xbf, 0xbe, 0xa7, 0x41, 0xf1,
    0x5f, 0x00, 0x00, 0x00,
];

/// A real 16x10 PNG.
const PNG_16X10: [u8; 80] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x0a, 0x08, 0x02, 0x00, 0x00, 0x00, 0x32, 0xdc, 0x49,
    0xcb, 0x00, 0x00, 0x00, 0x17, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xac, 0x68, 0xea, 0x61,
    0x20, 0x05, 0x30, 0x91, 0xa4, 0x7a, 0x54, 0xc3, 0xa0, 0xd1, 0x00, 0x00, 0xfc, 0x88, 0x01, 0x9a,
    0x3b, 0x26, 0x8c, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// One picture to install: its bytes, how it is stored and declared.
struct Picture {
    bytes: Vec<u8>,
    extension: &'static str,
    media_type: &'static str,
}

impl Picture {
    fn webp() -> Self {
        Self {
            bytes: WEBP_16X10.to_vec(),
            extension: "webp",
            media_type: "image/webp",
        }
    }

    fn png() -> Self {
        Self {
            bytes: PNG_16X10.to_vec(),
            extension: "png",
            media_type: "image/png",
        }
    }

    fn sha256(&self) -> String {
        lillux::sha256_hex(&self.bytes)
    }
}

fn dev_key() -> lillux::crypto::SigningKey {
    lillux::crypto::load_signing_key(
        &ryeos_engine::test_support::workspace_root().join(".dev-keys/PUBLISHER_DEV.pem"),
    )
    .expect("established development publisher key")
}

fn ai_dir(state: &AppState) -> std::path::PathBuf {
    test_support::fixture_bundle_root(state).join(".ai")
}

fn blob_path(state: &AppState, picture: &Picture) -> std::path::PathBuf {
    ai_dir(state).join(format!(
        "pictures/blobs/{}.{}",
        picture.sha256(),
        picture.extension
    ))
}

/// Install pictures into the fixture bundle the way a published bundle
/// carries them: blobs, signed records, and a signed picture manifest.
fn install(state: &AppState, pictures: &[Picture]) {
    install_at(&ai_dir(state), pictures);
}

fn install_at(ai: &std::path::Path, pictures: &[Picture]) {
    let key = dev_key();
    std::fs::create_dir_all(ai.join("pictures/blobs")).unwrap();
    let cas = lillux::cas::CasStore::new(ai.join("objects"));
    let mut item_source_hashes = HashMap::new();
    for (index, picture) in pictures.iter().enumerate() {
        let sha = picture.sha256();
        std::fs::write(
            ai.join(format!("pictures/blobs/{sha}.{}", picture.extension)),
            &picture.bytes,
        )
        .unwrap();
        let record = format!(
            "title: Test picture\nsource_url: https://example.test/object/{index}\nlicence: cc0\n\
             renditions:\n  - role: card\n    sha256: \"{sha}\"\n    media_type: {}\n    \
             width: 16\n    height: 10\n    bytes: {}\n",
            picture.media_type,
            picture.bytes.len()
        );
        std::fs::create_dir_all(ai.join("pictures/ryeos")).unwrap();
        std::fs::write(
            ai.join(format!("pictures/ryeos/p{index}.yaml")),
            lillux::signature::sign_content(&record, &key, "#", None),
        )
        .unwrap();
        let item_ref = format!("pictures/{sha}");
        let item_source = ItemSource {
            item_ref: item_ref.clone(),
            content_blob_hash: sha.clone(),
            integrity: format!("sha256:{sha}"),
            signature_info: None,
            mode: Some(0o644),
        };
        item_source_hashes.insert(item_ref, cas.store_object(&item_source.to_value()).unwrap());
    }
    let manifest = SourceManifest { item_source_hashes };
    let manifest_hash = cas.store_object(&manifest.to_value()).unwrap();
    let body = format!(
        "{}\n{manifest_hash}\n",
        ryeos_engine::executor_resolution::PICTURE_MANIFEST_REF_DOMAIN
    );
    std::fs::create_dir_all(ai.join("refs/bundles")).unwrap();
    std::fs::write(
        ai.join("refs/bundles/pictures"),
        lillux::signature::sign_content(&body, &key, "#", None),
    )
    .unwrap();
}

/// A live session whose attached binding names exactly `art`.
fn session_naming(state: &AppState, art: &[(&str, &str)]) -> String {
    let mut context = test_support::launch_context(
        "surface:test/art",
        None,
        EffectiveUiPosture::ObservationOnly,
        None,
    );
    {
        let binding = &mut Arc::make_mut(&mut context.compiled_binding).binding;
        for (hash, media_type) in art {
            binding.art.insert(
                (*hash).to_owned(),
                CompiledUiArt {
                    media_type: (*media_type).to_owned(),
                },
            );
        }
    }
    let (session_id, token) = test_support::mint_launch(state, context);
    let ui = ryeos_ui::state::get_ui_state(state).expect("UI state");
    assert_eq!(
        ui.browser_sessions.consume_launch_token(&token),
        Some(session_id.clone())
    );
    session_id
}

fn art_mode(state: &AppState) -> Arc<dyn ryeos_api::routes::compile::CompiledResponseMode> {
    let ui = ryeos_ui::state::get_ui_state(state).expect("UI state");
    let raw = serde_json::from_value(serde_json::json!({
        "id": "ui/art",
        "path": "/ui/art/{sha256}",
        "methods": ["GET"],
        "auth": "browser_session",
        "limits": {"body_bytes_max": 0, "timeout_ms": 5000, "concurrent_max": 4},
        "response": {"mode": "art"}
    }))
    .unwrap();
    ArtMode { ui }.compile(&raw).expect("compile the art route")
}

async fn serve_with(
    mode: &dyn ryeos_api::routes::compile::CompiledResponseMode,
    state: &AppState,
    method: Method,
    headers: HeaderMap,
    principal: &str,
    key: &str,
) -> Result<Response, RouteDispatchError> {
    mode.as_any()
        .downcast_ref::<CompiledArtMode>()
        .expect("the art mode")
        .serve(ArtRequest {
            method,
            headers,
            key: key.to_owned(),
            principal_id: principal.to_owned(),
            engine: state.engine.clone(),
        })
        .await
}

async fn serve(
    mode: &dyn ryeos_api::routes::compile::CompiledResponseMode,
    state: &AppState,
    session_id: &str,
    key: &str,
) -> Result<Response, RouteDispatchError> {
    serve_with(
        mode,
        state,
        Method::GET,
        HeaderMap::new(),
        &format!("session:{session_id}"),
        key,
    )
    .await
}

async fn body_of(response: Response) -> Vec<u8> {
    axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec()
}

fn structured(error: RouteDispatchError) -> (String, u16) {
    match error {
        RouteDispatchError::Structured { code, status, .. } => (code, status),
        other => panic!("expected a structured error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_hash_the_sessions_binding_names_is_served_with_the_signed_headers() {
    let (_tmp, _store, state) = test_support::build_test_state_with_live_bundles();
    let webp = Picture::webp();
    install(&state, std::slice::from_ref(&webp));
    let sha = webp.sha256();
    let session = session_naming(&state, &[(&sha, "image/webp")]);
    let mode = art_mode(&state);

    let response = serve(mode.as_ref(), &state, &session, &sha).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    let text = |name: header::HeaderName| headers.get(name).unwrap().to_str().unwrap().to_owned();
    assert_eq!(text(header::CONTENT_TYPE), "image/webp");
    assert_eq!(text(header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert_eq!(
        text(header::CONTENT_SECURITY_POLICY),
        "default-src 'none'; sandbox"
    );
    assert_eq!(
        text(header::CACHE_CONTROL),
        "private, max-age=31536000, immutable"
    );
    assert_eq!(text(header::ETAG), format!("\"{sha}\""));
    assert_eq!(text(header::CONTENT_LENGTH), WEBP_16X10.len().to_string());
    assert_eq!(text(header::ACCEPT_RANGES), "none");
    assert_eq!(body_of(response).await, WEBP_16X10.to_vec());

    // A caller that already holds the bytes is told so, without a body.
    let mut revalidate = HeaderMap::new();
    revalidate.insert(
        header::IF_NONE_MATCH,
        HeaderValue::from_str(&format!("\"{sha}\"")).unwrap(),
    );
    let response = serve_with(
        mode.as_ref(),
        &state,
        Method::GET,
        revalidate,
        &format!("session:{session}"),
        &sha,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(body_of(response).await.is_empty());
}

#[tokio::test]
async fn a_hash_only_another_sessions_binding_names_is_a_404_for_this_session() {
    let (_tmp, _store, state) = test_support::build_test_state_with_live_bundles();
    let (webp, png) = (Picture::webp(), Picture::png());
    install(&state, &[Picture::webp(), Picture::png()]);
    let (webp_sha, png_sha) = (webp.sha256(), png.sha256());
    let first = session_naming(&state, &[(&webp_sha, "image/webp")]);
    let second = session_naming(&state, &[(&png_sha, "image/png")]);
    let nothing = session_naming(&state, &[]);
    let mode = art_mode(&state);

    assert!(
        serve(mode.as_ref(), &state, &first, &webp_sha)
            .await
            .is_ok()
    );
    assert!(
        serve(mode.as_ref(), &state, &second, &png_sha)
            .await
            .is_ok()
    );
    for (session, hash) in [
        (&first, &png_sha),
        (&second, &webp_sha),
        (&nothing, &webp_sha),
        (&nothing, &png_sha),
    ] {
        assert!(matches!(
            serve(mode.as_ref(), &state, session, hash).await,
            Err(RouteDispatchError::NotFound)
        ));
    }

    // An installed hash no binding names and a hash nothing installs are the
    // same answer.
    let absent = "ab".repeat(32);
    let unnamed = serve(mode.as_ref(), &state, &nothing, &webp_sha)
        .await
        .unwrap_err();
    let unknown = serve(mode.as_ref(), &state, &nothing, &absent)
        .await
        .unwrap_err();
    assert_eq!(format!("{unnamed:?}"), format!("{unknown:?}"));

    // No session at all is not a 404.
    assert!(matches!(
        serve(mode.as_ref(), &state, "no-such-session", &webp_sha).await,
        Err(RouteDispatchError::Unauthorized)
    ));
    assert!(matches!(
        serve_with(
            mode.as_ref(),
            &state,
            Method::GET,
            HeaderMap::new(),
            "fp:someone",
            &webp_sha
        )
        .await,
        Err(RouteDispatchError::Unauthorized)
    ));
}

#[tokio::test]
async fn path_traversal_and_malformed_keys_are_a_404_and_never_read_a_file() {
    let (_tmp, _store, state) = test_support::build_test_state_with_live_bundles();
    let webp = Picture::webp();
    install(&state, std::slice::from_ref(&webp));
    let sha = webp.sha256();
    let session = session_naming(&state, &[(&sha, "image/webp")]);
    let mode = art_mode(&state);

    let upper = sha.to_uppercase();
    let with_extension = format!("{sha}.webp");
    let with_slash = format!("blobs/{sha}");
    let traversal = format!("../../{sha}");
    let short = sha[..63].to_owned();
    let nul = format!("{}\0", &sha[..63]);
    for key in [
        "",
        "..",
        "../../etc/passwd",
        "%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "....//....//etc/passwd",
        "/etc/passwd",
        upper.as_str(),
        with_extension.as_str(),
        with_slash.as_str(),
        traversal.as_str(),
        short.as_str(),
        nul.as_str(),
    ] {
        assert!(
            matches!(
                serve(mode.as_ref(), &state, &session, key).await,
                Err(RouteDispatchError::NotFound)
            ),
            "key {key:?}"
        );
    }
}

#[tokio::test]
async fn only_get_is_answered_and_other_methods_are_a_405() {
    let (_tmp, _store, state) = test_support::build_test_state_with_live_bundles();
    let webp = Picture::webp();
    install(&state, std::slice::from_ref(&webp));
    let sha = webp.sha256();
    let session = session_naming(&state, &[(&sha, "image/webp")]);
    let mode = art_mode(&state);
    for method in [Method::POST, Method::HEAD, Method::DELETE, Method::PUT] {
        let error = serve_with(
            mode.as_ref(),
            &state,
            method,
            HeaderMap::new(),
            &format!("session:{session}"),
            &sha,
        )
        .await
        .unwrap_err();
        assert_eq!(structured(error).1, 405);
    }
}

#[tokio::test]
async fn a_binding_that_declares_another_media_type_than_the_signed_record_is_a_404() {
    let (_tmp, _store, state) = test_support::build_test_state_with_live_bundles();
    let webp = Picture::webp();
    install(&state, std::slice::from_ref(&webp));
    let sha = webp.sha256();
    let session = session_naming(&state, &[(&sha, "image/png")]);
    assert!(matches!(
        serve(art_mode(&state).as_ref(), &state, &session, &sha).await,
        Err(RouteDispatchError::NotFound)
    ));
}

#[tokio::test]
async fn the_grant_set_serves_what_no_binding_names_and_is_per_session_and_bounded() {
    let (_tmp, _store, state) = test_support::build_test_state_with_live_bundles();
    let webp = Picture::webp();
    install(&state, std::slice::from_ref(&webp));
    let sha = webp.sha256();
    let granted = session_naming(&state, &[]);
    let other = session_naming(&state, &[]);
    let mode = art_mode(&state);
    let ui = ryeos_ui::state::get_ui_state(&state).unwrap();
    let sessions = &ui.browser_sessions;

    assert!(matches!(
        serve(mode.as_ref(), &state, &granted, &sha).await,
        Err(RouteDispatchError::NotFound)
    ));
    sessions.grant_art(&granted, &sha).unwrap();
    let response = serve(mode.as_ref(), &state, &granted, &sha).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(matches!(
        serve(mode.as_ref(), &state, &other, &sha).await,
        Err(RouteDispatchError::NotFound)
    ));

    assert_eq!(
        sessions.grant_art(&granted, "../../etc/passwd"),
        Err(ArtGrantRefusal::MalformedHash)
    );
    assert_eq!(
        sessions.grant_art("no-such-session", &sha),
        Err(ArtGrantRefusal::UnknownSession)
    );
    // A grant of a hash nothing installs is accepted into the set but serves
    // nothing.
    let absent = "cd".repeat(32);
    sessions.grant_art(&granted, &absent).unwrap();
    assert!(matches!(
        serve(mode.as_ref(), &state, &granted, &absent).await,
        Err(RouteDispatchError::NotFound)
    ));
    for index in 0..MAX_ART_GRANTS_PER_SESSION {
        let hash = format!("{index:064x}");
        let outcome = sessions.grant_art(&granted, &hash);
        if outcome.is_err() {
            assert_eq!(outcome, Err(ArtGrantRefusal::SetFull));
            return;
        }
    }
    panic!("the grant set must refuse once it holds {MAX_ART_GRANTS_PER_SESSION} hashes");
}

#[tokio::test]
async fn bytes_that_are_not_the_declared_image_are_refused() {
    let (_tmp, _store, state) = test_support::build_test_state_with_live_bundles();
    // A VP8X header with the animation flag set.
    let mut animated = Vec::new();
    animated.extend_from_slice(b"RIFF");
    animated.extend_from_slice(&22u32.to_le_bytes());
    animated.extend_from_slice(b"WEBPVP8X");
    animated.extend_from_slice(&10u32.to_le_bytes());
    animated.extend_from_slice(&[0x02, 0, 0, 0, 15, 0, 0, 9, 0, 0]);
    let cases = [
        (
            "webp bytes declared as png",
            Picture {
                bytes: WEBP_16X10.to_vec(),
                extension: "png",
                media_type: "image/png",
            },
        ),
        (
            "png bytes declared as webp",
            Picture {
                bytes: PNG_16X10.to_vec(),
                extension: "webp",
                media_type: "image/webp",
            },
        ),
        (
            "animated webp",
            Picture {
                bytes: animated,
                extension: "webp",
                media_type: "image/webp",
            },
        ),
        (
            "svg declared as png",
            Picture {
                bytes: b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"10\"/>"
                    .to_vec(),
                extension: "png",
                media_type: "image/png",
            },
        ),
        (
            "gif declared as webp",
            Picture {
                bytes: b"GIF89a\x10\x00\x0a\x00\x00\x00\x00;".to_vec(),
                extension: "webp",
                media_type: "image/webp",
            },
        ),
    ];
    let pictures: Vec<Picture> = cases
        .iter()
        .map(|(_, picture)| Picture {
            bytes: picture.bytes.clone(),
            extension: picture.extension,
            media_type: picture.media_type,
        })
        .collect();
    install(&state, &pictures);
    let mode = art_mode(&state);
    for (name, picture) in &cases {
        let sha = picture.sha256();
        let session = session_naming(&state, &[(&sha, picture.media_type)]);
        let error = serve(mode.as_ref(), &state, &session, &sha)
            .await
            .expect_err(name);
        assert_eq!(structured(error), ("art_refused".to_owned(), 500), "{name}");
    }
}

#[tokio::test]
async fn bytes_changed_after_signing_are_an_integrity_failure() {
    let (_tmp, _store, state) = test_support::build_test_state_with_live_bundles();
    let webp = Picture::webp();
    install(&state, std::slice::from_ref(&webp));
    let sha = webp.sha256();
    let session = session_naming(&state, &[(&sha, "image/webp")]);
    let mode = art_mode(&state);
    assert!(serve(mode.as_ref(), &state, &session, &sha).await.is_ok());

    // One byte changed, same length.
    let path = blob_path(&state, &webp);
    let mut tampered = WEBP_16X10.to_vec();
    tampered[30] ^= 1;
    std::fs::write(&path, &tampered).unwrap();
    let error = serve(mode.as_ref(), &state, &session, &sha)
        .await
        .unwrap_err();
    assert_eq!(structured(error), ("art_integrity".to_owned(), 500));

    // Replaced by a file over the size cap.
    std::fs::write(&path, vec![0u8; 2 * 1024 * 1024 + 1]).unwrap();
    let error = serve(mode.as_ref(), &state, &session, &sha)
        .await
        .unwrap_err();
    assert_eq!(structured(error), ("art_integrity".to_owned(), 500));

    // Restored, it is served again.
    std::fs::write(&path, WEBP_16X10).unwrap();
    assert!(serve(mode.as_ref(), &state, &session, &sha).await.is_ok());
}

#[tokio::test]
async fn pictures_without_a_verifying_signed_manifest_are_not_served() {
    let (_tmp, _store, state) = test_support::build_test_state_with_live_bundles();
    let webp = Picture::webp();
    install(&state, std::slice::from_ref(&webp));
    let sha = webp.sha256();
    let session = session_naming(&state, &[(&sha, "image/webp")]);
    let mode = art_mode(&state);
    assert!(serve(mode.as_ref(), &state, &session, &sha).await.is_ok());

    // The signed ref names a different manifest: nothing verifies.
    let ref_path = ai_dir(&state).join("refs/bundles/pictures");
    let signed = std::fs::read_to_string(&ref_path).unwrap();
    std::fs::write(
        &ref_path,
        signed.replace("ryeos:bundle-picture-manifest", "x"),
    )
    .unwrap();
    assert!(matches!(
        serve(mode.as_ref(), &state, &session, &sha).await,
        Err(RouteDispatchError::NotFound)
    ));

    // The ref gone entirely, with the pictures still on disk.
    std::fs::remove_file(&ref_path).unwrap();
    assert!(matches!(
        serve(mode.as_ref(), &state, &session, &sha).await,
        Err(RouteDispatchError::NotFound)
    ));
}

#[tokio::test]
async fn a_record_over_the_size_cap_keeps_the_whole_bundle_out_of_the_index() {
    let (_tmp, _store, state) = test_support::build_test_state_with_live_bundles();
    let big = Picture {
        bytes: vec![0u8; 2 * 1024 * 1024 + 1],
        extension: "webp",
        media_type: "image/webp",
    };
    let webp = Picture::webp();
    install(&state, &[Picture::webp(), big]);
    let sha = webp.sha256();
    let session = session_naming(&state, &[(&sha, "image/webp")]);
    assert!(matches!(
        serve(art_mode(&state).as_ref(), &state, &session, &sha).await,
        Err(RouteDispatchError::NotFound)
    ));
}

#[test]
fn the_route_compiles_only_for_browser_session_get_with_the_hash_capture() {
    let (_tmp, _store, state) = test_support::build_test_state_with_live_bundles();
    let ui = ryeos_ui::state::get_ui_state(&state).expect("UI state");
    let compile = |auth: &str, methods: serde_json::Value, path: &str| {
        let raw = serde_json::from_value(serde_json::json!({
            "id": "ui/art", "path": path, "methods": methods, "auth": auth,
            "response": {"mode": "art"}
        }))
        .unwrap();
        ArtMode { ui: ui.clone() }.compile(&raw).map(|_| ())
    };
    assert!(
        compile(
            "browser_session",
            serde_json::json!(["GET"]),
            "/ui/art/{sha256}"
        )
        .is_ok()
    );
    assert!(compile("none", serde_json::json!(["GET"]), "/ui/art/{sha256}").is_err());
    assert!(
        compile(
            "browser_session",
            serde_json::json!(["GET", "POST"]),
            "/ui/art/{sha256}"
        )
        .is_err()
    );
    assert!(
        compile(
            "browser_session",
            serde_json::json!(["POST"]),
            "/ui/art/{sha256}"
        )
        .is_err()
    );
    assert!(
        compile(
            "browser_session",
            serde_json::json!(["GET"]),
            "/ui/art/{name}"
        )
        .is_err()
    );
}

#[test]
fn a_corrupt_copy_in_one_bundle_falls_through_to_a_good_copy_in_another() {
    let webp = Picture::webp();
    let sha = webp.sha256();
    let tmp = tempfile::tempdir().unwrap();
    let (first, second) = (tmp.path().join("a"), tmp.path().join("b"));
    for root in [&first, &second] {
        std::fs::create_dir_all(root.join(".ai/pictures/blobs")).unwrap();
        install_at(&root.join(".ai"), std::slice::from_ref(&webp));
    }
    let trust = ryeos_engine::test_support::live_trust_store();
    let roots = [first.clone(), second.clone()];
    let index = ryeos_ui::art::ArtIndex::build(&roots, &trust);
    assert_eq!(index.candidates(&sha).len(), 2);

    // Same length, one byte changed, in the earlier bundle.
    let corrupt = first.join(format!(".ai/pictures/blobs/{sha}.webp"));
    let mut bytes = WEBP_16X10.to_vec();
    bytes[30] ^= 1;
    std::fs::write(&corrupt, bytes).unwrap();
    let body = ryeos_ui::art::read_art(&index, &sha, Some("image/webp")).unwrap();
    assert_eq!(body.bytes, WEBP_16X10.to_vec());

    // With no good copy anywhere the integrity failure is returned.
    std::fs::write(second.join(format!(".ai/pictures/blobs/{sha}.webp")), [0u8; 52]).unwrap();
    assert!(matches!(
        ryeos_ui::art::read_art(&index, &sha, Some("image/webp")),
        Err(ryeos_ui::art::ArtFailure::Integrity(_))
    ));
}
