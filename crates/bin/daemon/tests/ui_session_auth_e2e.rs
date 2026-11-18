//! End-to-end browser-session auth over real HTTP through the daemon's route
//! auth layer.
//!
//! One session goes through its whole authority lifecycle:
//!
//!   * the configured operator mints a launch (signed request) and the
//!     browser activates it through the launch URL, receiving the
//!     `ryeos_session` cookie;
//!   * the cookie authenticates the session routes, and a presentation
//!     producer is claimed through the seat route;
//!   * a dispatch naming a source inside the session's compiled binding is
//!     admitted (200);
//!   * a dispatch naming a source outside the compiled binding is refused
//!     (403) by the same route;
//!   * the operator revokes the session (signed request);
//!   * the next cookie request, on any session route, is answered 401 with
//!     the stable code `browser_session_revoked`, not the plain unauthorized
//!     answer an unknown cookie gets.
//!
//! The in-process tests in `ryeos-ui` drive the verifier and the handlers
//! directly; this test is the one that proves the same behaviour through the
//! HTTP routes, the auth factories and the signed operator surface.
//!
//! A second test opens the shipped presentation the way a client does and
//! dispatches every source its initial Surfaces declare for themselves (the
//! ones a client fetches at start, before any view), asserting each is
//! answered. A handler called directly in a test can be handed a context the
//! dispatch route never builds; only the route shows what a client gets.

use ryeosd::test_support as common;

use common::DaemonHarness;
use reqwest::StatusCode;
use serde_json::{Value, json};

/// Send a JSON request authenticated only by the session cookie.
async fn cookie_request(
    harness: &DaemonHarness,
    method: reqwest::Method,
    path: &str,
    session_id: &str,
    body: Option<Value>,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut request = reqwest::Client::new()
        .request(method, format!("http://{}{path}", harness.bind))
        .header("cookie", format!("ryeos_session={session_id}"));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await?;
    let status = response.status();
    let body = response.json().await.unwrap_or(Value::Null);
    Ok((status, body))
}

/// Activate a launch URL the way a browser does, without following the
/// redirect, and return the session id carried by the `Set-Cookie` header.
async fn activate_launch(launch_url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client.get(launch_url).send().await?;
    anyhow::ensure!(
        response.status() == StatusCode::SEE_OTHER,
        "launch activation must redirect, got {}",
        response.status()
    );
    let set_cookie = response
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .ok_or_else(|| anyhow::anyhow!("launch activation set no cookie"))?
        .to_str()?
        .to_owned();
    set_cookie
        .split(';')
        .find_map(|part| part.trim().strip_prefix("ryeos_session="))
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("no ryeos_session cookie in `{set_cookie}`"))
}

/// Claim the first presentation producer of an activated session through the
/// seat route: prepare, observe, then claim.
async fn claim_producer(
    harness: &DaemonHarness,
    session_id: &str,
    root: &Value,
) -> anyhow::Result<Value> {
    let post = |body: Value| {
        cookie_request(
            harness,
            reqwest::Method::POST,
            "/ui/api/session/seat/open",
            session_id,
            Some(body),
        )
    };

    let mut prepare = root.clone();
    prepare["action"] = json!("prepare");
    prepare["creation_request_id"] = json!(uuid::Uuid::new_v4().to_string());
    let (status, prepared) = post(prepare).await?;
    anyhow::ensure!(status == StatusCode::OK, "prepare: {status} {prepared}");

    let (status, observed) = post(json!({
        "action": "observe",
        "presentation_id": prepared["presentation_id"],
        "thread_id": prepared["thread_id"],
        "chain_root_id": prepared["chain_root_id"],
    }))
    .await?;
    anyhow::ensure!(status == StatusCode::OK, "observe: {status} {observed}");
    let observed: ryeos_client_base::ui::UiPresentationProducerObservation =
        serde_json::from_value(observed)?;

    let mut claim = ryeos_client_base::ui::UiSeatControlRequest::Claim {
        schema: 2,
        operation_id: String::new(),
        presentation_id: observed.presentation_id,
        thread_id: observed.thread_id,
        chain_root_id: observed.chain_root_id,
        predecessor: observed.control_predecessor,
        intent_nonce: uuid::Uuid::new_v4().to_string(),
        claim: ryeos_client_base::ui::UiSeatProducerClaim::Fresh,
    };
    claim
        .assign_identity()
        .map_err(|error| anyhow::anyhow!("derive claim identity: {error}"))?;
    let (status, claimed) = post(serde_json::to_value(claim)?).await?;
    anyhow::ensure!(status == StatusCode::OK, "claim: {status} {claimed}");
    Ok(json!({
        "presentation_id": claimed["presentation_id"],
        "thread_id": claimed["thread_id"],
        "producer_incarnation": claimed["producer_incarnation"],
    }))
}

