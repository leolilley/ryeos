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

/// Await one request and print its wall time to stderr as
/// `timing <step> <ms>ms`. The line is hidden on a pass unless the test binary
/// runs with `--nocapture`; it is shown on a failure either way.
async fn timed<T>(step: &str, request: impl std::future::Future<Output = T>) -> T {
    let started = std::time::Instant::now();
    let answer = request.await;
    eprintln!("timing {step} {}ms", started.elapsed().as_millis());
    answer
}

/// A daemon log line with its terminal colour sequences removed.
fn without_terminal_colors(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            // Skip through the letter that ends the sequence.
            for skipped in characters.by_ref() {
                if skipped.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(character);
        }
    }
    plain
}

/// The unsigned value of `name=<digits>` in a colour-free phase timing line.
fn phase_field(line: &str, name: &str) -> anyhow::Result<u64> {
    let start = line
        .find(&format!("{name}="))
        .ok_or_else(|| anyhow::anyhow!("phase timing line has no {name}: {line}"))?
        + name.len()
        + 1;
    let digits: String = line[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .map_err(|error| anyhow::anyhow!("phase timing field {name} in {line}: {error}"))
}

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
    let body_action = body
        .as_ref()
        .and_then(|body| body["action"].as_str())
        .map(str::to_owned);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let step = match body_action.as_deref() {
        Some(action) => format!("{path} {action}"),
        None => path.to_owned(),
    };
    let (status, body) = timed(&step, async {
        let response = request.send().await?;
        let status = response.status();
        let body = response.json().await.unwrap_or(Value::Null);
        anyhow::Ok((status, body))
    })
    .await?;
    Ok((status, body))
}

/// Activate a launch URL the way a browser does, without following the
/// redirect, and return the session id carried by the `Set-Cookie` header.
async fn activate_launch(launch_url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = timed("launch activate", client.get(launch_url).send()).await?;
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
    let (harness, _fixture) = timed(
        "daemon start",
        DaemonHarness::start_fast_with(
            |_, _, _| Ok(()),
            |cmd| {
                // The daemon's per-phase timing lines for mint and seat open.
                cmd.env("RUST_LOG", "info,ryeos_ui::timing=debug");
            },
        ),
    )
    .await?;
    let outcome = lifecycle(&harness).await;

    // The same daemon once more with a fresh session, so the timing lines show
    // a warm open next to the cold one.
    eprintln!("timing --- repeat: mint, activate, prepare, observe, claim");
    let repeat = fresh_session_claim(&harness).await;

    // The daemon's stderr goes to a file the harness reads back. Its phase
    // timing lines say where each mint and seat open spent its time.
    let log = harness.stderr_so_far();
    let phase_lines: Vec<String> = log
        .lines()
        .filter(|line| line.contains("request phase timing"))
        .map(without_terminal_colors)
        .collect();
    for line in &phase_lines {
        eprintln!("daemon {line}");
    }
    let verification_counts = prepare_verification_counts(&phase_lines);
    if outcome.is_err() || repeat.is_err() {
        // Show the tail so a failed request can be matched with what the
        // daemon logged.
        let lines: Vec<&str> = log.lines().collect();
        let tail = &lines[lines.len().saturating_sub(200)..];
        eprintln!(
            "daemon stderr, last {} of {} lines:\n{}",
            tail.len(),
            lines.len(),
            tail.join("\n")
        );
    }
    outcome.and(repeat)?;

    // One prepare verifies the installed bundles 10 times: before and after
    // the Surface compile (2); before and after the admission block plus once
    // before it publishes the membership (3); before and after the seat-root
    // admission (2); before and after the initialization step plus once
    // before it appends the record (3). Work inside a guarded operation adds
    // none. The bound leaves room for an unrelated daemon task verifying
    // while the prepare runs; a guard that stopped covering nested work would
    // show several hundred.
    let verification_counts = verification_counts?;
    anyhow::ensure!(
        verification_counts.len() == 2,
        "expected two prepare timing lines, found {}",
        verification_counts.len()
    );
    for verifications in verification_counts {
        anyhow::ensure!(
            (10..=22).contains(&verifications),
            "one prepare ran {verifications} bundle-generation verifications; 10 are expected"
        );
    }
    Ok(())
}

