//! Live UI pickup of a revised, project-authored view, end to end over real
//! HTTP.
//!
//! A project holds an operator-signed Surface, view set and view. A browser
//! seat opens that Surface through the presentation's open-surface operation.
//! The operator then signs a new version of the view in their own process and
//! tells the node with a signed `items/revised` request, the way the CLI does.
//! From there the seat does what the client does on an `item.hint`:
//!
//!   * the session's event stream carries exactly one `item.hint` for the
//!     view, and a second session that does not name the view hears none;
//!   * `ui/session/attachments/revise` answers `revised: true` with a
//!     successor descriptor that holds the new view content;
//!   * a `RebindAttachment` composition request, prepared and appended, is
//!     accepted and its completion is retained;
//!   * the authenticated seat history verifies with the rebind folded in;
//!   * a dispatch against the successor is admitted;
//!   * detaching the settled source is the idempotent no-op.

use ryeosd::test_support as common;

use std::sync::{Arc, Mutex};

use common::DaemonHarness;
use reqwest::StatusCode;
use ryeos_client_base::ids::{PresentationId, RyeOsViewInstanceKey, SurfaceInstanceId};
use ryeos_client_base::ui::admission_operation::{
    UiPreparedPresentationOperation, UiSeatAppendIdentity,
};
use ryeos_client_base::ui::composition_operation::{CompositionMutation, CompositionOwner};
use ryeos_client_base::ui::composition_replay::UiCompositionReplay;
use ryeos_client_base::ui::seat::retained_seat_event_with_evidence;
use ryeos_client_base::ui::{SeatEventKind, UiBindingAttachmentCoordinate, seat_payload_digest};
use serde_json::{Value, json};

const SURFACE_REF: &str = "surface:acme/ui/notes";
const VIEW_REF: &str = "view:acme/notes";
const PREPARED: &str = "seat.presentation_operation.prepared";
const COMPLETED: &str = "seat.presentation_operation.completed";

/// The view at two versions. The marker is the observable difference between
/// them: it is the input placeholder, so it is part of the compiled view.
fn view_body(marker: &str) -> String {
    format!(
        "name: acme-notes\nversion: \"1.0.0\"\ndescription: Notes\n\nwidget: rows\nsources:\n  default:\n    ref: service:ui/ryeos-ui/field/project\n    params: {{ expansions: [] }}\n    collection: rows\ninput:\n  id: line\n  placeholder: {marker}\n  local_filter: true\n"
    )
}

const VIEW_SET_BODY: &str = "schema: 2\nid: notes\ntitle: Notes\nroot:\n  type: group\n  views: [\"view:acme/notes\"]\n  active: 0\n";

const SURFACE_BODY: &str = "name: acme-notes\ntitle: Notes\nversion: \"1.0.0\"\ncapabilities:\n  sources: [\"ryeos.execute.service.ui/ryeos-ui/field/project\"]\n  affordances: []\nview_sets:\n  - view_set:acme/notes\n";

/// Write one project item signed by `signer`; returns the signed bytes.
fn write_signed(
    project: &std::path::Path,
    relative: &str,
    body: &str,
    signer: &lillux::crypto::SigningKey,
) -> Vec<u8> {
    let path = project.join(relative);
    std::fs::create_dir_all(path.parent().expect("item parent")).expect("create item directory");
    let signed = lillux::signature::sign_content(body, signer, "#", None);
    std::fs::write(&path, &signed).expect("write signed item");
    signed.into_bytes()
}

/// What `ryeos sign` leaves behind for the view: the new signed file and a
/// revision record signed with the operator's key. Returns the record's
/// sequence number.
fn operator_signs_view(
    project: &std::path::Path,
    operator: &lillux::crypto::SigningKey,
    marker: &str,
) -> u64 {
    use ryeos_node_runtime::item_revision::{ActorRole, ChainItem, Change, append};
    let relative = ".ai/views/acme/notes.yaml";
    let before = std::fs::read(project.join(relative)).ok();
    let after = write_signed(project, relative, &view_body(marker), operator);
    append(
        project,
        operator,
        ActorRole::Operator,
        Change {
            item: &ChainItem::parse(VIEW_REF).expect("view chain item"),
            file_ext: ".yaml",
            before: before.as_deref(),
            after: Some(&after),
            provenance: std::collections::BTreeMap::from([(
                "action".to_owned(),
                "sign".to_owned(),
            )]),
        },
    )
    .expect("operator revision record")
    .seq
}