fn dispatch_body(root: &Value, producer: &Value, view_ref: &str) -> Value {
    json!({
        "producer": producer,
        "binding": {
            "binding_attachment_id": root["binding_attachment_id"],
            "binding_generation": root["binding_generation"],
            "binding_digest": root["binding_digest"],
            "coordinate": { "kind": "source", "view_ref": view_ref, "channel": "default" },
            "payload": { "kind": "source_parameters", "params": {} },
        },
    })
}

#[tokio::test]
async fn browser_session_authority_lifecycle_over_http() -> anyhow::Result<()> {
    let (harness, _fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {}).await?;

    // The configured operator mints a launch for the shipped presentation.
    let (status, minted) = harness
        .post_json(
            "/ui/api/launch/mint",
            json!({
                "ui_binding_contract_revision": ryeos_ui::UI_BINDING_CONTRACT_REVISION,
                "root_ref": "config:ryeos/ui/presentation",
            }),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "mint: {minted}");
    let session_id = minted["session_id"]
        .as_str()
        .expect("mint returns the session id")
        .to_owned();
    let launch_url = minted["launch_url"].as_str().expect("mint returns a url");
    let root = json!({
        "binding_attachment_id": minted["attachment"]["binding_attachment_id"],
        "binding_generation": minted["attachment"]["binding_generation"],
        "binding_digest": minted["attachment"]["binding_digest"],
    });

    // Before activation the session id alone is not a credential the route
    // accepts; activation delivers it as the cookie.
    let cookie_session = activate_launch(launch_url).await?;
    assert_eq!(cookie_session, session_id, "the cookie names the session");

    let (status, current) = cookie_request(
        &harness,
        reqwest::Method::GET,
        "/ui/api/session/current",
        &session_id,
        None,
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "live session authenticates: {current}"
    );

    let producer = claim_producer(&harness, &session_id, &root).await?;

    // In-binding dispatch: the root binding's closure is the presentation's
    // dialog Views (initial Surfaces bind separately), and the Node details
    // dialog names this source.
    let (status, admitted) = cookie_request(
        &harness,
        reqwest::Method::POST,
        "/ui/api/invocations/dispatch",
        &session_id,
        Some(dispatch_body(&root, &producer, "view:ryeos/node/status")),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "in-binding dispatch: {admitted}");

    // Out-of-binding dispatch: the same session and producer, a source the
    // compiled binding does not contain.
    let (status, refused) = cookie_request(
        &harness,
        reqwest::Method::POST,
        "/ui/api/invocations/dispatch",
        &session_id,
        Some(dispatch_body(&root, &producer, "view:ryeos/not-in-binding")),
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "out-of-binding dispatch: {refused}"
    );

    // The operator revokes the session.
    let (status, revoked) = harness
        .post_json(
            "/ui/api/sessions/revoke",
            json!({ "session_id": session_id }),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "revoke: {revoked}");
    assert_eq!(revoked["revoked"], true);

    // Every later cookie request is the revoked answer, on a read route and
    // on the dispatch route alike.
    for (method, path, body) in [
        (reqwest::Method::GET, "/ui/api/session/current", None),
        (
            reqwest::Method::POST,
            "/ui/api/invocations/dispatch",
            Some(dispatch_body(&root, &producer, "view:ryeos/node/status")),
        ),
    ] {
        let (status, answer) = cookie_request(&harness, method, path, &session_id, body).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}: {answer}");
        assert_eq!(answer["error_code"], "browser_session_revoked", "{path}");
    }

    // A cookie that never named a session stays the plain unauthorized
    // answer, so the revoked code is not an oracle for arbitrary ids.
    let (status, unknown) = cookie_request(
        &harness,
        reqwest::Method::GET,
        "/ui/api/session/current",
        "no-such-session",
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_ne!(unknown["error_code"], "browser_session_revoked");
    Ok(())
}