/// The bundle-generation verification count of each prepare, from the
/// daemon's colour-free `total` phase timing lines.
fn prepare_verification_counts(phase_lines: &[String]) -> anyhow::Result<Vec<u64>> {
    phase_lines
        .iter()
        .filter(|line| {
            line.contains("request=\"seat_open_prepare\"") && line.contains("phase=\"total\"")
        })
        .map(|line| phase_field(line, "generation_verifications"))
        .collect()
}

/// Mint, activate and claim a producer for a new session on a running daemon.
async fn fresh_session_claim(harness: &DaemonHarness) -> anyhow::Result<()> {
    let (status, minted) = timed(
        "mint",
        harness.post_json(
            "/ui/api/launch/mint",
            json!({
                "ui_binding_contract_revision": ryeos_ui::UI_BINDING_CONTRACT_REVISION,
                "root_ref": "config:ryeos/ui/presentation",
            }),
        ),
    )
    .await?;
    anyhow::ensure!(status == StatusCode::OK, "mint: {minted}");
    let session_id = minted["session_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("mint returned no session id"))?
        .to_owned();
    let root = json!({
        "binding_attachment_id": minted["attachment"]["binding_attachment_id"],
        "binding_generation": minted["attachment"]["binding_generation"],
        "binding_digest": minted["attachment"]["binding_digest"],
    });
    activate_launch(
        minted["launch_url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("mint returned no launch url"))?,
    )
    .await?;
    claim_producer(harness, &session_id, &root).await?;
    Ok(())
}

async fn lifecycle(harness: &DaemonHarness) -> anyhow::Result<()> {
    // The configured operator mints a launch for the shipped presentation.
    let (status, minted) = timed(
        "mint",
        harness.post_json(
            "/ui/api/launch/mint",
            json!({
                "ui_binding_contract_revision": ryeos_ui::UI_BINDING_CONTRACT_REVISION,
                "root_ref": "config:ryeos/ui/presentation",
            }),
        ),
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
        harness,
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

    let producer = claim_producer(harness, &session_id, &root).await?;

    // In-binding dispatch: the root binding's closure is the presentation's
    // dialog Views (initial Surfaces bind separately), and the Node details
    // dialog names this source.
    let (status, admitted) = cookie_request(
        harness,
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
        harness,
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
    let (status, revoked) = timed(
        "revoke",
        harness.post_json(
            "/ui/api/sessions/revoke",
            json!({ "session_id": session_id }),
        ),
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
        let (status, answer) = cookie_request(harness, method, path, &session_id, body).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}: {answer}");
        assert_eq!(answer["error_code"], "browser_session_revoked", "{path}");
    }

    // A cookie that never named a session stays the plain unauthorized
    // answer, so the revoked code is not an oracle for arbitrary ids.
    let (status, unknown) = cookie_request(
        harness,
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

/// The signed-picture route over real HTTP: it answers only a live session,
/// serves nothing a binding does not name (no shipped Surface names a
/// picture), answers every key shape that is not an allowed hash with the same
/// 404, and answers a revoked session the revoked code like every other
/// session route.
#[tokio::test]
async fn picture_route_serves_nothing_a_session_binding_does_not_name() -> anyhow::Result<()> {
    let (harness, _fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {}).await?;
    let unnamed = "ab".repeat(32);
    let url = format!("http://{}/ui/art/{unnamed}", harness.bind);

    // No cookie, and a cookie that names no session, are the auth layer's 401.
    let anonymous = reqwest::Client::new().get(&url).send().await?;
    anyhow::ensure!(
        anonymous.status() == StatusCode::UNAUTHORIZED,
        "no cookie: {}",
        anonymous.status()
    );
    let (status, _) = cookie_request(
        &harness,
        reqwest::Method::GET,
        &format!("/ui/art/{unnamed}"),
        "no-such-session",
        None,
    )
    .await?;
    anyhow::ensure!(
        status == StatusCode::UNAUTHORIZED,
        "unknown cookie: {status}"
    );

    let (status, minted) = harness
        .post_json(
            "/ui/api/launch/mint",
            json!({
                "ui_binding_contract_revision": ryeos_ui::UI_BINDING_CONTRACT_REVISION,
                "root_ref": "config:ryeos/ui/presentation",
            }),
        )
        .await?;
    anyhow::ensure!(status == StatusCode::OK, "mint: {minted}");
    let session_id = activate_launch(
        minted["launch_url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("mint returned no launch url"))?,
    )
    .await?;

    // A live session is told a hash no binding names is not found, in the
    // route's own body.
    let (status, body) = cookie_request(
        &harness,
        reqwest::Method::GET,
        &format!("/ui/art/{unnamed}"),
        &session_id,
        None,
    )
    .await?;
    anyhow::ensure!(
        status == StatusCode::NOT_FOUND,
        "unnamed hash: {status} {body}"
    );
    anyhow::ensure!(body["error"] == "not found", "unnamed hash body: {body}");

    // Keys that are not a lowercase hex SHA-256 never reach a lookup.
    for key in [
        "..%2F..%2Fetc%2Fpasswd".to_owned(),
        "%2e%2e%2f%2e%2e%2fetc%2fpasswd".to_owned(),
        unnamed.to_uppercase(),
        format!("{unnamed}.webp"),
        unnamed[..63].to_owned(),
    ] {
        let (status, _) = cookie_request(
            &harness,
            reqwest::Method::GET,
            &format!("/ui/art/{key}"),
            &session_id,
            None,
        )
        .await?;
        anyhow::ensure!(status == StatusCode::NOT_FOUND, "key {key}: {status}");
    }

    // Only GET is served.
    let (status, _) = cookie_request(
        &harness,
        reqwest::Method::POST,
        &format!("/ui/art/{unnamed}"),
        &session_id,
        Some(json!({})),
    )
    .await?;
    anyhow::ensure!(
        status.is_client_error(),
        "a POST to the picture route must be refused, got {status}"
    );

    // Revocation reaches this route like every other session route.
    let (status, revoked) = harness
        .post_json(
            "/ui/api/sessions/revoke",
            json!({ "session_id": session_id }),
        )
        .await?;
    anyhow::ensure!(status == StatusCode::OK, "revoke: {revoked}");
    let (status, answer) = cookie_request(
        &harness,
        reqwest::Method::GET,
        &format!("/ui/art/{unnamed}"),
        &session_id,
        None,
    )
    .await?;
    anyhow::ensure!(status == StatusCode::UNAUTHORIZED, "revoked: {status}");
    anyhow::ensure!(
        answer["error_code"] == "browser_session_revoked",
        "revoked body: {answer}"
    );
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

/// A booted node reads its own signed capability report back through the
/// capability service: the report stands, its signature verified, and the
/// service names the signer and when the node measured it. The wording
/// service answers from the signed config.
#[tokio::test(flavor = "multi_thread")]
async fn node_capability_facts_are_read_from_the_verified_report() -> anyhow::Result<()> {
    let (harness, _fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {}).await?;

    let (status, body) = harness
        .post_execute("service:ui/ryeos-ui/node/capabilities", ".", json!({}))
        .await?;
    anyhow::ensure!(status.is_success(), "capabilities: {status} {body}");
    let facts = body.get("result").unwrap_or(&body);
    let facts = facts.get("result").unwrap_or(facts);
    anyhow::ensure!(
        facts["schema_version"] == "ryeos.ui.node.capabilities.v1",
        "facts: {body}"
    );
    anyhow::ensure!(facts["report"]["state"] == "present", "report: {body}");
    anyhow::ensure!(facts["report"]["verified"] == true, "verified: {body}");
    anyhow::ensure!(
        facts["report"]["signer"]
            .as_str()
            .is_some_and(|signer| signer.starts_with("fp:")),
        "signer: {body}"
    );
    anyhow::ensure!(
        facts["report"]["measured_at"]
            .as_str()
            .is_some_and(|at| !at.is_empty()),
        "measured_at: {body}"
    );
    anyhow::ensure!(
        facts["host"]["class"]
            .as_str()
            .is_some_and(|class| !class.is_empty()),
        "host class: {body}"
    );

    let (status, body) = harness
        .post_execute(
            "service:ui/ryeos-ui/node/capabilities/wording",
            ".",
            json!({}),
        )
        .await?;
    anyhow::ensure!(status.is_success(), "wording: {status} {body}");
    let wording = body.get("result").unwrap_or(&body);
    let wording = wording.get("result").unwrap_or(wording);
    anyhow::ensure!(
        wording["schema_version"] == "ryeos.ui.node_capability_wording.v1",
        "wording: {body}"
    );
    Ok(())
}