/// The value a route answers with, whether or not the route wraps it under
/// `result`: the object that carries `key`.
fn locate<'a>(value: &'a Value, key: &str) -> &'a Value {
    if value.get(key).is_some() {
        return value;
    }
    if let Some(inner) = value.get("result") {
        return locate(inner, key);
    }
    panic!("no `{key}` in {value}");
}

async fn cookie_post(
    harness: &DaemonHarness,
    path: &str,
    session_id: &str,
    body: Value,
) -> anyhow::Result<(StatusCode, Value)> {
    let response = reqwest::Client::new()
        .post(format!("http://{}{path}", harness.bind))
        .header("cookie", format!("ryeos_session={session_id}"))
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let body = response.json().await.unwrap_or(Value::Null);
    Ok((status, body))
}

async fn cookie_get(
    harness: &DaemonHarness,
    path: &str,
    session_id: &str,
) -> anyhow::Result<Value> {
    let response = reqwest::Client::new()
        .get(format!("http://{}{path}", harness.bind))
        .header("cookie", format!("ryeos_session={session_id}"))
        .send()
        .await?;
    anyhow::ensure!(response.status() == StatusCode::OK, "GET {path}");
    Ok(response.json().await?)
}

async fn session_current(harness: &DaemonHarness, session_id: &str) -> anyhow::Result<Value> {
    let current = cookie_get(harness, "/ui/api/session/current", session_id).await?;
    Ok(current.get("result").cloned().unwrap_or(current))
}

/// Activate a launch URL without following the redirect; returns the session
/// id the `Set-Cookie` header carries.
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