/// Every source the initial Surfaces of the shipped presentation declare for
/// themselves is answered through the dispatch route, under the context that
/// route builds for the source's service.
#[tokio::test]
async fn initial_surface_sources_are_answered_over_http() -> anyhow::Result<()> {
    let (harness, _fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {}).await?;

    let (status, minted) = harness
        .post_json(
            "/ui/api/launch/mint",
            json!({
                "ui_binding_contract_revision": ryeos_ui::UI_BINDING_CONTRACT_REVISION,
                "root_ref": "config:ryeos/ui/presentation",
            }),
        )
        .await?;
    assert_eq!(status, StatusCode::OK, "mint: {minted}");
    let launch_url = minted["launch_url"].as_str().expect("mint returns a url");
    let root = json!({
        "binding_attachment_id": minted["attachment"]["binding_attachment_id"],
        "binding_generation": minted["attachment"]["binding_generation"],
        "binding_digest": minted["attachment"]["binding_digest"],
    });
    let session_id = activate_launch(launch_url).await?;

    // The seat's preparation makes the presentation a session membership, so
    // the session is read after the claim, as a client reads it.
    let producer = claim_producer(&harness, &session_id, &root).await?;
    let (status, current) = cookie_request(
        &harness,
        reqwest::Method::GET,
        "/ui/api/session/current",
        &session_id,
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "session: {current}");
    let current = current.get("result").cloned().unwrap_or(current);

    let presentation = current["presentations"]
        .as_array()
        .expect("session lists its presentations")
        .iter()
        .find(|membership| membership["presentation_id"] == producer["presentation_id"])
        .unwrap_or_else(|| panic!("the claimed presentation is a session membership: {current}"));
    let attachments = current["binding_attachments"]
        .as_array()
        .expect("session lists its binding attachments");
    let surfaces = presentation["surfaces"]
        .as_array()
        .expect("presentation lists its initial Surfaces");
    assert!(!surfaces.is_empty(), "the presentation opens a Surface");

    let mut dispatched = Vec::new();
    for surface in surfaces {
        let attachment = attachments
            .iter()
            .find(|attachment| {
                attachment["binding_attachment_id"] == surface["binding"]["binding_attachment_id"]
            })
            .expect("an initial Surface names a session attachment");
        let sources = attachment["presentation"]["definition"]["surface"]["sources"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        for (channel, source) in sources {
            // A client fetches the sources activated at start, and a source
            // that needs a project only when the attachment has one.
            if source["activation"] == "on_demand"
                || (source["requires_project"] == true && attachment["project_path"].is_null())
            {
                continue;
            }
            let body = json!({
                "producer": producer,
                "binding": {
                    "binding_attachment_id": attachment["binding_attachment_id"],
                    "binding_generation": attachment["binding_generation"],
                    "binding_digest": attachment["binding_digest"],
                    "coordinate": {
                        "kind": "source",
                        "view_ref": attachment["root_ref"],
                        "channel": channel,
                    },
                    "payload": {
                        "kind": "source_parameters",
                        "params": source.get("params").cloned().unwrap_or_else(|| json!({})),
                    },
                },
            });
            let (status, answer) = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                cookie_request(
                    &harness,
                    reqwest::Method::POST,
                    "/ui/api/invocations/dispatch",
                    &session_id,
                    Some(body),
                ),
            )
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Surface source `{channel}` of {} was not answered",
                    attachment["root_ref"]
                )
            })??;
            assert_eq!(
                status,
                StatusCode::OK,
                "Surface source `{channel}` ({}) of {}: {answer}",
                source["ref"],
                attachment["root_ref"]
            );
            assert_eq!(answer["status"], "executed", "`{channel}`: {answer}");
            dispatched.push(channel);
        }
    }
    // The shipped Home reads the node summary for its bars and views; a
    // presentation that stopped declaring it would make this test vacuous.
    assert!(
        dispatched.iter().any(|channel| channel == "dimension"),
        "the initial Surface declares the node summary source: {dispatched:?}"
    );
    Ok(())
}