/// Mint and activate a session for the shipped presentation; returns its id
/// and the root binding coordinate.
async fn mint_session(
    harness: &DaemonHarness,
    project: Option<&std::path::Path>,
) -> anyhow::Result<(String, Value)> {
    let mut request = json!({
        "ui_binding_contract_revision": ryeos_ui::UI_BINDING_CONTRACT_REVISION,
        "root_ref": "config:ryeos/ui/presentation",
    });
    if let Some(project) = project {
        request["project_path"] = json!(project.to_str().expect("utf-8 project path"));
    }
    let (status, minted) = harness.post_json("/ui/api/launch/mint", request).await?;
    anyhow::ensure!(status == StatusCode::OK, "mint: {minted}");
    let session_id = activate_launch(
        minted["launch_url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("mint returned no launch url"))?,
    )
    .await?;
    let root = json!({
        "binding_attachment_id": minted["attachment"]["binding_attachment_id"],
        "binding_generation": minted["attachment"]["binding_generation"],
        "binding_digest": minted["attachment"]["binding_digest"],
    });
    Ok((session_id, root))
}

/// Claim the first presentation producer through the seat route.
async fn claim_producer(
    harness: &DaemonHarness,
    session_id: &str,
    root: &Value,
) -> anyhow::Result<Value> {
    let post = |body: Value| cookie_post(harness, "/ui/api/session/seat/open", session_id, body);
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

/// One claimed seat and the next engine sequence its producer may append at.
struct Seat {
    session_id: String,
    producer: Value,
    root: Value,
    bounds: Value,
    next_engine_seq: u64,
}

impl Seat {
    fn presentation_id(&self) -> &str {
        self.producer["presentation_id"].as_str().unwrap()
    }

    /// Append events to the seat's own history at the next engine positions.
    async fn append(
        &mut self,
        harness: &DaemonHarness,
        events: &[(&str, Value)],
    ) -> anyhow::Result<Value> {
        let first = self.next_engine_seq;
        let last = first + events.len() as u64 - 1;
        let rows = Value::Array(
            events
                .iter()
                .enumerate()
                .map(|(offset, (event_type, payload))| {
                    json!({
                        "engine_seq": first + offset as u64,
                        "event_type": event_type,
                        "payload": payload,
                    })
                })
                .collect(),
        );
        let payload_digest = seat_payload_digest(&rows).expect("payload digest");
        let operation_id = UiSeatAppendIdentity {
            presentation_id: self.presentation_id().to_owned(),
            chain_root_id: self.producer["thread_id"].as_str().unwrap().to_owned(),
            thread_id: self.producer["thread_id"].as_str().unwrap().to_owned(),
            producer_incarnation: self.producer["producer_incarnation"]
                .as_str()
                .unwrap()
                .to_owned(),
            first_engine_seq: first,
            last_engine_seq: last,
            event_count: events.len() as u64,
            payload_digest: payload_digest.clone(),
        }
        .derive_identity()
        .expect("append identity");
        let (status, receipt) = cookie_post(
            harness,
            "/ui/api/session/seat/append",
            &self.session_id,
            json!({
                "presentation_id": self.producer["presentation_id"],
                "thread_id": self.producer["thread_id"],
                "producer_incarnation": self.producer["producer_incarnation"],
                "operation_id": operation_id,
                "first_engine_seq": first,
                "last_engine_seq": last,
                "event_count": events.len(),
                "payload_digest": payload_digest,
                "events": rows,
            }),
        )
        .await?;
        anyhow::ensure!(
            status == StatusCode::OK,
            "append at engine sequence {first}: {status} {receipt}"
        );
        self.next_engine_seq = last + 1;
        Ok(receipt)
    }

    /// The seat's authenticated history, read over the stream route: every
    /// frame as (event name, payload), ending at the completion fence.
    async fn history(&self, harness: &DaemonHarness) -> anyhow::Result<Vec<(String, Value)>> {
        let response = reqwest::Client::new()
            .post(format!(
                "http://{}/ui/api/session/seat/history",
                harness.bind
            ))
            .header("cookie", format!("ryeos_session={}", self.session_id))
            .json(&json!({
                "presentation_id": self.producer["presentation_id"],
                "chain_root_id": self.producer["thread_id"],
            }))
            .send()
            .await?;
        anyhow::ensure!(response.status() == StatusCode::OK, "history route");
        let text = tokio::time::timeout(std::time::Duration::from_secs(30), response.text())
            .await
            .map_err(|_| anyhow::anyhow!("seat history did not end"))??;
        let frames = parse_sse(&text);
        anyhow::ensure!(
            frames
                .last()
                .is_some_and(|(event, _)| event == "ui_seat.replay.complete.v1"),
            "seat history did not reach its completion fence: {text}"
        );
        Ok(frames)
    }

    /// The seat's composition, folded from its authenticated history the way
    /// the client folds it.
    async fn replay(&self, harness: &DaemonHarness) -> anyhow::Result<UiCompositionReplay> {
        let frames = self.history(harness).await?;
        let events = frames
            .iter()
            .filter(|(event, _)| event == "ui_seat.replay.events.v1")
            .flat_map(|(_, data)| data["events"].as_array().expect("history rows").iter())
            .filter_map(|row| {
                retained_seat_event_with_evidence(row)
                    .expect("a retained seat row decodes")
                    .map(|retained| retained.event)
            })
            .collect::<Vec<_>>();
        let SeatEventKind::PresentationInitialized(record) = &events
            .first()
            .ok_or_else(|| anyhow::anyhow!("seat history has no rows"))?
            .kind
        else {
            anyhow::bail!("seat history does not begin with its initialization");
        };
        UiCompositionReplay::from_history(&record.initialization, &events)
            .map_err(|error| anyhow::anyhow!("fold seat history: {error:?}"))
    }

    fn prepared_operation(
        &self,
        body: Value,
        operation_id: &str,
    ) -> UiPreparedPresentationOperation {
        let mut prepared: UiPreparedPresentationOperation = serde_json::from_value(json!({
            "schema": 2,
            "prepared_engine_seq": self.next_engine_seq,
            "operation_id": operation_id,
            "producer": self.producer,
            "root_binding": self.root,
            "request_bounds": self.bounds,
            "body": body,
        }))
        .expect("prepared operation");
        prepared.assign_identity().expect("operation identity");
        prepared
    }
}

/// Server-sent frames as (event name, decoded data).
fn parse_sse(text: &str) -> Vec<(String, Value)> {
    text.split("\n\n")
        .filter_map(|frame| {
            let mut event = None;
            let mut data = None;
            for line in frame.lines() {
                if let Some(name) = line.strip_prefix("event:") {
                    event = Some(name.trim().to_owned());
                } else if let Some(body) = line.strip_prefix("data:") {
                    data = serde_json::from_str::<Value>(body.trim()).ok();
                }
            }
            Some((event?, data?))
        })
        .collect()
}

/// A session event stream read in the background, kept as the frames it has
/// delivered so far.
struct EventTap {
    frames: Arc<Mutex<Vec<(String, Value)>>>,
}

impl EventTap {
    /// Subscribe; returns once the daemon has answered, which is after the
    /// session's bus subscription exists.
    async fn open(harness: &DaemonHarness, session_id: &str) -> anyhow::Result<Self> {
        let mut response = reqwest::Client::new()
            .get(format!(
                "http://{}/ui/events/session/{session_id}",
                harness.bind
            ))
            .header("cookie", format!("ryeos_session={session_id}"))
            .send()
            .await?;
        anyhow::ensure!(
            response.status() == StatusCode::OK,
            "session events: {}",
            response.status()
        );
        let frames = Arc::new(Mutex::new(Vec::new()));
        let sink = frames.clone();
        tokio::spawn(async move {
            let mut buffer = String::new();
            while let Ok(Some(chunk)) = response.chunk().await {
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(end) = buffer.find("\n\n") {
                    let frame: String = buffer.drain(..end + 2).collect();
                    sink.lock().unwrap().extend(parse_sse(&frame));
                }
            }
        });
        Ok(Self { frames })
    }

    fn hints(&self) -> Vec<Value> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|(event, _)| event == "item.hint")
            .map(|(_, data)| data.clone())
            .collect()
    }

    async fn wait_for_hints(&self, count: usize) -> Vec<Value> {
        for _ in 0..100 {
            let hints = self.hints();
            if hints.len() >= count {
                return hints;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        self.hints()
    }
}

fn descriptor_of<'a>(current: &'a Value, attachment_id: &str) -> &'a Value {
    current["binding_attachments"]
        .as_array()
        .expect("session lists its binding attachments")
        .iter()
        .find(|attachment| attachment["binding_attachment_id"] == attachment_id)
        .unwrap_or_else(|| panic!("session holds attachment {attachment_id}: {current}"))
}

fn coordinate_of(descriptor: &Value) -> UiBindingAttachmentCoordinate {
    UiBindingAttachmentCoordinate {
        binding_attachment_id: descriptor["binding_attachment_id"]
            .as_str()
            .unwrap()
            .to_owned(),
        binding_generation: descriptor["binding_generation"].as_u64().unwrap(),
        binding_digest: descriptor["binding_digest"].as_str().unwrap().to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_seat_picks_up_a_revised_project_view_through_hint_revise_and_rebind()
-> anyhow::Result<()> {
    let (harness, fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {}).await?;
    let operator = fixture.user.clone();

    // A project whose Surface, view set and view the operator signed: items
    // signed with the operator's key are trusted project items.
    let guard = tempfile::tempdir()?;
    let project = guard.path().canonicalize()?;
    write_signed(
        &project,
        ".ai/surfaces/acme/ui/notes.yaml",
        SURFACE_BODY,
        &operator,
    );
    write_signed(
        &project,
        ".ai/view_sets/acme/notes.yaml",
        VIEW_SET_BODY,
        &operator,
    );
    assert_eq!(operator_signs_view(&project, &operator, "marker-v1"), 1);

    // A browser seat bound to the project, and a second session that is not.
    let (session_id, root) = mint_session(&harness, Some(&project)).await?;
    let producer = claim_producer(&harness, &session_id, &root).await?;
    let (other_session, _) = mint_session(&harness, None).await?;
    let current = session_current(&harness, &session_id).await?;
    let bounds = descriptor_of(&current, root["binding_attachment_id"].as_str().unwrap())
        ["binding_request_bounds"]
        .clone();
    let mut seat = Seat {
        session_id: session_id.clone(),
        producer,
        root: root.clone(),
        bounds,
        next_engine_seq: 0,
    };

    // Open the project Surface through the presentation's open-surface
    // operation: prepare it on the seat, dispatch it, retain its result.
    let presentation_id = PresentationId::try_new(seat.presentation_id().to_owned())
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let origin = RyeOsViewInstanceKey::shared_dialog(&presentation_id, "surfaces")
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let admission = seat.prepared_operation(
        json!({
            "kind": "admission",
            "purpose": "open_surface",
            "surface_open_target": {
                "selection": {"kind": "authored", "surface_ref": SURFACE_REF},
                "placement": {"kind": "new_surface"},
            },
            "particular_selection": null,
            "invocation_origin": origin,
            "request": {
                "binding_attachment_id": root["binding_attachment_id"],
                "binding_generation": root["binding_generation"],
                "binding_digest": root["binding_digest"],
                "coordinate": {
                    "kind": "affordance",
                    "view_ref": "view:ryeos/surfaces/list",
                    "affordance_id": "open",
                },
                "payload": {"kind": "selection", "record": {"canonical_ref": SURFACE_REF}},
            },
            "success_notice": null,
        }),
        "00000000-0000-4000-8000-000000000000",
    );
    seat.append(&harness, &[(PREPARED, serde_json::to_value(&admission)?)])
        .await?;
    let (status, dispatched) = cookie_post(
        &harness,
        "/ui/api/invocations/dispatch",
        &session_id,
        json!({
            "presentation_operation_id": admission.operation_id,
            "producer": seat.producer,
            "binding": admission.admission().expect("admission body").request,
        }),
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "open the project Surface: {dispatched}"
    );
    let opened = locate(&dispatched, "ui_transition");
    let transition = &opened["ui_transition"];
    assert_eq!(transition["kind"], "open_surface", "{dispatched}");
    seat.append(
        &harness,
        &[(
            COMPLETED,
            json!({"operation_id": admission.operation_id, "result": opened}),
        )],
    )
    .await?;
    let surface_id = transition["surface"]["surface_instance_id"]
        .as_str()
        .expect("the opened Surface has an instance id")
        .to_owned();
    let from = coordinate_of(&transition["attachment"]);

    // The Surface's descriptor names the view, so the daemon will tell this
    // session when the view is revised, and the seat will know to revise it.
    let current = session_current(&harness, &session_id).await?;
    let held = descriptor_of(&current, &from.binding_attachment_id);
    let named = held["named_items"].as_array().expect("named_items");
    assert!(
        named.iter().any(|item| item == VIEW_REF),
        "the Surface descriptor names {VIEW_REF}: {named:?}"
    );
    let held_view = serde_json::to_string(&held["presentation"])?;
    assert!(held_view.contains("marker-v1"), "the open Surface shows v1");

    // Both sessions listen on their event routes.
    let tap = EventTap::open(&harness, &session_id).await?;
    let other_tap = EventTap::open(&harness, &other_session).await?;

    // The operator signs a new version of the view in their own process and
    // announces it with a signed `items/revised` request.
    assert_eq!(operator_signs_view(&project, &operator, "marker-v2"), 2);
    let (status, announced) = harness
        .post_execute(
            "service:items/revised",
            ".",
            json!({"project_path": project.to_str().unwrap(), "item_refs": [VIEW_REF]}),
        )
        .await?;
    assert!(status.is_success(), "items/revised: {status} {announced:#}");

    // Exactly one hint reaches the seat's session, naming the view, and none
    // reaches the unrelated session.
    let hints = tap.wait_for_hints(1).await;
    assert_eq!(hints.len(), 1, "one item.hint: {hints:?}");
    assert_eq!(hints[0]["item_ref"], VIEW_REF, "{hints:?}");
    assert_eq!(hints[0]["revision"], 2, "{hints:?}");

    // The seat asks the daemon to recompile the Surface's attachment.
    let (status, revised) = cookie_post(
        &harness,
        "/ui/api/session/attachments/revise",
        &session_id,
        json!({
            "presentation_id": seat.presentation_id(),
            "binding_attachment_id": from.binding_attachment_id,
            "binding_generation": from.binding_generation,
            "binding_digest": from.binding_digest,
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "revise: {revised}");
    let revised = locate(&revised, "revised");
    assert_eq!(revised["revised"], true, "{revised}");
    let successor = &revised["attachment"];
    let to = coordinate_of(successor);
    assert_ne!(to.binding_digest, from.binding_digest);
    assert_eq!(
        serde_json::from_value::<UiBindingAttachmentCoordinate>(revised["from"].clone())?,
        from
    );
    let successor_view = serde_json::to_string(&successor["presentation"])?;
    assert!(
        successor_view.contains("marker-v2") && !successor_view.contains("marker-v1"),
        "the successor resolves the new view content"
    );

    // The source stays owned by its Surface until the rebind settles in
    // history, so it cannot be detached yet.
    let (status, again) = cookie_post(
        &harness,
        "/ui/api/session/attachments/detach",
        &session_id,
        json!({
            "binding_attachment_id": from.binding_attachment_id,
            "binding_generation": from.binding_generation,
            "binding_digest": from.binding_digest,
        }),
    )
    .await?;
    assert_ne!(
        status,
        StatusCode::OK,
        "the source cannot be detached while its Surface owns it: {again}"
    );

    // The rebind request, prepared and appended from the successor's
    // descriptor, then completed the way the client's own replay computes it.
    let replay = seat.replay(&harness).await?;
    let maximum = seat.bounds["max_request_bytes"].as_u64().unwrap();
    let operation_id = "11111111-2222-4333-8444-555555555555";
    let surface = SurfaceInstanceId::try_new(surface_id.clone())
        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let rebind = seat.prepared_operation(
        json!({"kind": "composition", "request": {
            "schema": 1,
            "presentation_id": seat.presentation_id(),
            "operation_id": operation_id,
            "expected_revision": replay.revision,
            "expected_next_mount_id": replay.next_mount_id,
            "owner": serde_json::to_value(CompositionOwner::Surface {
                surface_instance_id: surface.clone(),
                binding: from.clone(),
            })?,
            "mutation": serde_json::to_value(CompositionMutation::RebindAttachment {
                from: from.clone(),
                to: to.clone(),
                descriptor: Box::new(serde_json::from_value(successor.clone())?),
            })?,
        }}),
        operation_id,
    );
    let completion = replay
        .expected_composition_completion(&rebind, maximum)
        .map_err(|error| anyhow::anyhow!("expected rebind completion: {error:?}"))?;
    seat.append(&harness, &[(PREPARED, serde_json::to_value(&rebind)?)])
        .await?;
    seat.append(&harness, &[(COMPLETED, serde_json::to_value(&completion)?)])
        .await?;

    // The authenticated history verifies with the rebind folded in.
    let replay = seat.replay(&harness).await?;
    assert_eq!(replay.completed_rebinds().len(), 1);
    assert_eq!(replay.composition.surfaces()[&surface].binding(), &to);

    // The next request, owned by the successor, is accepted too.
    let rename_id = "66666666-7777-4888-8999-000000000000";
    let rename = seat.prepared_operation(
        json!({"kind": "composition", "request": {
            "schema": 1,
            "presentation_id": seat.presentation_id(),
            "operation_id": rename_id,
            "expected_revision": replay.revision,
            "expected_next_mount_id": replay.next_mount_id,
            "owner": serde_json::to_value(CompositionOwner::Surface {
                surface_instance_id: surface.clone(),
                binding: to.clone(),
            })?,
            "mutation": serde_json::to_value(CompositionMutation::RenameSurface {
                title: "After rebind".into(),
            })?,
        }}),
        rename_id,
    );
    let rename_completion = replay
        .expected_composition_completion(&rename, maximum)
        .map_err(|error| anyhow::anyhow!("expected rename completion: {error:?}"))?;
    seat.append(&harness, &[(PREPARED, serde_json::to_value(&rename)?)])
        .await?;
    seat.append(
        &harness,
        &[(COMPLETED, serde_json::to_value(&rename_completion)?)],
    )
    .await?;

    // A dispatch on the rebound tile is admitted.
    let (status, admitted) = cookie_post(
        &harness,
        "/ui/api/invocations/dispatch",
        &session_id,
        json!({
            "producer": seat.producer,
            "binding": {
                "binding_attachment_id": to.binding_attachment_id,
                "binding_generation": to.binding_generation,
                "binding_digest": to.binding_digest,
                "coordinate": {"kind": "source", "view_ref": VIEW_REF, "channel": "default"},
                "payload": {"kind": "source_parameters", "params": {"expansions": []}},
            },
        }),
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "dispatch on the rebound tile: {admitted}"
    );

    // The session now holds the successor with the new view content.
    let current = session_current(&harness, &session_id).await?;
    let rebound =
        serde_json::to_string(&descriptor_of(&current, &to.binding_attachment_id)["presentation"])?;
    assert!(
        rebound.contains("marker-v2"),
        "the rebound binding resolves v2"
    );

    // Detaching the settled source is the idempotent no-op.
    let (status, detached) = cookie_post(
        &harness,
        "/ui/api/session/attachments/detach",
        &session_id,
        json!({
            "binding_attachment_id": from.binding_attachment_id,
            "binding_generation": from.binding_generation,
            "binding_digest": from.binding_digest,
        }),
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "detach the settled source: {detached}"
    );
    assert_eq!(
        locate(&detached, "detached")["detached"],
        false,
        "{detached}"
    );

    // Nothing further was announced to either session.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(tap.hints().len(), 1, "still exactly one hint");
    assert!(
        other_tap.hints().is_empty(),
        "an unrelated session hears nothing: {:?}",
        other_tap.hints()
    );
    Ok(())
}
