//! Session-authenticated RyeOS UI seat services for browser renderers.
//!
//! Renderers arrive through a `session:<id>` wrapper and open a seat against
//! one exact retained binding-attachment triple. The triple is only a lookup
//! coordinate: surface/project authority comes from the retained attachment,
//! and placement or display paths cannot rebind it.
//!
//! The signed services use ordinary verified dispatch, like session/current.
//! Here "verified" selects descriptor verification and preservation of the
//! route context; it does not promote a cookie to signing-key authority.
//! Do not mark these lifecycle services session_local: that class is for
//! compiled UI binding dispatch and generic node routes deliberately refuse it.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::NewEventRecord;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_ui_presentation_contract::seat_control::{SEAT_APPEND_RECEIPT_SCHEMA, SeatAppendReceipt};

use crate::browser_session::{BindingAttachmentCoordinate, BrowserSession};
use crate::state::get_ui_state;

mod append_ack;
mod authenticated_history;
mod composition;
pub(crate) mod initialization;
mod transfer_ingress;
#[cfg(test)]
pub(crate) use authenticated_history::accepted_pair_observation_fixture_with_session;
pub(crate) use initialization::{
    require as require_presentation_initialization, require_retired_prefix,
};
#[cfg(test)]
pub(crate) use transfer_ingress::test_cut_from_completed_positive_proof;

fn initial_surface_seeds(
    definition: &ryeos_client_base::presentation::UiCompositionDefinition,
) -> Result<Vec<ryeos_client_base::presentation::PresentationSurfaceSeed>> {
    match definition {
        ryeos_client_base::presentation::UiCompositionDefinition::SharedPresentation { config } => {
            Ok(config.shared_presentation.initial_surfaces.clone())
        }
        ryeos_client_base::presentation::UiCompositionDefinition::Surface { .. } => {
            Err(HandlerError::BadRequest(
                "presentation Prepare requires its admitted Shared Config root".into(),
            )
            .into())
        }
    }
}

#[cfg(test)]
mod initial_surface_contract_tests {
    use super::*;

    fn config(seeds: Value) -> ryeos_client_base::presentation::UiCompositionDefinition {
        serde_json::from_value(json!({"kind":"shared_presentation","config": {
            "category":"ui", "shared_presentation": {
                "schema":1,"title":"Presentation","launcher":"launch","docks":{},
                "dialogs":[{"id":"launch","title":"Launch","view":"view:test/launcher"}],
                "initial_surfaces":seeds,"capabilities":{"sources":[],"affordances":[]}
            }
        }}))
        .unwrap()
    }

    #[test]
    fn empty_config_seeds_are_valid_without_a_fabricated_surface() {
        assert!(
            initial_surface_seeds(&config(json!([])))
                .unwrap()
                .is_empty()
        );
        let surface = serde_json::from_value(
            json!({"kind":"surface","surface":{"name":"not-config","tiles":[]}}),
        )
        .unwrap();
        assert!(initial_surface_seeds(&surface).is_err());
    }

    #[test]
    fn seed_selection_and_label_come_from_the_exact_admitted_definition() {
        let seeds = initial_surface_seeds(&config(json!([
            {"id":"development","title":"Development","surface":"surface:test/development"},
            {"id":"solve","title":"Solve","surface":"surface:test/solve"}
        ])))
        .unwrap();
        assert_eq!(seeds[0].id, "development");
        assert_eq!(seeds[1].title, "Solve");
        assert_eq!(seeds[1].surface, "surface:test/solve");
    }
}

const SEAT_KIND: &str = "seat_session";
const SEAT_EVENT_PREFIX: &str = "seat.";
const SEAT_APPEND_RECEIPT_EVENT: &str = "ui_seat.append_receipt.v2";
const MAX_SEAT_APPEND_EVENTS: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum OpenRequest {
    Observe {
        presentation_id: String,
        thread_id: String,
        chain_root_id: String,
    },
    Prepare {
        creation_request_id: String,
        binding_attachment_id: String,
        binding_generation: u64,
        binding_digest: String,
    },
    Claim {
        schema: u32,
        presentation_id: String,
        thread_id: String,
        chain_root_id: String,
        operation_id: String,
        predecessor: ryeos_client_base::ui::UiSeatControlPredecessor,
        intent_nonce: String,
        claim: ryeos_client_base::ui::UiSeatProducerClaim,
    },
}

use ryeos_ui_presentation_contract::seat_control::{
    SEAT_PRODUCER_EVENT, SEAT_PRODUCER_SCHEMA, UiSeatControlRequest,
    UiSeatProducer as SeatProducer, UiSeatProducerClaim as ProducerClaim,
    UiSeatProducerState as ProducerState,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppendRequest {
    presentation_id: String,
    thread_id: String,
    producer_incarnation: String,
    operation_id: String,
    first_engine_seq: u64,
    last_engine_seq: u64,
    event_count: usize,
    payload_digest: String,
    events: Vec<AppendEvent>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AppendEvent {
    engine_seq: u64,
    event_type: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CloseAction {
    Close,
    Relinquish,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseRequest {
    schema: u32,
    presentation_id: String,
    thread_id: String,
    chain_root_id: String,
    predecessor: ryeos_client_base::ui::UiSeatControlPredecessor,
    intent_nonce: String,
    producer_incarnation: String,
    operation_id: String,
    action: CloseAction,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptLookup {
    thread_id: String,
    producer_incarnation: String,
    operation_id: String,
    first_engine_seq: u64,
    last_engine_seq: u64,
    event_count: usize,
    payload_digest: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptLookupRequest {
    presentation_id: String,
    chain_root_id: String,
    receipt: ReceiptLookup,
}

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

fn browser_session(ctx: &HandlerContext, state: &AppState) -> Result<BrowserSession, HandlerError> {
    let session_id = session_id_from_context(ctx)
        .ok_or_else(|| HandlerError::Forbidden("session cookie required".into()))?;
    get_ui_state(state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))
}

fn seat_owner(session: &BrowserSession) -> String {
    format!("session:{}", session.session_id)
}

fn canonical_digest(value: &str) -> bool {
    lillux::valid_hash(value) && !value.bytes().any(|byte| byte.is_ascii_uppercase())
}

use ryeos_client_base::ui::seat_payload_digest;

fn canonical_operation_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.hyphenated().to_string() == value)
}

fn append_acknowledgement(
    receipt: &SeatAppendReceipt,
    chain_seq: i64,
    receipt_thread_seq: i64,
    receipt_event_hash: &str,
    events: &[ryeos_app::state_store::PersistedEventRecord],
    events_authority: &str,
) -> Value {
    json!({
        "producer_incarnation": receipt.producer_incarnation,
        "operation_id": receipt.operation_id,
        "first_engine_seq": receipt.first_engine_seq.to_string(),
        "last_engine_seq": receipt.last_engine_seq.to_string(),
        "event_count": receipt.event_count,
        "payload_digest": receipt.payload_digest,
        "appended": receipt.event_count,
        "chain_seq": chain_seq.to_string(),
        "receipt_thread_seq": receipt_thread_seq.to_string(),
        "receipt_event_hash": receipt_event_hash,
        "events_authority": events_authority,
        "events": append_ack::wire_events(events),
    })
}

fn current_producer(
    state: &AppState,
    session: &BrowserSession,
    presentation_id: &str,
    detail: &ryeos_app::state_store::ThreadDetail,
) -> Result<Option<SeatProducer>, HandlerError> {
    if state
        .state_store
        .capture_thread_event_read_anchor(&detail.chain_root_id, &detail.thread_id)
        .map_err(|error| HandlerError::Internal(error.to_string()))?
        .is_none()
    {
        // A newly reserved seat has not yet written its initialization. This
        // cannot be treated as absence once a signed history head exists.
        return Ok(None);
    }
    let ui =
        get_ui_state(state).ok_or_else(|| HandlerError::Internal("UI state is absent".into()))?;
    let membership = ui
        .browser_sessions
        .require_presentation(
            &session.session_id,
            presentation_id,
            Some(&detail.thread_id),
            None,
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    composition::read_history(state, detail, &membership.initialization)
        .and_then(|verified| {
            verified.require_ordinary_mutation()?;
            Ok(verified.control)
        })
        .map_err(|error| HandlerError::Conflict(error.to_string()))
}

fn retained_control(
    state: &AppState,
    thread_id: &str,
    operation_id: &str,
) -> Result<Option<SeatProducer>, HandlerError> {
    let detail = state
        .state_store
        .get_thread(thread_id)
        .map_err(|error| HandlerError::Internal(error.to_string()))?
        .ok_or(HandlerError::NotFound)?;
    authenticated_history::retained_control(state, &detail.chain_root_id, thread_id, operation_id)
}

fn append_control(
    state: &AppState,
    session: &BrowserSession,
    detail: &ryeos_app::state_store::ThreadDetail,
    control: &SeatProducer,
    anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
    anchors: &[ryeos_state::AuthoritativeThreadEventReadAnchor],
    retired: &[ryeos_state::RetiredSeatPositivePrefix],
) -> Result<(), HandlerError> {
    control
        .validate()
        .map_err(|error| HandlerError::Internal(error.into()))?;
    let outcome = state
        .threads
        .append_seat_events_and_renew_if_mixed_cut(
            &detail.chain_root_id,
            &detail.thread_id,
            &seat_owner(session),
            &detail.item_ref,
            &detail.executor_ref,
            &[NewEventRecord {
                event_type: SEAT_PRODUCER_EVENT.into(),
                storage_class: "indexed".into(),
                payload: serde_json::to_value(control)
                    .map_err(|error| HandlerError::Internal(error.to_string()))?,
            }],
            anchor,
            anchors,
            retired,
        )
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let records = match outcome {
        ryeos_app::state_store::RunningConditionalEventAppendResult::Committed(records) => records,
        ryeos_app::state_store::RunningConditionalEventAppendResult::Conflict => {
            return Err(HandlerError::Conflict(
                "seat control predecessor changed".into(),
            ));
        }
        ryeos_app::state_store::RunningConditionalEventAppendResult::NotRunning => {
            return Err(HandlerError::Conflict("seat is no longer running".into()));
        }
    };
    if records.len() != 1 {
        return Err(HandlerError::Internal(
            "producer transition acknowledgement is invalid".into(),
        ));
    }
    Ok(())
}

fn presentation_executor_ref(presentation_id: &str, root: &BindingAttachmentCoordinate) -> String {
    format!(
        "client:ryeos/ui-presentation/{presentation_id}/{}:{}:{}",
        root.binding_attachment_id, root.binding_generation, root.binding_digest
    )
}

fn require_presentation_seat(
    state: &AppState,
    session: &BrowserSession,
    presentation_id: &str,
    thread_id: Option<&str>,
    chain_root_id: Option<&str>,
) -> Result<ryeos_app::state_store::ThreadDetail, HandlerError> {
    require_presentation_seat_with_reconcile(
        state,
        session,
        presentation_id,
        thread_id,
        chain_root_id,
    )
    .map(|(detail, _)| detail)
}

fn require_presentation_seat_with_reconcile(
    state: &AppState,
    session: &BrowserSession,
    presentation_id: &str,
    thread_id: Option<&str>,
    chain_root_id: Option<&str>,
) -> Result<(ryeos_app::state_store::ThreadDetail, bool), HandlerError> {
    let (detail, membership) = require_presentation_seat_for_read(
        state,
        session,
        presentation_id,
        thread_id,
        chain_root_id,
    )?;
    let transfer_bearing = reconcile_surface_closures(state, session, &membership, &detail)?;
    Ok((detail, transfer_bearing))
}

/// Read-only seat authorization. Unlike mutation admission, historical replay
/// must not repair transient Surface membership while holding the global seat
/// transition lock through a potentially long signed-history scan.
fn require_presentation_seat_for_read(
    state: &AppState,
    session: &BrowserSession,
    presentation_id: &str,
    thread_id: Option<&str>,
    chain_root_id: Option<&str>,
) -> Result<
    (
        ryeos_app::state_store::ThreadDetail,
        crate::browser_session::PresentationMembership,
    ),
    HandlerError,
> {
    let ui = get_ui_state(state).expect("UiState not set");
    let membership = ui
        .browser_sessions
        .require_presentation(
            &session.session_id,
            presentation_id,
            thread_id,
            chain_root_id,
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let thread = membership
        .thread_id
        .as_deref()
        .ok_or_else(|| HandlerError::Conflict("presentation seat is not prepared".into()))?;
    let detail = require_owned_seat(state, thread, &seat_owner(session))?;
    let attachment = ui
        .browser_sessions
        .resolve_attachment(&session.session_id, &membership.root_coordinate)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    if membership.chain_root_id.as_deref() != Some(detail.chain_root_id.as_str())
        || detail.item_ref != attachment.root_ref
        || detail.executor_ref
            != presentation_executor_ref(presentation_id, &membership.root_coordinate)
    {
        return Err(HandlerError::Forbidden(
            "presentation seat authority does not match retained membership".into(),
        ));
    }
    Ok((detail, membership))
}

/// Revalidate one exact browser-session seat before disclosing each bounded
/// historical stream batch. This is read-only: streaming must neither repair
/// Surface membership nor renew a writer lease. The caller also compares the
/// returned immutable thread association with the one captured at begin.
pub(crate) fn authorize_history_stream_batch(
    state: &AppState,
    session_id: &str,
    presentation_id: &str,
    chain_root_id: &str,
    thread_id: Option<&str>,
) -> Result<ryeos_app::state_store::ThreadDetail, HandlerError> {
    let ui = get_ui_state(state).expect("UiState not set");
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = ui
        .browser_sessions
        .get_session(session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;
    require_presentation_seat_for_read(
        state,
        &session,
        presentation_id,
        thread_id,
        Some(chain_root_id),
    )
    .map(|(detail, _)| detail)
}

/// Send a provisional history frame while the same transition guard that
/// admits seat reads still protects its session association. Never wait for
/// channel capacity under that guard: a full channel asks the producer to
/// yield and retry authorization before another attempt.
pub(crate) fn try_send_authorized_history_frame(
    state: &AppState,
    session_id: &str,
    presentation_id: &str,
    expected: &ryeos_app::state_store::ThreadDetail,
    sender: &tokio::sync::mpsc::Sender<ryeos_app::stream_envelope::RouteStreamEnvelope>,
    frame: &ryeos_app::stream_envelope::RouteStreamEnvelope,
    expected_tip: Option<(&str, u64)>,
) -> Result<bool, HandlerError> {
    let ui = get_ui_state(state).expect("UiState not set");
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = ui
        .browser_sessions
        .get_session(session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;
    let (current, _) = require_presentation_seat_for_read(
        state,
        &session,
        presentation_id,
        Some(&expected.thread_id),
        Some(&expected.chain_root_id),
    )?;
    if current.item_ref != expected.item_ref
        || current.executor_ref != expected.executor_ref
        || current.requested_by != expected.requested_by
    {
        return Err(HandlerError::Forbidden(
            "presentation seat association changed during history stream".into(),
        ));
    }
    if let Some(tip) = expected_tip {
        let anchor = state
            .state_store
            .capture_thread_event_read_anchor(&expected.chain_root_id, &expected.thread_id)
            .map_err(|error| HandlerError::Internal(error.to_string()))?
            .ok_or_else(|| {
                HandlerError::Conflict("seat history signed anchor disappeared".into())
            })?;
        if anchor.tip() != Some(tip) {
            return Err(HandlerError::Conflict(
                "seat history signed tip changed before completion".into(),
            ));
        }
    }
    match sender.try_send(frame.clone()) {
        Ok(()) => Ok(true),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(false),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(HandlerError::Conflict(
            "seat history stream receiver closed".into(),
        )),
    }
}

pub(crate) use authenticated_history::{
    FixedAnchorReplay, FixedAnchorReplayStep, LiveSeatReplayAdvance, VerifiedTransferSeatPrefix,
};
pub(crate) use transfer_ingress::{
    DiscoveredRootTransferCut, FixedTransferCut, read_replay_transfer_cut_at_anchor,
    read_replay_transfer_cut_at_retired_prefix,
};

/// Paired disclosure is atomic with every completed cut, not just the root
/// tip. A full channel releases both exclusions before retrying.
pub(crate) fn try_send_authorized_transfer_history_frame(
    state: &AppState,
    session_id: &str,
    cut: &transfer_ingress::FixedTransferCut,
    sender: &tokio::sync::mpsc::Sender<ryeos_app::stream_envelope::RouteStreamEnvelope>,
    frame: &ryeos_app::stream_envelope::RouteStreamEnvelope,
) -> Result<bool, HandlerError> {
    let ui = get_ui_state(state).expect("UiState not set");
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = ui
        .browser_sessions
        .get_session(session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;
    let (anchors, retired) = cut
        .publication_cut(state, &session)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let target_root = anchors
        .first()
        .ok_or_else(|| {
            HandlerError::Conflict("paired history has no live publication target".into())
        })?
        .chain_root_id();
    let memberships = anchors
        .iter()
        .map(|anchor| {
            session
                .presentations
                .values()
                .find(|membership| {
                    membership.thread_id.as_deref() == Some(anchor.thread_id())
                        && membership.chain_root_id.as_deref() == Some(anchor.chain_root_id())
                })
                .cloned()
                .ok_or_else(|| {
                    HandlerError::Forbidden("paired history membership disappeared".into())
                })
        })
        .chain(retired.iter().map(|prefix| {
            session
                .presentations
                .values()
                .find(|membership| {
                    membership.chain_root_id.as_deref() == Some(prefix.chain_root_id())
                        && membership.thread_id.as_deref() == Some(prefix.chain_root_id())
                })
                .cloned()
                .ok_or_else(|| {
                    HandlerError::Forbidden("retired history membership disappeared".into())
                })
        }))
        .collect::<Result<Vec<_>, _>>()?;
    ui.browser_sessions
        .with_exact_presentation_read_session(
            session_id,
            &memberships,
            state.node_policy.generation_digest(),
            || {
                state.state_store.with_thread_event_read_mixed_cut(
                    target_root,
                    &anchors,
                    &retired,
                    || {
                        // The live session owner is already held. No additional locks,
                        // state/CAS reentry, authorization, or writes inside this callback.
                        match sender.try_send(frame.clone()) {
                            Ok(()) => Ok(true),
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(false),
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                anyhow::bail!("seat history stream receiver closed")
                            }
                        }
                    },
                )
            },
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?
        .map_err(|error| HandlerError::Conflict(error.to_string()))?
        .ok_or_else(|| HandlerError::Conflict("paired history signed cut advanced".into()))
}

/// Complete transport and transient cleanup share one original cut. A full
/// channel commits neither the candidate session nor a completion frame.
pub(crate) fn try_send_reconciled_transfer_complete(
    state: &AppState,
    session_id: &str,
    presentation_id: &str,
    chain_root_id: &str,
    thread_id: &str,
    tip_hash: &str,
    thread_seq: u64,
    cut: &FixedTransferCut,
    replay: &ryeos_client_base::ui::composition_replay::UiCompositionReplay,
    sender: &tokio::sync::mpsc::Sender<ryeos_app::stream_envelope::RouteStreamEnvelope>,
    frame: &ryeos_app::stream_envelope::RouteStreamEnvelope,
) -> Result<bool, HandlerError> {
    let ui = get_ui_state(state).expect("UiState not set");
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = ui
        .browser_sessions
        .get_session(session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;
    let (anchors, retired) = cut
        .publication_cut(state, &session)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    if !anchors.iter().any(|anchor| {
        anchor.chain_root_id() == chain_root_id
            && anchor.thread_id() == thread_id
            && anchor.tip() == Some((tip_hash, thread_seq))
    }) {
        return Err(HandlerError::Conflict(
            "paired completion root tip differs".into(),
        ));
    }
    let membership = session
        .presentations
        .get(presentation_id)
        .filter(|membership| {
            membership.chain_root_id.as_deref() == Some(chain_root_id)
                && membership.thread_id.as_deref() == Some(thread_id)
        })
        .ok_or_else(|| {
            HandlerError::Forbidden("paired completion root membership disappeared".into())
        })?;
    let expected = session.presentations.values().cloned().collect::<Vec<_>>();
    let roots = [(membership, replay, cut)];
    ui.browser_sessions
        .with_reconciled_paired_roots_at_anchors(
            &state.state_store,
            &anchors,
            &retired,
            session_id,
            &expected,
            state.node_policy.generation_digest(),
            &roots,
            |_| match sender.try_send(frame.clone()) {
                Ok(()) => Ok(Some(true)),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Ok(None),
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    anyhow::bail!("seat history stream receiver closed")
                }
            },
        )
        .map_err(|error| HandlerError::Conflict(error.to_string()))
        .map(|sent| sent.unwrap_or(false))
}

/// Repair the transient membership projection from its existing durable seat
/// owner. This is also used before authorization: a lost response after append
/// must not leave a closed Surface usable by another document. The caller holds
/// the seat-transition lock; no producer claim or lease renewal occurs here.
fn reconcile_surface_closures(
    state: &AppState,
    session: &BrowserSession,
    membership: &crate::browser_session::PresentationMembership,
    detail: &ryeos_app::state_store::ThreadDetail,
) -> Result<bool, HandlerError> {
    let ui = get_ui_state(state).expect("UiState not set");
    if state
        .state_store
        .capture_thread_event_read_anchor(&detail.chain_root_id, &detail.thread_id)
        .map_err(|error| HandlerError::Internal(error.to_string()))?
        .is_none()
    {
        // A newly reserved seat has no signed initialization or producer
        // history. Later admission still requires that initialization.
        return Ok(false);
    }
    // A projected row is only a fast positive hint. A missing row requires a
    // complete signed absence proof before Prepare may append initialization;
    // a present row still passes through the authenticated fold below.
    if state
        .state_store
        .thread_events_by_type(
            &detail.thread_id,
            ryeos_client_base::ui::presentation_initialization::PRESENTATION_INITIALIZED_EVENT,
        )
        .map_err(|error| HandlerError::Internal(error.to_string()))?
        .is_empty()
        && authenticated_history::initialization_record(
            state,
            &detail.chain_root_id,
            &detail.thread_id,
        )?
        .is_none()
    {
        return Ok(false);
    }
    let verified = composition::read_history(state, detail, &membership.initialization)
        .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    if verified.transfer_testimony_seen {
        // Session-wide inspection must remain available. A transfer-bearing
        // seat cannot use one-seat composition to retire memberships or
        // activated imports; the paired phase owner will settle them.
        return Ok(true);
    }
    let replay = verified.replay;
    for surface in membership
        .surfaces
        .values()
        .filter(|surface| !surface.closed)
    {
        let Some(closure) = replay.closed_surfaces.get(&surface.surface_instance_id) else {
            continue;
        };
        let coordinate = BindingAttachmentCoordinate {
            binding_attachment_id: closure.binding.binding_attachment_id.clone(),
            binding_generation: closure.binding.binding_generation,
            binding_digest: closure.binding.binding_digest.clone(),
        };
        if closure.presentation_id != membership.presentation_id
            || closure.surface_instance_id != surface.surface_instance_id
            || coordinate != surface.coordinate
        {
            return Err(HandlerError::Internal(
                "retained Surface closure contradicts membership".into(),
            ));
        }
        ui.browser_sessions
            .retire_surface_membership(
                &session.session_id,
                &membership.presentation_id,
                &surface.surface_instance_id,
                &coordinate,
                state.node_policy.generation_digest(),
            )
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    }
    // The same authenticated replay is the current composition after a lost
    // append ACK, retry or document reconnect. Retire only activated imported
    // ownership absent from that signed state; hidden Accepted batches remain
    // governed by their separate activation/abort protocol.
    ui.browser_sessions
        .reconcile_activated_transfer_ownership(
            &session.session_id,
            &membership.presentation_id,
            &membership.root_coordinate,
            &replay,
        )
        .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    Ok(false)
}

/// Session bootstrap observes the same retained closure projection as dispatch.
/// Prepared-but-unbound memberships have no seat history to reconcile.
pub(crate) fn reconcile_session_surface_closures(
    state: &AppState,
    session: &BrowserSession,
) -> Result<
    (
        Vec<crate::browser_session::PresentationMembership>,
        Vec<(
            crate::browser_session::PresentationMembership,
            ryeos_state::RetiredSeatPositivePrefix,
        )>,
    ),
    HandlerError,
> {
    let mut paired = Vec::new();
    let mut retired = Vec::new();
    for membership in session.presentations.values() {
        if let Some(thread_id) = membership.thread_id.as_deref() {
            let root = membership
                .chain_root_id
                .as_deref()
                .ok_or_else(|| HandlerError::Conflict("session seat has no chain root".into()))?;
            if state
                .state_store
                .capture_thread_event_read_anchor(root, thread_id)
                .map_err(|error| HandlerError::Internal(error.to_string()))?
                .is_none()
            {
                if root != thread_id {
                    return Err(HandlerError::Conflict(
                        "retired session seat is not an exact root".into(),
                    ));
                }
                let authority = state
                    .state_store
                    .pinned_state_authority()
                    .map_err(|error| HandlerError::Internal(error.to_string()))?;
                let guard = authority
                    .acquire_shared_guard()
                    .map_err(|error| HandlerError::Internal(error.to_string()))?;
                let prefix = authority
                    .capture_retired_seat_positive_prefix(root, &guard)
                    .map_err(|error| HandlerError::Conflict(error.to_string()))?
                    .ok_or_else(|| {
                        HandlerError::Conflict(
                            "session seat has neither live head nor retired marker".into(),
                        )
                    })?;
                retired.push((membership.clone(), prefix));
                continue;
            }
            let transfer_bearing = match require_presentation_seat_with_reconcile(
                state,
                session,
                &membership.presentation_id,
                None,
                None,
            ) {
                Ok((_, transfer_bearing)) => transfer_bearing,
                // The singleton fold can fail after paired materialization.
                // Treat this only as a request for the full fixed-cut reader;
                // an Empty discovery or failed proof still refuses bootstrap.
                Err(_) => true,
            };
            if transfer_bearing {
                paired.push(membership.clone());
            }
        }
    }
    Ok((paired, retired))
}

fn require_writer(
    state: &AppState,
    session: &BrowserSession,
    presentation_id: &str,
    detail: &ryeos_app::state_store::ThreadDetail,
    incarnation: &str,
) -> Result<SeatProducer, HandlerError> {
    if !canonical_digest(incarnation) {
        return Err(HandlerError::BadRequest(
            "producer incarnation must be canonical".into(),
        ));
    }
    let control = current_producer(state, session, presentation_id, detail)?
        .ok_or_else(|| HandlerError::Conflict("presentation has no producer".into()))?;
    if control.producer_incarnation != incarnation
        || control.state != ProducerState::Active
        || detail.status != "running"
    {
        return Err(HandlerError::Conflict(
            "presentation producer is not current and active".into(),
        ));
    }
    Ok(control)
}

/// Return the exact authenticated frontier used by a subsequent short
/// StateStore action callback. A stale frontier is a retry, never authority.
pub(crate) struct UiSeatActionAdmission {
    pub anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    pub owner: String,
    pub item_ref: String,
    pub executor_ref: String,
}

const PRESENTATION_PREPARED_EVENT: &str = "seat.presentation_operation.prepared";
const PRESENTATION_COMPLETED_EVENT: &str = "seat.presentation_operation.completed";

/// Validate a retained admission description, not layout/tab ownership. The
/// current writer fence is separate: its incarnation may replace the original.
fn validate_prepared_operation(
    state: &AppState,
    session: &BrowserSession,
    operation: &ryeos_client_base::ui::admission_operation::UiPreparedPresentationOperation,
) -> Result<()> {
    operation
        .validate()
        .map_err(|error| HandlerError::BadRequest(error.into()))?;
    let admission = operation.admission().ok_or_else(|| {
        HandlerError::BadRequest(
            "composition operations are not supported until causal validation is integrated".into(),
        )
    })?;
    if !canonical_operation_id(&operation.operation_id)
        || !canonical_digest(&operation.producer.producer_incarnation)
    {
        return Err(HandlerError::BadRequest("invalid prepared operation identity".into()).into());
    }
    let ui = get_ui_state(state).expect("UiState not set");
    let membership = ui
        .browser_sessions
        .require_presentation(
            &session.session_id,
            &operation.producer.presentation_id,
            Some(&operation.producer.thread_id),
            None,
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let root = &operation.root_binding;
    if membership.root_coordinate.binding_attachment_id != root.binding_attachment_id
        || membership.root_coordinate.binding_generation != root.binding_generation
        || membership.root_coordinate.binding_digest != root.binding_digest
        || operation.request_bounds != super::ui_launch_mint::binding_request_bounds(state)?
    {
        return Err(HandlerError::Forbidden(
            "prepared operation root or bounds differ from retained presentation".into(),
        )
        .into());
    }
    let coordinate = BindingAttachmentCoordinate {
        binding_attachment_id: admission.request.binding_attachment_id.clone(),
        binding_generation: admission.request.binding_generation,
        binding_digest: admission.request.binding_digest.clone(),
    };
    let attachment = ui
        .browser_sessions
        .require_presentation_attachment(
            &session.session_id,
            &operation.producer.presentation_id,
            &coordinate,
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    use ryeos_client_base::ids::UiViewMountOwner;
    let owned = match admission.invocation_origin.mount_owner() {
        Some(UiViewMountOwner::Shared { presentation_id }) => {
            presentation_id.as_str() == operation.producer.presentation_id
                && coordinate == membership.root_coordinate
        }
        Some(UiViewMountOwner::Surface {
            presentation_id,
            surface_instance_id,
        }) => {
            presentation_id.as_str() == operation.producer.presentation_id
                && membership
                    .surfaces
                    .get(surface_instance_id.as_str())
                    .is_some_and(|surface| surface.owns_coordinate(&coordinate))
        }
        None => false,
    };
    if !owned {
        return Err(HandlerError::Forbidden(
            "prepared operation origin is not owned by its binding".into(),
        )
        .into());
    }
    super::ui_invocations_dispatch::validate_prepared_operation_binding(&attachment, operation)
}

fn validate_completed_operation(
    state: &AppState,
    session: &BrowserSession,
    producer: &ryeos_client_base::ui::UiPresentationProducer,
    completed: &ryeos_client_base::ui::admission_operation::UiCompletedPresentationOperation,
    prepared: &ryeos_client_base::ui::admission_operation::UiPreparedPresentationOperation,
) -> Result<()> {
    completed
        .validate_for(prepared)
        .map_err(|error| HandlerError::BadRequest(error.into()))?;
    let admission = prepared.admission().ok_or_else(|| {
        HandlerError::BadRequest("composition completion is not an executable admission".into())
    })?;
    if admission.purpose == ryeos_client_base::ui::admission_operation::UiPresentationOperationKind::OpenPresentationDocument {
        let transition:ryeos_client_base::ui::binding::UiPreparedPresentationDocumentTransition=serde_json::from_value(
            completed.result.get("ui_transition").cloned().ok_or_else(||HandlerError::BadRequest("prepared document receipt absent".into()))?
        ).map_err(|error|HandlerError::BadRequest(format!("invalid prepared document: {error}")))?;
        transition.validate().map_err(HandlerError::BadRequest)?;
        if transition.presentation_id!=producer.presentation_id { return Err(HandlerError::Forbidden("prepared document parent differs".into()).into()); }
        let child=&transition.prepared;
        let membership=get_ui_state(state).expect("UiState not set").browser_sessions.require_presentation(
            &session.session_id,&child.presentation_id,Some(&child.thread_id),Some(&child.chain_root_id),state.node_policy.generation_digest(),
        ).map_err(|error|HandlerError::Forbidden(error.to_string()))?;
        if !membership.matches_prepared_document(&completed.operation_id,child) {
            return Err(HandlerError::Forbidden("prepared document differs from retained initialization".into()).into());
        }
        let signed=authenticated_history::initialization_record(state,&child.chain_root_id,&child.thread_id)?
            .ok_or_else(||HandlerError::Conflict("prepared document initialization not durably published".into()))?;
        if signed.initialization!=child.initialization || signed.operation_id!=completed.operation_id {
            return Err(HandlerError::Forbidden("prepared document testimony differs".into()).into());
        }
        state.engine.with_checked_bundle_generation(|generation| initialization::revalidate_current_admission(generation,state,&session.session_id,&membership))?;
        return Ok(());
    }
    if admission.purpose == ryeos_client_base::ui::admission_operation::UiPresentationOperationKind::ImportSurfaceTabs {
        let transition: ryeos_client_base::ui::binding::UiSurfaceTabsAdmissionTransition = serde_json::from_value(
            completed.result.get("ui_transition").cloned().ok_or_else(||HandlerError::BadRequest("completed import has no transition".into()))?
        ).map_err(|error|HandlerError::BadRequest(format!("invalid completed import: {error}")))?;
        transition.validate().map_err(HandlerError::BadRequest)?;
        let ryeos_client_base::surface::opening::SurfaceOpenPlacement::AppendTabs {destination_surface_instance_id,destination_binding,..}=&transition.opening.placement else {
            return Err(HandlerError::BadRequest("import has no destination placement".into()).into());
        };
        let ui=get_ui_state(state).expect("UiState not set");
        let membership=ui.browser_sessions.require_presentation(&session.session_id,&producer.presentation_id,Some(&producer.thread_id),None,state.node_policy.generation_digest())
            .map_err(|error|HandlerError::Forbidden(error.to_string()))?;
        let surface=membership.surfaces.get(destination_surface_instance_id.as_str()).filter(|surface| !surface.closed
            && surface.coordinate.binding_attachment_id==destination_binding.binding_attachment_id
            && surface.coordinate.binding_generation==destination_binding.binding_generation
            && surface.coordinate.binding_digest==destination_binding.binding_digest)
            .ok_or_else(||HandlerError::Forbidden("import completion has no exact live destination".into()))?;
        let retained=surface.tab_openings.get(&completed.operation_id)
            .ok_or_else(||HandlerError::Forbidden("import completion has no retained admission receipt".into()))?;
        if transition.presentation_id!=producer.presentation_id || transition!=retained.result {
            return Err(HandlerError::Forbidden("import completion differs from retained atomic admission".into()).into());
        }
        let bounds=super::ui_launch_mint::binding_request_bounds(state)?;
        for descriptor in std::iter::once(&transition.origin_attachment).chain(&transition.tab_attachments) {
            let coordinate=BindingAttachmentCoordinate {binding_attachment_id:descriptor.binding_attachment_id.clone(),binding_generation:descriptor.binding_generation,binding_digest:descriptor.binding_digest.clone()};
            if !retained.current_coordinates.contains(&coordinate) {
                return Err(HandlerError::Forbidden("import admission attachment was retired".into()).into());
            }
            let admitted=ui.browser_sessions.require_presentation_attachment(&session.session_id,&producer.presentation_id,&coordinate,state.node_policy.generation_digest())
                .map_err(|error|HandlerError::Forbidden(error.to_string()))?;
            if admitted.public_descriptor(bounds)!=*descriptor {
                return Err(HandlerError::Forbidden("import completion attachment changed".into()).into());
            }
        }
        return Ok(());
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Transition {
        kind: String,
        presentation_id: String,
        surface: ryeos_client_base::ui::binding::UiSurfaceMembership,
        attachment: ryeos_client_base::ui::UiBindingAttachment,
        #[serde(default)]
        creation_request_id: Option<String>,
        #[serde(default)]
        tab_attachments: Option<Vec<ryeos_client_base::ui::UiBindingAttachment>>,
        #[serde(default)]
        opening: Option<ryeos_client_base::surface::opening::SurfaceOpenRequest>,
    }
    let transition: Transition =
        serde_json::from_value(completed.result.get("ui_transition").cloned().ok_or_else(
            || HandlerError::BadRequest("completed admission has no transition".into()),
        )?)
        .map_err(|error| {
            HandlerError::BadRequest(format!("invalid completed admission: {error}"))
        })?;
    let membership = get_ui_state(state)
        .expect("UiState not set")
        .browser_sessions
        .require_presentation(
            &session.session_id,
            &producer.presentation_id,
            Some(&producer.thread_id),
            None,
            state.node_policy.generation_digest(),
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let surface = membership
        .surfaces
        .get(&transition.surface.surface_instance_id)
        .filter(|surface| !surface.closed && surface.creation_request_id == completed.operation_id)
        .ok_or_else(|| {
            HandlerError::Forbidden("completion has no exact admitted Surface".into())
        })?;
    let attachment = get_ui_state(state)
        .expect("UiState not set")
        .browser_sessions
        .resolve_attachment(&session.session_id, &surface.coordinate)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    use ryeos_client_base::ui::admission_operation::UiPresentationOperationKind;
    let expected_kind = match admission.purpose {
        UiPresentationOperationKind::OpenSurface => "open_surface",
        UiPresentationOperationKind::ResumeParticularViewSet => "resume_particular_view_set",
        UiPresentationOperationKind::ResumeParticularSurface => "resume_particular_surface",
        UiPresentationOperationKind::ImportSurfaceTabs => unreachable!("handled import receipt"),
        UiPresentationOperationKind::OpenPresentationDocument => {
            unreachable!("handled prepared document receipt")
        }
    };
    if transition.kind != expected_kind
        || (admission.purpose == UiPresentationOperationKind::OpenSurface)
            != transition.opening.is_some()
        || transition.presentation_id != producer.presentation_id
        || transition.surface != surface.public_descriptor()
        || transition.attachment
            != attachment.public_descriptor(super::ui_launch_mint::binding_request_bounds(state)?)
        || transition
            .creation_request_id
            .as_ref()
            .is_some_and(|id| id != &completed.operation_id)
    {
        return Err(HandlerError::Forbidden(
            "completion differs from retained admission membership".into(),
        )
        .into());
    }
    let received_particular_selection = completed
        .result
        .get("particular_selection")
        .cloned()
        .map(
            serde_json::from_value::<
                ryeos_client_base::ui::admission_operation::UiParticularAdmissionSelection,
            >,
        )
        .transpose()
        .map_err(|_| {
            HandlerError::BadRequest(
                "completion has an invalid particular selection receipt".into(),
            )
        })?;
    let retained_particular_selection = match (
        &surface.particular_resume,
        &surface.saved_resume,
        admission.purpose,
    ) {
        (Some(receipt), None, UiPresentationOperationKind::ResumeParticularViewSet) => {
            Some(receipt.particular_selection())
        }
        (None, Some(receipt), UiPresentationOperationKind::ResumeParticularSurface) => {
            receipt.particular_selection()
        }
        (None, None, UiPresentationOperationKind::OpenSurface)
        | (None, Some(_), UiPresentationOperationKind::OpenSurface) => None,
        _ => {
            return Err(HandlerError::Forbidden(
                "completion purpose has no exact retained particular receipt".into(),
            )
            .into());
        }
    };
    if received_particular_selection != retained_particular_selection
        || admission.particular_selection != retained_particular_selection
    {
        return Err(HandlerError::Forbidden(
            "completion particular selection differs from its retained receipt".into(),
        )
        .into());
    }
    match (
        &surface.particular_resume,
        &surface.saved_resume,
        admission.purpose,
    ) {
        (None, None, UiPresentationOperationKind::OpenSurface)
            if transition.creation_request_id.as_deref()
                == Some(completed.operation_id.as_str())
                && transition
                    .tab_attachments
                    .as_ref()
                    .is_some_and(Vec::is_empty) => {}
        (Some(receipt), None, UiPresentationOperationKind::ResumeParticularViewSet)
            if transition.creation_request_id.as_deref()
                == Some(completed.operation_id.as_str())
                && transition.tab_attachments.is_none()
                && completed.result.get("particular_view_set")
                    == Some(&serde_json::to_value(&receipt.resolved)?) => {}
        (None, Some(receipt), purpose)
            if transition.creation_request_id.as_deref()
                == Some(completed.operation_id.as_str())
                && match (&receipt.body, purpose) {
                    (
                        crate::browser_session::SavedSurfaceReceiptBody::Particular { .. },
                        UiPresentationOperationKind::ResumeParticularSurface,
                    ) => {
                        completed.result.get("particular_surface")
                            == Some(&serde_json::to_value(receipt.public_descriptor())?)
                    }
                    (
                        crate::browser_session::SavedSurfaceReceiptBody::Reusable { .. }
                        | crate::browser_session::SavedSurfaceReceiptBody::Composed { .. },
                        UiPresentationOperationKind::OpenSurface,
                    ) => completed.result.get("particular_surface").is_none(),
                    _ => false,
                } =>
        {
            let received = transition.tab_attachments.as_ref().ok_or_else(|| {
                HandlerError::Forbidden(
                    "saved Surface completion has no exact tab attachments".into(),
                )
            })?;
            let mut exact = std::collections::BTreeMap::new();
            for coordinate in receipt.coordinates() {
                let admitted = get_ui_state(state)
                    .expect("UiState not set")
                    .browser_sessions
                    .resolve_attachment(&session.session_id, coordinate)
                    .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
                exact.insert(
                    admitted.binding_attachment_id.clone(),
                    admitted
                        .public_descriptor(super::ui_launch_mint::binding_request_bounds(state)?),
                );
            }
            let mut supplied = std::collections::BTreeMap::new();
            for descriptor in received {
                if supplied
                    .insert(descriptor.binding_attachment_id.clone(), descriptor.clone())
                    .is_some()
                {
                    return Err(HandlerError::Forbidden(
                        "saved Surface completion duplicates tab attachments".into(),
                    )
                    .into());
                }
            }
            if supplied != exact {
                return Err(HandlerError::Forbidden(
                    "saved Surface completion differs from retained tab attachments".into(),
                )
                .into());
            }
        }
        _ => {
            return Err(HandlerError::Forbidden(
                "completion purpose or particular receipt differs".into(),
            )
            .into());
        }
    }
    Ok(())
}

fn producer_response(
    presentation_id: &str,
    detail: &ryeos_app::state_store::ThreadDetail,
    control: &SeatProducer,
) -> Value {
    json!({"presentation_id":presentation_id,"thread_id":detail.thread_id,"chain_root_id":detail.chain_root_id,
        "operation_id":control.operation_id,"producer_incarnation":control.producer_incarnation,
        "state":control.state,"next_engine_seq":control.next_engine_seq.to_string()})
}

fn require_owned_seat(
    state: &AppState,
    thread_id: &str,
    owner: &str,
) -> Result<ryeos_app::state_store::ThreadDetail, HandlerError> {
    let detail = state
        .state_store
        .get_thread(thread_id)
        .map_err(|e| HandlerError::Internal(e.to_string()))?
        .ok_or(HandlerError::NotFound)?;
    if detail.kind != SEAT_KIND {
        return Err(HandlerError::BadRequest(format!(
            "thread {thread_id} is not a seat session"
        )));
    }
    if detail.requested_by.as_deref() != Some(owner) {
        return Err(HandlerError::NotFound);
    }
    Ok(detail)
}

/// Read-only observation uses the same completed paired fold as replay. No
/// session or chain exclusion is held while bounded history pages are read.
async fn observe_completed_seat(
    ctx: &HandlerContext,
    state: Arc<AppState>,
    presentation_id: String,
    thread_id: String,
    chain_root_id: String,
) -> Result<Value> {
    let ui = get_ui_state(&state).expect("UiState not set");
    let (session_id, membership, anchor) = {
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        let session = browser_session(ctx, &state)?;
        let (_, membership) = require_presentation_seat_for_read(
            &state,
            &session,
            &presentation_id,
            Some(&thread_id),
            Some(&chain_root_id),
        )?;
        let anchor = state
            .state_store
            .capture_thread_event_read_anchor(&chain_root_id, &thread_id)?
            .ok_or_else(|| {
                HandlerError::Conflict("producer observation requires signed initialization".into())
            })?;
        (session.session_id, membership, anchor)
    };
    let discovered = read_replay_transfer_cut_at_anchor(
        state.clone(),
        &session_id,
        &presentation_id,
        &chain_root_id,
        &thread_id,
        anchor.clone(),
    )
    .await?;
    let (selected, selected_anchor, cut) = match discovered {
        DiscoveredRootTransferCut::Empty { membership, anchor } => (membership, anchor, None),
        DiscoveredRootTransferCut::Transfers {
            membership,
            anchor,
            cut,
            ..
        } => (membership, anchor, Some(Arc::new(cut))),
    };
    if selected != membership || selected_anchor != anchor {
        return Err(HandlerError::Conflict("producer observation cut changed".into()).into());
    }
    let mut replay = FixedAnchorReplay::begin_at_anchor(
        &state,
        &presentation_id,
        &chain_root_id,
        &thread_id,
        anchor.clone(),
    )?;
    loop {
        let task_state = state.clone();
        let (returned, step) = tokio::task::spawn_blocking(move || {
            let step = replay.next_step(&task_state);
            (replay, step)
        })
        .await
        .map_err(|error| {
            HandlerError::Internal(format!("producer observation reader failed: {error}"))
        })?;
        replay = returned;
        match step? {
            FixedAnchorReplayStep::Batch { .. } => {}
            FixedAnchorReplayStep::RequiresPairedTransfer { anchor: requested } => {
                if requested != anchor {
                    return Err(HandlerError::Conflict(
                        "producer observation handoff changed cut".into(),
                    )
                    .into());
                }
                replay.attach_paired_cut(cut.clone().ok_or_else(|| {
                    HandlerError::Conflict("producer observation paired proof missing".into())
                })?)?;
            }
            FixedAnchorReplayStep::Complete { .. } => break,
        }
    }
    let (revision, control_predecessor, control) = replay.completed_revision_and_control()?;
    use ryeos_client_base::ui::{
        UiBindingAttachmentCoordinate, UiObservedPresentationProducer,
        UiPresentationProducerObservation,
    };
    let producer = match control {
        None => UiObservedPresentationProducer::Unclaimed,
        Some(control) => {
            if !canonical_digest(&control.producer_incarnation) {
                return Err(
                    HandlerError::Internal("invalid retained producer incarnation".into()).into(),
                );
            }
            let producer_incarnation = control.producer_incarnation;
            match control.state {
                ProducerState::Active => UiObservedPresentationProducer::Active {
                    producer_incarnation,
                },
                ProducerState::Relinquished => UiObservedPresentationProducer::Relinquished {
                    producer_incarnation,
                },
                ProducerState::Closed => UiObservedPresentationProducer::Closed {
                    producer_incarnation,
                },
            }
        }
    };
    let root = membership.root_coordinate.clone();
    let observation = UiPresentationProducerObservation {
        presentation_id: presentation_id.clone(),
        thread_id: thread_id.clone(),
        chain_root_id: chain_root_id.clone(),
        root_binding: UiBindingAttachmentCoordinate {
            binding_attachment_id: root.binding_attachment_id,
            binding_generation: root.binding_generation,
            binding_digest: root.binding_digest,
        },
        control_predecessor,
        producer,
        revision,
    };
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(ctx, &state)?;
    let (_, current) = require_presentation_seat_for_read(
        &state,
        &session,
        &presentation_id,
        Some(&thread_id),
        Some(&chain_root_id),
    )?;
    if current != membership {
        return Err(
            HandlerError::Conflict("producer observation membership changed".into()).into(),
        );
    }
    let (anchors, retired) = match cut {
        Some(cut) => cut.publication_cut(&state, &session)?,
        None => (vec![anchor], Vec::new()),
    };
    let memberships = anchors
        .iter()
        .map(|anchor| {
            session
                .presentations
                .values()
                .find(|member| {
                    member.thread_id.as_deref() == Some(anchor.thread_id())
                        && member.chain_root_id.as_deref() == Some(anchor.chain_root_id())
                })
                .cloned()
                .ok_or_else(|| {
                    HandlerError::Forbidden("producer observation dependency disappeared".into())
                })
        })
        .chain(retired.iter().map(|prefix| {
            session
                .presentations
                .values()
                .find(|member| {
                    member.chain_root_id.as_deref() == Some(prefix.chain_root_id())
                        && member.thread_id.as_deref() == Some(prefix.chain_root_id())
                })
                .cloned()
                .ok_or_else(|| {
                    HandlerError::Forbidden(
                        "producer observation retired dependency disappeared".into(),
                    )
                })
        }))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ui
        .browser_sessions
        .with_exact_presentation_read_session(
            &session_id,
            &memberships,
            state.node_policy.generation_digest(),
            || {
                state.state_store.with_thread_event_read_mixed_cut(
                    &chain_root_id,
                    &anchors,
                    &retired,
                    || serde_json::to_value(observation).map_err(anyhow::Error::from),
                )
            },
        )
        .map_err(|error| HandlerError::Forbidden(error.to_string()))??
        .ok_or_else(|| HandlerError::Conflict("producer observation signed cut advanced".into()))?)
}

pub async fn handle_open(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let req: OpenRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid seat open: {error}")))?;
    if let OpenRequest::Observe {
        presentation_id,
        thread_id,
        chain_root_id,
    } = &req
    {
        return observe_completed_seat(
            &ctx,
            state,
            presentation_id.clone(),
            thread_id.clone(),
            chain_root_id.clone(),
        )
        .await;
    }
    let claim_history = if let OpenRequest::Claim {
        presentation_id,
        thread_id,
        ..
    } = &req
    {
        let session = browser_session(&ctx, &state)?;
        Some(
            read_completed_seat_history(state.clone(), &session, presentation_id, thread_id)
                .await?,
        )
    } else {
        None
    };
    let ui = get_ui_state(&state).expect("UiState not set");
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(&ctx, &state)?;
    match req {
        OpenRequest::Observe { .. } => {
            unreachable!("Observe is dispatched before transition exclusion")
        }
        OpenRequest::Prepare {
            creation_request_id,
            binding_attachment_id,
            binding_generation,
            binding_digest,
        } => {
            let root = BindingAttachmentCoordinate {
                binding_attachment_id,
                binding_generation,
                binding_digest,
            };
            let policy = state.node_policy.require::<ryeos_app::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>()?;
            let attachment = ui
                .browser_sessions
                .resolve_attachment(&session.session_id, &root)
                .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
            let seeds = initial_surface_seeds(&attachment.presentation.definition)?;
            let mut membership =
                match ui
                    .browser_sessions
                    .prepared_presentation(
                        &session.session_id,
                        &creation_request_id,
                        &root,
                        state.node_policy.generation_digest(),
                    )
                    .map_err(|error| HandlerError::Conflict(error.to_string()))?
                {
                    Some(membership) => {
                        state
                            .engine
                            .with_checked_bundle_generation(|generation| -> Result<_> {
                                initialization::revalidate_current_admission(
                                    generation,
                                    &state,
                                    &session.session_id,
                                    &membership,
                                )?;
                                ui.browser_sessions.prepare_presentation_with_initial_surfaces(
                        &session.session_id, &creation_request_id, &root, Vec::new(),
                        usize::try_from(policy.max_presentations_per_session)?,
                        usize::try_from(policy.max_live_binding_attachments_per_session)?,
                        usize::try_from(policy.max_surface_instances_per_presentation)?,
                        usize::try_from(policy.shared_presentation.max_label_bytes)?,
                        state.node_policy.generation_digest(),
                        super::ui_launch_mint::binding_request_bounds(&state)?,
                        |candidate, descriptors| {
                            initialization::preflight(&state, candidate)?;
                            super::ui_session_current::preflight_initial_publication(
                                &state, &session, candidate, descriptors,
                            )
                        },
                    ).map_err(|error| HandlerError::Conflict(error.to_string()).into())
                            })?
                    }
                    None => {
                        // Seeds come only from the admitted Config. Each Surface
                        // compiles its own signed closure/capabilities under the
                        // retained caller and the SAME pinned project authority.
                        let compile_context = HandlerContext::new(
                            session.principal_id.clone(),
                            session.granted_caps.clone(),
                            true,
                        );
                        let mut candidates = Vec::with_capacity(seeds.len());
                        for seed in seeds {
                            let request = super::ui_launch_mint::Request {
                            ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION
                                .into(),
                            composition:
                                ryeos_client_base::presentation::UiCompositionSelection::Surface {
                                    surface_ref: seed.surface.clone(),
                                },
                            project_path: attachment.compiled_binding.binding.project_root.clone(),
                            user_principal_id: None,
                        };
                            let (compiled_binding, presentation) =
                                super::ui_launch_mint::compile_session_binding(
                                    &request,
                                    &compile_context,
                                    &state,
                                    attachment.project_authority.as_deref(),
                                )?;
                            candidates.push(crate::browser_session::InitialSurfaceCandidate {
                                seed,
                                candidate: crate::browser_session::BindingAttachmentCandidate {
                                    registered_project_id: attachment.registered_project_id.clone(),
                                    compiled_binding: Arc::new(compiled_binding),
                                    presentation,
                                    project_authority: attachment.project_authority.clone(),
                                },
                            });
                        }
                        let resolution = attachment
                            .project_authority
                            .as_ref()
                            .map(|authority| authority.descriptor_path())
                            .transpose()?;
                        state
                            .engine
                            .with_checked_bundle_generation(|generation| -> Result<_> {
                                let current = generation.request_engine_generation_identity();
                                if current
                                    != attachment
                                        .compiled_binding
                                        .binding
                                        .request_engine_generation_identity
                                    || candidates.iter().any(|candidate| {
                                        current
                                            != candidate
                                                .candidate
                                                .compiled_binding
                                                .binding
                                                .request_engine_generation_identity
                                    })
                                {
                                    return Err(HandlerError::Conflict(
                                    "UI engine generation changed during initial Surface admission"
                                        .into(),
                                )
                                .into());
                                }
                                super::ui_launch_mint::revalidate_binding_authority(
                                    generation,
                                    &state,
                                    attachment.compiled_binding.as_ref(),
                                    resolution.as_deref(),
                                    attachment.project_authority.as_deref(),
                                )?;
                                for candidate in &candidates {
                                    super::ui_launch_mint::revalidate_binding_authority(
                                        generation,
                                        &state,
                                        candidate.candidate.compiled_binding.as_ref(),
                                        resolution.as_deref(),
                                        attachment.project_authority.as_deref(),
                                    )?;
                                }
                                // The store checks root/policy/principal/project identity,
                                // every capacity and counter before committing ANY seed.
                                ui.browser_sessions
                                .prepare_presentation_with_initial_surfaces(
                                    &session.session_id,
                                    &creation_request_id,
                                    &root,
                                    candidates,
                                    usize::try_from(policy.max_presentations_per_session)?,
                                    usize::try_from(
                                        policy.max_live_binding_attachments_per_session,
                                    )?,
                                    usize::try_from(policy.max_surface_instances_per_presentation)?,
                                    usize::try_from(policy.shared_presentation.max_label_bytes)?,
                                    state.node_policy.generation_digest(),
                                    super::ui_launch_mint::binding_request_bounds(&state)?,
                                    |candidate, descriptors| {
                                        initialization::preflight(&state, candidate)?;
                                        super::ui_session_current::preflight_initial_publication(
                                            &state, &session, candidate, descriptors,
                                        )
                                    },
                                )
                                .map_err(|error| HandlerError::Conflict(error.to_string()).into())
                            })?
                    }
                };
            membership = initialization::materialize_unclaimed(&state, &session, membership)?;
            Ok(
                json!({"presentation_id":membership.presentation_id,"thread_id":membership.thread_id,"chain_root_id":membership.chain_root_id,"root_binding":root}),
            )
        }
        OpenRequest::Claim {
            schema,
            presentation_id,
            thread_id,
            chain_root_id,
            operation_id,
            predecessor,
            intent_nonce,
            claim,
        } => {
            let request = UiSeatControlRequest::Claim {
                schema,
                operation_id: operation_id.clone(),
                presentation_id: presentation_id.clone(),
                thread_id: thread_id.clone(),
                chain_root_id: chain_root_id.clone(),
                predecessor: predecessor.clone(),
                intent_nonce,
                claim: claim.clone(),
            };
            request
                .validate()
                .map_err(|error| HandlerError::BadRequest(error.into()))?;
            let history = claim_history
                .as_ref()
                .expect("claim history was read before transition");
            let admission =
                history.authorize_history(&state, &session, &presentation_id, &thread_id)?;
            let (detail, _) = require_presentation_seat_for_read(
                &state,
                &session,
                &presentation_id,
                Some(&thread_id),
                Some(&chain_root_id),
            )?;
            if detail.thread_id != thread_id || detail.chain_root_id != chain_root_id {
                return Err(HandlerError::Conflict("claim seat coordinate changed".into()).into());
            }
            initialization::require(&state, &session, &presentation_id)?;
            if let Some(retained) = retained_control(&state, &detail.thread_id, &operation_id)? {
                if retained.request != request {
                    return Err(HandlerError::Conflict(
                        "claim operation reused with different content".into(),
                    )
                    .into());
                }
                // Repair a delivery failure after the durable claim but before
                // lease publication, without reviving a displaced producer.
                if detail.status != "running"
                    || !history.control().is_some_and(|current| {
                        current.operation_id == retained.operation_id
                            && current.producer_incarnation == retained.producer_incarnation
                            && current.state == ProducerState::Active
                    })
                {
                    return Err(HandlerError::Conflict(
                        "retained claim no longer owns the current active producer".into(),
                    )
                    .into());
                }
                if !state.state_store.touch_seat_lease_if_mixed_cut(
                    &detail.chain_root_id,
                    &detail.thread_id,
                    &admission.action.anchor,
                    &admission.anchors,
                    &admission.retired,
                    &admission.action.owner,
                    &admission.action.item_ref,
                    &admission.action.executor_ref,
                )? {
                    return Err(HandlerError::Conflict("claim lease cut changed".into()).into());
                }
                return Ok(producer_response(&presentation_id, &detail, &retained));
            }
            if detail.status != "running" {
                return Err(
                    HandlerError::Conflict("presentation seat is not running".into()).into(),
                );
            }
            let (current_predecessor, previous, anchor) =
                authenticated_history::control_predecessor(
                    &state,
                    &detail.chain_root_id,
                    &detail.thread_id,
                )?;
            if predecessor != current_predecessor {
                return Err(
                    HandlerError::Conflict("claim control predecessor changed".into()).into(),
                );
            }
            if current_predecessor != history.control_predecessor()
                || serde_json::to_value(&previous)? != serde_json::to_value(history.control())?
                || anchor != admission.action.anchor
            {
                return Err(HandlerError::Conflict(
                    "claim signed control differs from completed history".into(),
                )
                .into());
            }
            let previous_incarnation = match (&claim, &previous) {
                (ProducerClaim::Fresh, None) => None,
                (ProducerClaim::Resume { expected_producer_incarnation }, Some(previous))
                    if previous.state == ProducerState::Relinquished && &previous.producer_incarnation == expected_producer_incarnation => Some(expected_producer_incarnation.clone()),
                (ProducerClaim::Takeover { expected_producer_incarnation }, Some(previous))
                    if previous.state != ProducerState::Closed && &previous.producer_incarnation == expected_producer_incarnation => Some(expected_producer_incarnation.clone()),
                _ => return Err(HandlerError::Conflict("live presentation requires explicit predecessor-fenced takeover or relinquished resume".into()).into()),
            };
            let control = SeatProducer {
                schema_version: SEAT_PRODUCER_SCHEMA.into(),
                request,
                operation_id,
                claim: Some(claim),
                predecessor: previous_incarnation,
                producer_incarnation: lillux::sha256_hex(&rand::random::<[u8; 32]>()),
                state: ProducerState::Active,
                next_engine_seq: history.next_engine_seq(),
            };
            append_control(
                &state,
                &session,
                &detail,
                &control,
                &anchor,
                &admission.anchors,
                &admission.retired,
            )?;
            Ok(producer_response(&presentation_id, &detail, &control))
        }
    }
}

/// Freeze an append base without holding the transition lock across bounded
/// history work. The ordinary checkpoint remains the transfer-free fast path.
async fn append_completed_frontier(
    state: Arc<AppState>,
    session: &BrowserSession,
    presentation_id: &str,
    thread_id: &str,
) -> Result<(
    composition::VerifiedHistory,
    ryeos_state::AuthoritativeThreadEventReadAnchor,
    Option<Arc<FixedTransferCut>>,
    crate::browser_session::PresentationMembership,
)> {
    let (membership, anchor) = {
        let ui = get_ui_state(&state).expect("UiState not set");
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        // The append frontier itself will authenticate closures and paired
        // transfer effects. Singleton repair cannot precede that proof.
        let (detail, membership) = require_presentation_seat_for_read(
            &state,
            session,
            presentation_id,
            Some(thread_id),
            None,
        )?;
        if let Ok((verified, anchor)) =
            composition::verified_frontier(&state, &detail, &membership.initialization)
        {
            return Ok((verified, anchor, None, membership));
        }
        let anchor = state
            .state_store
            .capture_thread_event_read_anchor(&detail.chain_root_id, &detail.thread_id)?
            .ok_or_else(|| {
                HandlerError::Conflict("append requires signed initialization".into())
            })?;
        (membership, anchor)
    };
    let discovered = read_replay_transfer_cut_at_anchor(
        state.clone(),
        &session.session_id,
        presentation_id,
        anchor.chain_root_id(),
        thread_id,
        anchor.clone(),
    )
    .await?;
    let (selected, selected_anchor, cut) = match discovered {
        DiscoveredRootTransferCut::Empty { membership, anchor } => (membership, anchor, None),
        DiscoveredRootTransferCut::Transfers {
            membership,
            anchor,
            cut,
            ..
        } => (membership, anchor, Some(Arc::new(cut))),
    };
    if selected != membership || selected_anchor != anchor {
        return Err(HandlerError::Conflict("append proof cut changed".into()).into());
    }
    let mut replay = FixedAnchorReplay::begin_at_anchor(
        &state,
        presentation_id,
        anchor.chain_root_id(),
        thread_id,
        anchor.clone(),
    )?;
    loop {
        let task_state = state.clone();
        let (returned, step) = tokio::task::spawn_blocking(move || {
            let step = replay.next_step(&task_state);
            (replay, step)
        })
        .await
        .map_err(|error| HandlerError::Internal(format!("append proof reader failed: {error}")))?;
        replay = returned;
        match step? {
            FixedAnchorReplayStep::Batch { .. } => {}
            FixedAnchorReplayStep::RequiresPairedTransfer { anchor: requested } => {
                if requested != anchor {
                    return Err(HandlerError::Conflict("append handoff changed cut".into()).into());
                }
                replay.attach_paired_cut(cut.clone().ok_or_else(|| {
                    HandlerError::Conflict("append paired proof missing".into())
                })?)?;
            }
            FixedAnchorReplayStep::Complete { .. } => break,
        }
    }
    Ok((
        replay.completed_composition_frontier(&membership.initialization)?,
        anchor,
        cut,
        membership,
    ))
}

/// Completed same-store history, not a grant or an executable dispatch token.
pub(crate) struct CompletedSeatIngressRead {
    _state_owner: Arc<AppState>,
    state_identity: usize,
    session_id: String,
    verified: composition::VerifiedHistory,
    anchor: ryeos_state::AuthoritativeThreadEventReadAnchor,
    cut: Option<Arc<FixedTransferCut>>,
    membership: crate::browser_session::PresentationMembership,
}

pub(crate) struct CompletedSeatIngressAdmission {
    pub action: UiSeatActionAdmission,
    pub anchors: Vec<ryeos_state::AuthoritativeThreadEventReadAnchor>,
    pub retired: Vec<ryeos_state::RetiredSeatPositivePrefix>,
}

#[cfg(test)]
mod completed_ingress_scope_tests {
    use super::*;

    #[test]
    fn exact_control_readback_does_not_require_an_active_incarnation() {
        // Predicate coverage only: these closed typed records do not mint
        // signed outcome evidence or qualify terminal lifecycle admission.
        for requested_state in [ProducerState::Closed, ProducerState::Relinquished] {
            let incarnation = "a".repeat(64);
            let mut request = if requested_state == ProducerState::Closed {
                UiSeatControlRequest::Close {
                    schema: 2, operation_id: String::new(), presentation_id: "00000000-0000-4000-8000-000000000097".into(),
                    thread_id: "T-seat".into(), chain_root_id: "T-seat".into(),
                    predecessor: ryeos_ui_presentation_contract::seat_control::UiSeatControlPredecessor::Control { event_hash: "b".repeat(64) },
                    intent_nonce: "00000000-0000-4000-8000-000000000099".into(),
                    producer_incarnation: incarnation.clone(),
                }
            } else {
                UiSeatControlRequest::Relinquish {
                    schema: 2, operation_id: String::new(), presentation_id: "00000000-0000-4000-8000-000000000097".into(),
                    thread_id: "T-seat".into(), chain_root_id: "T-seat".into(),
                    predecessor: ryeos_ui_presentation_contract::seat_control::UiSeatControlPredecessor::Control { event_hash: "b".repeat(64) },
                    intent_nonce: "00000000-0000-4000-8000-000000000099".into(),
                    producer_incarnation: incarnation.clone(),
                }
            };
            request.assign_identity().unwrap();
            let retained = SeatProducer {
                schema_version: SEAT_PRODUCER_SCHEMA.into(),
                operation_id: request.operation_id().into(),
                request: request.clone(),
                claim: None,
                predecessor: Some(incarnation.clone()),
                producer_incarnation: incarnation.clone(),
                state: requested_state.clone(),
                next_engine_seq: 7,
            };
            retained.validate().unwrap();
            assert!(
                validate_exact_control_readback(
                    &retained,
                    &request,
                    requested_state.clone(),
                    &incarnation
                )
                .is_ok()
            );
            assert!(
                validate_exact_control_readback(
                    &retained,
                    &request,
                    requested_state.clone(),
                    &"c".repeat(64)
                )
                .is_err()
            );
            assert!(
                validate_exact_control_readback(
                    &retained,
                    &request,
                    ProducerState::Active,
                    &incarnation
                )
                .is_err()
            );
            let mut altered = request.clone();
            match &mut altered {
                UiSeatControlRequest::Close { intent_nonce, .. }
                | UiSeatControlRequest::Relinquish { intent_nonce, .. } => {
                    *intent_nonce = "00000000-0000-4000-8000-000000000098".into()
                }
                _ => unreachable!(),
            }
            assert!(
                validate_exact_control_readback(&retained, &altered, requested_state, &incarnation)
                    .is_err()
            );
        }
    }

    #[test]
    fn completed_signed_prefix_cannot_be_reused_in_another_store_or_session() {
        // Real signed/CAS completed prefix; session metadata here is only a
        // negative scope fixture, not executable admission or a paired grant.
        let (_directory, state, paired, prefix, _, _, _) =
            authenticated_history::completed_cancellation_fixture();
        let source = &paired.transfer().source;
        let initial = authenticated_history::initialization_record(
            &state,
            &source.chain_root_id,
            &source.thread_id,
        )
        .unwrap()
        .unwrap()
        .initialization;
        let verified = prefix.history(&initial).unwrap();
        let store = crate::browser_session::BrowserSessionStore::new();
        let mut context = crate::browser_session::tests::test_shared_context();
        Arc::make_mut(&mut context.compiled_binding)
            .binding
            .node_policy_generation_digest = state.node_policy.generation_digest().to_owned();
        let (id, token, _) = store
            .mint_token(context, 4, state.node_policy.generation_digest())
            .unwrap();
        store.consume_launch_token(&token).unwrap();
        let session = store.get_session(&id).unwrap();
        let membership = crate::browser_session::PresentationMembership {
            presentation_id: initial.presentation_id.clone(),
            creation_request_id: initial.creation_request_id.clone(),
            root_coordinate: BindingAttachmentCoordinate {
                binding_attachment_id: initial.root_binding.binding_attachment_id.clone(),
                binding_generation: initial.root_binding.binding_generation,
                binding_digest: initial.root_binding.binding_digest.clone(),
            },
            thread_id: Some(source.thread_id.clone()),
            chain_root_id: Some(source.chain_root_id.clone()),
            surfaces: Default::default(),
            activated_transfer_reservations: Default::default(),
            next_mount_id: initial.next_mount_id,
            initialization_digest: seat_payload_digest(&serde_json::to_value(&initial).unwrap())
                .unwrap(),
            initialization: initial,
        };
        let state = Arc::new(state);
        let read = CompletedSeatIngressRead {
            _state_owner: state.clone(),
            state_identity: Arc::as_ptr(&state) as usize,
            session_id: session.session_id.clone(),
            verified,
            anchor: prefix.anchor().clone(),
            cut: None,
            membership,
        };
        let (_other_directory, other) = crate::test_state::build_test_state();
        let error = read
            .authorize_history(&other, &session, &source.presentation_id, &source.thread_id)
            .err()
            .expect("same bytes do not qualify another StateStore");
        assert!(error.to_string().contains("exact session/seat"));
        let mut replacement = session.clone();
        replacement.session_id = "different-session".into();
        let error = read
            .authorize_history(
                &state,
                &replacement,
                &source.presentation_id,
                &source.thread_id,
            )
            .err()
            .expect("same retained prefix does not qualify another browser session");
        assert!(error.to_string().contains("exact session/seat"));
        let error = read
            .authorize_history(
                &state,
                &session,
                "different-presentation",
                &source.thread_id,
            )
            .err()
            .expect("exact root ownership must match");
        assert!(error.to_string().contains("exact session/seat"));
    }
}

pub(crate) async fn read_completed_seat_ingress(
    state: Arc<AppState>,
    session: &BrowserSession,
    producer: &ryeos_client_base::ui::UiPresentationProducer,
) -> Result<CompletedSeatIngressRead> {
    read_completed_seat_history(
        state,
        session,
        &producer.presentation_id,
        &producer.thread_id,
    )
    .await
}

pub(crate) async fn read_completed_seat_history(
    state: Arc<AppState>,
    session: &BrowserSession,
    presentation_id: &str,
    thread_id: &str,
) -> Result<CompletedSeatIngressRead> {
    let state_identity = Arc::as_ptr(&state) as usize;
    let state_owner = state.clone();
    let (verified, anchor, cut, membership) =
        append_completed_frontier(state, session, presentation_id, thread_id).await?;
    Ok(CompletedSeatIngressRead {
        _state_owner: state_owner,
        state_identity,
        session_id: session.session_id.clone(),
        verified,
        anchor,
        cut,
        membership,
    })
}

impl CompletedSeatIngressRead {
    /// Caller holds the transition lock. Actual publication must additionally
    /// hold every returned anchor through the existing atomic action owner.
    pub(crate) fn authorize(
        &self,
        state: &AppState,
        session: &BrowserSession,
        producer: &ryeos_client_base::ui::UiPresentationProducer,
    ) -> Result<CompletedSeatIngressAdmission> {
        let admission = self.authorize_history(
            state,
            session,
            &producer.presentation_id,
            &producer.thread_id,
        )?;
        if !canonical_digest(&producer.producer_incarnation)
            || !self.verified.control.as_ref().is_some_and(|control| {
                control.state == ProducerState::Active
                    && control.producer_incarnation == producer.producer_incarnation
            })
        {
            return Err(HandlerError::Conflict(
                "ingress producer is not current and active".into(),
            )
            .into());
        }
        Ok(admission)
    }

    /// Exact target/cut only: selecting or replacing a producer remains the
    /// control writer's responsibility. This is not active action authority.
    pub(crate) fn authorize_history(
        &self,
        state: &AppState,
        session: &BrowserSession,
        presentation_id: &str,
        thread_id: &str,
    ) -> Result<CompletedSeatIngressAdmission> {
        if state as *const AppState as usize != self.state_identity
            || session.session_id != self.session_id
            || presentation_id != self.membership.presentation_id
            || thread_id != self.anchor.thread_id()
        {
            return Err(HandlerError::Conflict(
                "ingress proof differs from exact session/seat".into(),
            )
            .into());
        }
        let (detail, membership) = require_presentation_seat_for_read(
            state,
            session,
            presentation_id,
            Some(thread_id),
            Some(self.anchor.chain_root_id()),
        )?;
        if membership != self.membership
            || detail.status != "running"
            || detail.chain_root_id != detail.thread_id
        {
            return Err(
                HandlerError::Conflict("ingress producer or membership changed".into()).into(),
            );
        }
        let (anchors, retired) = match &self.cut {
            Some(cut) => cut.publication_cut(state, session)?,
            None => (vec![self.anchor.clone()], Vec::new()),
        };
        if !anchors.iter().any(|anchor| anchor == &self.anchor) {
            return Err(HandlerError::Conflict("ingress cut omitted root".into()).into());
        }
        Ok(CompletedSeatIngressAdmission {
            action: UiSeatActionAdmission {
                anchor: self.anchor.clone(),
                owner: seat_owner(session),
                item_ref: detail.item_ref,
                executor_ref: detail.executor_ref,
            },
            anchors,
            retired,
        })
    }

    pub(crate) fn next_mount_id(&self) -> u64 {
        self.verified.replay.next_mount_id
    }
    pub(crate) fn composition_replay(
        &self,
    ) -> &ryeos_client_base::ui::composition_replay::UiCompositionReplay {
        &self.verified.replay
    }
    pub(crate) fn next_engine_seq(&self) -> u64 {
        self.verified.next_engine_seq
    }
    pub(crate) fn control(&self) -> Option<SeatProducer> {
        self.verified.control.clone()
    }
    pub(crate) fn control_predecessor(
        &self,
    ) -> ryeos_ui_presentation_contract::seat_control::UiSeatControlPredecessor {
        self.verified.control_predecessor.clone()
    }

    pub(crate) fn require_prepared(
        &self,
        state: &AppState,
        session: &BrowserSession,
        producer: &ryeos_client_base::ui::UiPresentationProducer,
        operation_id: &str,
        request: &ryeos_client_base::ui::UiBindingRequest,
    ) -> Result<ryeos_client_base::ui::admission_operation::UiPreparedPresentationOperation> {
        self.authorize(state, session, producer)?;
        if !canonical_operation_id(operation_id) {
            return Err(
                HandlerError::BadRequest("invalid presentation operation UUID".into()).into(),
            );
        }
        let operation = self
            .verified
            .replay
            .pending
            .get(operation_id)
            .cloned()
            .ok_or_else(|| {
                HandlerError::Conflict(
                    "presentation operation has no authenticated pending preparation".into(),
                )
            })?;
        if operation.operation_id != operation_id
            || operation
                .admission()
                .is_none_or(|admission| admission.request != *request)
            || operation.producer.presentation_id != producer.presentation_id
            || operation.producer.thread_id != producer.thread_id
        {
            return Err(HandlerError::Conflict(
                "invocation differs from retained presentation operation".into(),
            )
            .into());
        }
        validate_prepared_operation(state, session, &operation)?;
        Ok(operation)
    }
}

pub async fn handle_append(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui_state = get_ui_state(&state).expect("UiState not set");
    let session = browser_session(&ctx, &state)?;
    let req: AppendRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    if req.events.is_empty() {
        return Err(HandlerError::BadRequest("no events to append".into()).into());
    }
    if req.events.len() > MAX_SEAT_APPEND_EVENTS {
        return Err(HandlerError::BadRequest(format!(
            "seat append exceeds the {MAX_SEAT_APPEND_EVENTS}-event limit"
        ))
        .into());
    }
    if req.event_count != req.events.len() {
        return Err(HandlerError::BadRequest(
            "event_count does not match the encoded event batch".into(),
        )
        .into());
    }
    if !canonical_digest(&req.producer_incarnation) {
        return Err(HandlerError::BadRequest(
            "producer_incarnation must be a canonical sha256 digest".into(),
        )
        .into());
    }
    if !canonical_operation_id(&req.operation_id) {
        return Err(HandlerError::BadRequest(
            "operation_id must be a canonical lowercase UUID".into(),
        )
        .into());
    }
    if !canonical_digest(&req.payload_digest) {
        return Err(HandlerError::BadRequest(
            "payload_digest must be a canonical sha256 digest".into(),
        )
        .into());
    }
    let expected_last = req
        .first_engine_seq
        .checked_add(req.events.len() as u64 - 1)
        .ok_or_else(|| HandlerError::BadRequest("engine sequence interval overflows".into()))?;
    if req.last_engine_seq != expected_last {
        return Err(HandlerError::BadRequest(
            "last_engine_seq does not match the exact event interval".into(),
        )
        .into());
    }
    for (offset, event) in req.events.iter().enumerate() {
        if !event.event_type.starts_with(SEAT_EVENT_PREFIX) {
            return Err(HandlerError::BadRequest(format!(
                "seat braids accept only `{SEAT_EVENT_PREFIX}*` events, got `{}`",
                event.event_type
            ))
            .into());
        }
        // The namespace is not permission to append an arbitrary contract.
        // Initialization is server-owned; producer events use the same closed
        // schema that both adapters replay, with no silently dropped payloads.
        serde_json::from_value::<ryeos_client_base::ui::SeatEventKind>(json!({
            "event_type": event.event_type, "payload": event.payload,
        }))
        .map_err(|error| HandlerError::BadRequest(format!("invalid typed seat event: {error}")))?;
        let expected_seq = req.first_engine_seq + offset as u64;
        if event.engine_seq != expected_seq {
            return Err(HandlerError::BadRequest(format!(
                "seat event engine sequence {} is not the expected {expected_seq}",
                event.engine_seq
            ))
            .into());
        }
    }
    let canonical_events = serde_json::to_value(&req.events)
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let actual_digest = seat_payload_digest(&canonical_events)
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    if req.payload_digest != actual_digest {
        return Err(HandlerError::BadRequest(
            "payload_digest does not match the typed event batch".into(),
        )
        .into());
    }

    let preflight = append_completed_frontier(
        state.clone(),
        &session,
        &req.presentation_id,
        &req.thread_id,
    )
    .await;
    let captured_session_id = session.session_id.clone();
    let _transition = ui_state
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(&ctx, &state)?;
    if session.session_id != captured_session_id {
        return Err(
            HandlerError::Conflict("append session changed during proof read".into()).into(),
        );
    }
    let owner = seat_owner(&session);
    let (detail, membership) = require_presentation_seat_for_read(
        &state,
        &session,
        &req.presentation_id,
        Some(&req.thread_id),
        None,
    )?;
    let expected_operation_id = ryeos_client_base::ui::admission_operation::UiSeatAppendIdentity {
        presentation_id: req.presentation_id.clone(),
        chain_root_id: detail.chain_root_id.clone(),
        thread_id: req.thread_id.clone(),
        producer_incarnation: req.producer_incarnation.clone(),
        first_engine_seq: req.first_engine_seq,
        last_engine_seq: req.last_engine_seq,
        event_count: req.event_count as u64,
        payload_digest: req.payload_digest.clone(),
    }
    .derive_identity()
    .map_err(|error| HandlerError::BadRequest(error.into()))?;
    if req.operation_id != expected_operation_id {
        return Err(HandlerError::BadRequest(
            "seat append operation ID differs from its exact batch commitment".into(),
        )
        .into());
    }
    let receipt = SeatAppendReceipt {
        schema_version: SEAT_APPEND_RECEIPT_SCHEMA.to_string(),
        producer_incarnation: req.producer_incarnation.clone(),
        operation_id: req.operation_id.clone(),
        first_engine_seq: req.first_engine_seq,
        last_engine_seq: req.last_engine_seq,
        event_count: req.event_count,
        payload_digest: req.payload_digest.clone(),
    };
    // Ordinary writes stay on the checkpointed composition path. A complete
    // authenticated receipt scan is reserved for a proven older interval or as
    // recovery when later malformed/incomplete history prevents reconstruction.
    // Returning that evidence does not renew a lease, select a writer, or admit
    // another mutation.
    let exact_retry = || -> Result<Option<Value>> {
        let Some((retained, chain_seq, receipt_thread_seq, receipt_event_hash, events)) =
            authenticated_history::receipt_and_interval(
                &state,
                &req.presentation_id,
                &detail.chain_root_id,
                &detail.thread_id,
                &req.operation_id,
            )?
        else {
            return Ok(None);
        };
        if retained != receipt {
            return Err(HandlerError::BadRequest(
                "operation_id was already used for a different seat append".into(),
            )
            .into());
        }
        Ok(Some(append_acknowledgement(
            &retained,
            chain_seq,
            receipt_thread_seq,
            &receipt_event_hash,
            &events,
            "authoritative",
        )))
    };
    let (verified, append_anchor, paired_cut, captured_membership) = match preflight {
        Ok(frontier) => frontier,
        Err(reconstruction_error) => {
            return exact_retry()?.ok_or(reconstruction_error);
        }
    };
    let authenticated_frontier = verified.next_engine_seq;
    if req.first_engine_seq > authenticated_frontier {
        return Err(HandlerError::BadRequest(format!(
            "seat append sequence gap: expected {authenticated_frontier}, got {}",
            req.first_engine_seq
        ))
        .into());
    }
    if req.first_engine_seq < authenticated_frontier {
        return exact_retry()?.ok_or_else(|| {
            HandlerError::Conflict(
                "historical seat append interval has no matching authenticated receipt".into(),
            )
            .into()
        });
    }

    if captured_membership != membership {
        return Err(
            HandlerError::Conflict("append membership changed during proof read".into()).into(),
        );
    }
    let (append_anchors, append_retired) = match &paired_cut {
        Some(cut) => cut.publication_cut(&state, &session)?,
        None => (vec![append_anchor.clone()], Vec::new()),
    };

    // A retained receipt is the authoritative outcome of an operation and
    // remains replayable even if a later open rotated producer authority.
    // Only a genuinely new write must prove that it owns the current producer.
    state.engine.with_checked_bundle_generation(|generation| -> Result<Value> {
    let producer = if paired_cut.is_some() {
        // The completed paired reader, not the ordinary sealed history path,
        // authenticates this control. Its entire cut is fenced by the writer.
        let producer = verified.control.clone().ok_or_else(|| HandlerError::Conflict("presentation has no producer".into()))?;
        if producer.producer_incarnation != req.producer_incarnation
            || producer.state != ProducerState::Active || detail.status != "running" {
            return Err(HandlerError::Conflict("presentation producer is not current and active".into()).into());
        }
        producer
    } else {
        require_writer(&state, &session, &req.presentation_id, &detail, &req.producer_incarnation)?
    };

    let mut prepared_ids = std::collections::BTreeSet::new();
    let mut prepared_batch = std::collections::BTreeMap::new();
    let mut preceding_closures = std::collections::BTreeSet::new();
    for event in &req.events {
        if event.event_type == ryeos_client_base::ui::seat::SURFACE_CLOSED_EVENT {
            let closure: ryeos_client_base::ui::seat::UiSurfaceClosure =
                serde_json::from_value(event.payload.clone()).map_err(|error| {
                    HandlerError::BadRequest(format!("invalid Surface closure: {error}"))
                })?;
            closure
                .validate()
                .map_err(|error| HandlerError::BadRequest(error.into()))?;
            if !closure_matches_live_surface(
                &closure,
                &req.presentation_id,
                membership.surfaces.get(&closure.surface_instance_id),
            ) || !preceding_closures.insert(closure.surface_instance_id.clone())
            {
                return Err(HandlerError::Conflict(
                    "Surface closure does not name one exact live membership".into(),
                )
                .into());
            }
            continue;
        }
        if event.event_type != PRESENTATION_PREPARED_EVENT {
            continue;
        }
        let operation: ryeos_client_base::ui::admission_operation::UiPreparedPresentationOperation =
            serde_json::from_value(event.payload.clone()).map_err(|error| {
                HandlerError::BadRequest(format!(
                    "invalid prepared presentation operation: {error}"
                ))
            })?;
        reject_closed_origin(&operation, &preceding_closures)?;
        if operation.producer.presentation_id != req.presentation_id
            || operation.producer.thread_id != req.thread_id
            || operation.producer.producer_incarnation != req.producer_incarnation
            || operation.prepared_engine_seq != event.engine_seq
            || operation.validate_identity().is_err()
            || !prepared_ids.insert(operation.operation_id.clone())
        {
            return Err(HandlerError::Conflict(
                "prepared operation writer or unique identity does not match append".into(),
            )
            .into());
        }
        if operation.admission().is_some() {
            validate_prepared_operation(&state, &session, &operation)?;
        }
        prepared_batch.insert(operation.operation_id.clone(), operation);
    }
    let current = ryeos_client_base::ui::UiPresentationProducer {
        presentation_id: req.presentation_id.clone(),
        thread_id: req.thread_id.clone(),
        producer_incarnation: req.producer_incarnation.clone(),
    };
    let mut completed_ids = std::collections::BTreeSet::new();
    let mut preceding_preparations = std::collections::BTreeSet::new();
    preceding_closures.clear();
    for event in &req.events {
        if event.event_type == ryeos_client_base::ui::seat::SURFACE_CLOSED_EVENT {
            if let Some(id) = event
                .payload
                .get("surface_instance_id")
                .and_then(Value::as_str)
            {
                preceding_closures.insert(id.to_owned());
            }
        }
        if event.event_type == PRESENTATION_PREPARED_EVENT {
            if let Some(id) = event.payload.get("operation_id").and_then(Value::as_str) {
                preceding_preparations.insert(id.to_owned());
            }
        }
        if event.event_type != PRESENTATION_COMPLETED_EVENT {
            continue;
        }
        let completed: ryeos_client_base::ui::admission_operation::UiCompletedPresentationOperation =
            serde_json::from_value(event.payload.clone()).map_err(|error|
                HandlerError::BadRequest(format!("invalid presentation completion: {error}")))?;
        if !canonical_operation_id(&completed.operation_id)
            || !completed_ids.insert(completed.operation_id.clone())
            || verified.replay.settled.contains(&completed.operation_id)
        {
            return Err(HandlerError::Conflict(
                "completion has no unique canonical operation".into(),
            )
            .into());
        }
        let prepared = match prepared_batch.get(&completed.operation_id) {
            Some(prepared) if preceding_preparations.contains(&completed.operation_id) => {
                prepared.clone()
            }
            Some(_) => {
                return Err(HandlerError::Conflict(
                    "completion precedes its prepared operation".into(),
                )
                .into());
            }
            None => {
                let prepared = verified
                    .replay
                    .pending
                    .get(&completed.operation_id)
                    .cloned()
                    .ok_or_else(|| HandlerError::Conflict(
                        "completion has no authenticated pending preparation".into(),
                    ))?;
                if prepared.admission().is_some() {
                    validate_prepared_operation(&state, &session, &prepared)?;
                }
                prepared
            }
        };
        if prepared.operation_id != completed.operation_id
            || prepared.producer.presentation_id != req.presentation_id
            || prepared.producer.thread_id != req.thread_id
        {
            return Err(HandlerError::Forbidden(
                "completion preparation belongs to another presentation".into(),
            )
            .into());
        }
        reject_closed_origin(&prepared, &preceding_closures)?;
        if completed
            .result
            .pointer("/ui_transition/surface/surface_instance_id")
            .and_then(Value::as_str)
            .is_some_and(|id| preceding_closures.contains(id))
        {
            return Err(HandlerError::Conflict(
                "completion follows closure of its target Surface".into(),
            )
            .into());
        }
        if prepared.admission().is_some() {
            validate_completed_operation(&state, &session, &current, &completed, &prepared)?;
        }
    }

    if detail.status != "running" {
        return Err(HandlerError::BadRequest(format!(
            "seat session is {}; only running seats accept new events",
            detail.status
        ))
        .into());
    }

    if producer.next_engine_seq > authenticated_frontier {
        return Err(HandlerError::Conflict(
            "seat producer frontier exceeds authenticated history".into(),
        )
        .into());
    }

    let prospective = composition::validate_prospective_append(
        generation,&state,&session,&membership,&detail,&req,&verified,
    )?;

    let mut records: Vec<NewEventRecord> = req
        .events
        .into_iter()
        .map(|event| {
            let mut payload = json!({
                "seq": event.engine_seq,
                "payload": event.payload,
            });
            if event.event_type == PRESENTATION_PREPARED_EVENT
                || event.event_type == PRESENTATION_COMPLETED_EVENT
            {
                payload["operation_id"] = payload["payload"]["operation_id"].clone();
            }
            if event.event_type == ryeos_client_base::ui::seat::SURFACE_CLOSED_EVENT {
                payload["operation_id"] = payload["payload"]["surface_instance_id"].clone();
            }
            NewEventRecord {
                event_type: event.event_type,
                storage_class: "indexed".to_string(),
                payload,
            }
        })
        .collect();
    records.push(NewEventRecord {
        event_type: SEAT_APPEND_RECEIPT_EVENT.to_string(),
        storage_class: "indexed".to_string(),
        payload: serde_json::to_value(&receipt)
            .map_err(|error| HandlerError::Internal(error.to_string()))?,
    });
    let persisted = match state.threads.append_seat_events_and_renew_if_mixed_cut(
        &detail.chain_root_id,
        &detail.thread_id,
        &owner,
        &detail.item_ref,
        &detail.executor_ref,
        &records,
        &append_anchor,
        &append_anchors,
        &append_retired,
    )? {
        ryeos_app::state_store::RunningConditionalEventAppendResult::Committed(records) => records,
        ryeos_app::state_store::RunningConditionalEventAppendResult::Conflict => {
            return Err(HandlerError::Conflict(
                "seat history advanced during conditional append; reconcile before retry".into(),
            )
            .into());
        }
        ryeos_app::state_store::RunningConditionalEventAppendResult::NotRunning => {
            return Err(HandlerError::Conflict(
                "seat is no longer running at the conditional append fence".into(),
            )
            .into());
        }
    };
    // The append owner has already committed and reconciled these records.
    // Do not replace that result with a second read of rebuildable projection.
    let (receipt_chain_seq, acknowledged) = append_ack::from_committed_append(
        &detail.chain_root_id,
        &detail.thread_id,
        &receipt,
        &persisted,
    )?;
    let committed_receipt = persisted.last().expect("validated append includes receipt");
    let receipt_hash = committed_receipt
        .event_hash
        .as_deref()
        .expect("validated receipt has canonical hash");

    // CAS already committed. Rebuildable session repair cannot change that
    // outcome into a refused append; current admission still fails closed on
    // stale membership until authenticated reconciliation repairs it.
    if let Err(error) = ui_state.browser_sessions.reconcile_presentation_mount_frontier(
        &session.session_id,&membership.presentation_id,&membership.root_coordinate,
        state.node_policy.generation_digest(),prospective.next_mount_id,
    ) {
        tracing::warn!(%error, presentation_id = %membership.presentation_id,
            "seat append committed; mount frontier reconciliation deferred");
    }
    if paired_cut.is_none() {
        if let Err(error) = reconcile_surface_closures(&state, &session, &membership, &detail) {
            tracing::warn!(%error, presentation_id = %membership.presentation_id,
                "seat append committed; Surface closure reconciliation deferred");
        }
    }
    Ok(append_acknowledgement(
        &receipt,
        receipt_chain_seq,
        committed_receipt.thread_seq,
        receipt_hash,
        acknowledged,
        "authoritative",
    ))
    })
}

fn reject_closed_origin(
    operation: &ryeos_client_base::ui::admission_operation::UiPreparedPresentationOperation,
    preceding_closures: &std::collections::BTreeSet<String>,
) -> Result<(), HandlerError> {
    if let Some(freeze) = operation.move_freeze() {
        use ryeos_client_base::ids::UiViewMountOwner;
        let is_closed = |owner: UiViewMountOwner| {
            matches!(owner,
            UiViewMountOwner::Surface { surface_instance_id, .. }
                if preceding_closures.contains(surface_instance_id.as_str()))
        };
        // Empty exported sets still have an exact Surface owner; inspecting
        // mounted Views alone would miss their lifetime boundary.
        let closed_set = freeze
            .export
            .sets()
            .is_some_and(|sets| sets.iter().any(|set| is_closed(set.source_owner.clone())));
        let closed_mount = freeze
            .export
            .mounts()
            .iter()
            .any(|mount| mount.source_instance.mount_owner().is_some_and(&is_closed));
        return if closed_set || closed_mount {
            Err(HandlerError::Conflict(
                "MOVE freeze follows closure of a source Surface".into(),
            ))
        } else {
            // This checks closure ordering only. Exact freeze identity,
            // inventory and source state remain validated by the replay owner.
            Ok(())
        };
    }
    if let Some(composition) = operation.composition() {
        return if composition
            .owner
            .surface_instance_id()
            .is_some_and(|id| preceding_closures.contains(id.as_str()))
        {
            Err(HandlerError::Conflict(
                "composition operation follows closure of its owner Surface".into(),
            ))
        } else {
            Ok(())
        };
    }
    let admission = operation
        .admission()
        .ok_or_else(|| HandlerError::BadRequest("unsupported preparation body".into()))?;
    if let Some(ryeos_client_base::ids::UiViewMountOwner::Surface {
        surface_instance_id,
        ..
    }) = admission.invocation_origin.mount_owner()
        && preceding_closures.contains(surface_instance_id.as_str())
    {
        return Err(HandlerError::Conflict(
            "presentation operation follows closure of its origin Surface".into(),
        ));
    }
    Ok(())
}

fn closure_matches_live_surface(
    closure: &ryeos_client_base::ui::seat::UiSurfaceClosure,
    presentation_id: &str,
    surface: Option<&crate::browser_session::SurfaceMembership>,
) -> bool {
    closure.validate().is_ok()
        && closure.presentation_id == presentation_id
        && surface.is_some_and(|surface| {
            !surface.closed
                && surface.surface_instance_id == closure.surface_instance_id
                && surface.coordinate.binding_attachment_id == closure.binding.binding_attachment_id
                && surface.coordinate.binding_generation == closure.binding.binding_generation
                && surface.coordinate.binding_digest == closure.binding.binding_digest
        })
}

/// The retained `ui.seat.replay` route now serves only exact append-receipt
/// reconciliation. Cold history has one fixed-anchor provisional stream owner.
pub async fn handle_receipt_lookup(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui = get_ui_state(&state).expect("UiState not set");
    let req: ReceiptLookupRequest = serde_json::from_value(params).map_err(|error| {
        HandlerError::BadRequest(format!("invalid seat receipt lookup: {error}"))
    })?;
    let (detail, receipt_anchor) = {
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        let session = browser_session(&ctx, &state)?;
        let detail = require_presentation_seat_for_read(
            &state,
            &session,
            &req.presentation_id,
            None,
            Some(&req.chain_root_id),
        )?
        .0;
        let anchor = state
            .state_store
            .capture_thread_event_read_anchor(&detail.chain_root_id, &detail.thread_id)
            .map_err(|error| HandlerError::Internal(error.to_string()))?
            .ok_or_else(|| {
                HandlerError::Conflict("seat receipt history has no signed anchor".into())
            })?;
        (detail, anchor)
    };
    // This may walk a long immutable signed prefix. No global UI transition
    // lock is held, and no candidate is disclosed until the exact session and
    // seat membership are revalidated below.
    let response = read_receipt_result(&state, &req, &detail, &receipt_anchor)?;
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(&ctx, &state)?;
    let (current, _) = require_presentation_seat_for_read(
        &state,
        &session,
        &req.presentation_id,
        Some(&detail.thread_id),
        Some(&detail.chain_root_id),
    )?;
    if current.item_ref != detail.item_ref
        || current.executor_ref != detail.executor_ref
        || current.requested_by != detail.requested_by
    {
        return Err(HandlerError::Forbidden(
            "presentation seat association changed during history read".into(),
        )
        .into());
    }
    if response["receipt_status"] == "not_found" {
        let current_anchor = state
            .state_store
            .capture_thread_event_read_anchor(&detail.chain_root_id, &detail.thread_id)
            .map_err(|error| HandlerError::Internal(error.to_string()))?
            .ok_or_else(|| HandlerError::Conflict("seat receipt history disappeared".into()))?;
        if current_anchor.tip() != receipt_anchor.tip() {
            return Err(HandlerError::Conflict(
                "seat receipt history advanced during negative lookup; retry exact request".into(),
            )
            .into());
        }
    }
    Ok(response)
}

fn read_receipt_result(
    state: &AppState,
    req: &ReceiptLookupRequest,
    detail: &ryeos_app::state_store::ThreadDetail,
    receipt_anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
) -> Result<Value> {
    let lookup = &req.receipt;
    if lookup.thread_id != detail.thread_id
        || !canonical_operation_id(&lookup.operation_id)
        || !canonical_digest(&lookup.producer_incarnation)
        || !canonical_digest(&lookup.payload_digest)
        || lookup.event_count == 0
        || lookup.event_count > MAX_SEAT_APPEND_EVENTS
        || lookup
            .first_engine_seq
            .checked_add(lookup.event_count as u64 - 1)
            != Some(lookup.last_engine_seq)
    {
        return Err(HandlerError::BadRequest("receipt lookup coordinate is invalid".into()).into());
    }
    if let Some((receipt, chain_seq, receipt_thread_seq, receipt_event_hash, events)) =
        authenticated_history::receipt_and_interval_at_anchor(
            state,
            &req.presentation_id,
            &detail.chain_root_id,
            &detail.thread_id,
            &lookup.operation_id,
            receipt_anchor,
        )?
    {
        if receipt.schema_version != SEAT_APPEND_RECEIPT_SCHEMA
            || receipt.producer_incarnation != lookup.producer_incarnation
            || receipt.first_engine_seq != lookup.first_engine_seq
            || receipt.last_engine_seq != lookup.last_engine_seq
            || receipt.event_count != lookup.event_count
            || receipt.payload_digest != lookup.payload_digest
        {
            return Err(HandlerError::Conflict(
                "receipt lookup content contradicts retained operation".into(),
            )
            .into());
        }
        return Ok(json!({
            "receipt_status":"found",
            "receipt":append_acknowledgement(&receipt,chain_seq,receipt_thread_seq,&receipt_event_hash,&events,"authoritative")
        }));
    }
    Ok(json!({"receipt_status":"not_found","receipt":null}))
}

async fn defer_closed_seat_settlement(
    state: Arc<AppState>,
    session_id: &str,
    presentation_id: &str,
    detail: &ryeos_app::state_store::ThreadDetail,
    closed: &SeatProducer,
) {
    if detail.status == "running" {
        // The old pre-append cut is never reused. Rebuild complete history at
        // the post-Closed tip without holding the UI transition lock, then
        // claim only if that exact signed tip and every dependency still hold.
        // A later suffix safely defers to ordinary expiry maintenance.
        let attempt = async {
            let ui = get_ui_state(&state).ok_or_else(|| anyhow::anyhow!("UiState not set"))?;
            let session = ui
                .browser_sessions
                .get_session(session_id)
                .ok_or_else(|| anyhow::anyhow!("closed seat session expired"))?;
            let read = read_completed_seat_history(
                state.clone(),
                &session,
                presentation_id,
                &detail.thread_id,
            )
            .await?;
            let _transition = ui
                .lock_seat_transition()
                .map_err(|_| anyhow::anyhow!("seat transition lock poisoned"))?;
            let session = ui
                .browser_sessions
                .get_session(session_id)
                .ok_or_else(|| anyhow::anyhow!("closed seat session expired"))?;
            let admission =
                read.authorize_history(&state, &session, presentation_id, &detail.thread_id)?;
            let observed = read
                .control()
                .ok_or_else(|| anyhow::anyhow!("closed control absent"))?;
            if serde_json::to_value(&observed)? != serde_json::to_value(closed)? {
                anyhow::bail!("completed closed control changed before lease claim");
            }
            state.state_store.claim_closed_seat_lease_if_tip(
                &admission.action.anchor,
                &admission.anchors,
                &admission.retired,
                &observed,
                &admission.action.owner,
                &admission.action.item_ref,
                &admission.action.executor_ref,
            )
        }
        .await;
        match attempt {
            Ok(true) => tracing::debug!(thread_id = %detail.thread_id,
                "closed seat handed to lifecycle settlement"),
            Ok(false) => tracing::debug!(thread_id = %detail.thread_id,
                "closed seat prompt claim deferred after signed cut change"),
            Err(error) => tracing::warn!(thread_id = %detail.thread_id, %error,
                "closed seat prompt settlement deferred"),
        }
        return;
    }
    if let Err(error) = state
        .state_store
        .remove_terminal_seat_lease_if_proved(&detail.thread_id)
    {
        // Cleanup cannot turn an exact retained control acknowledgement into
        // a reported refusal. The terminal owner can retry this repair.
        tracing::warn!(thread_id = %detail.thread_id, %error, "closed seat terminal lease cleanup deferred");
    }
}

fn validate_exact_control_readback(
    retained: &SeatProducer,
    request: &UiSeatControlRequest,
    requested_state: ProducerState,
    incarnation: &str,
) -> Result<(), HandlerError> {
    if &retained.request != request
        || retained.state != requested_state
        || retained.producer_incarnation != incarnation
    {
        return Err(HandlerError::Conflict(
            "transition operation reused with different content".into(),
        ));
    }
    Ok(())
}

pub async fn handle_close(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui = get_ui_state(&state).expect("UiState not set");
    let req: CloseRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid seat close: {error}")))?;
    let initial_session = browser_session(&ctx, &state)?;
    let history = read_completed_seat_history(
        state.clone(),
        &initial_session,
        &req.presentation_id,
        &req.thread_id,
    )
    .await;
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(&ctx, &state)?;
    let request = match req.action {
        CloseAction::Close => UiSeatControlRequest::Close {
            schema: req.schema,
            operation_id: req.operation_id.clone(),
            presentation_id: req.presentation_id.clone(),
            thread_id: req.thread_id.clone(),
            chain_root_id: req.chain_root_id.clone(),
            predecessor: req.predecessor.clone(),
            intent_nonce: req.intent_nonce.clone(),
            producer_incarnation: req.producer_incarnation.clone(),
        },
        CloseAction::Relinquish => UiSeatControlRequest::Relinquish {
            schema: req.schema,
            operation_id: req.operation_id.clone(),
            presentation_id: req.presentation_id.clone(),
            thread_id: req.thread_id.clone(),
            chain_root_id: req.chain_root_id.clone(),
            predecessor: req.predecessor.clone(),
            intent_nonce: req.intent_nonce.clone(),
            producer_incarnation: req.producer_incarnation.clone(),
        },
    };
    request
        .validate()
        .map_err(|error| HandlerError::BadRequest(error.into()))?;
    let (detail, _) = require_presentation_seat_for_read(
        &state,
        &session,
        &req.presentation_id,
        Some(&req.thread_id),
        None,
    )?;
    if detail.chain_root_id != req.chain_root_id {
        return Err(HandlerError::Conflict("transition seat coordinate changed".into()).into());
    }
    let requested_state = match req.action {
        CloseAction::Close => ProducerState::Closed,
        CloseAction::Relinquish => ProducerState::Relinquished,
    };
    if let Some(retained) = retained_control(&state, &detail.thread_id, &req.operation_id)? {
        validate_exact_control_readback(
            &retained,
            &request,
            requested_state,
            &req.producer_incarnation,
        )?;
        let response = producer_response(&req.presentation_id, &detail, &retained);
        if retained.state == ProducerState::Closed {
            let session_id = session.session_id.clone();
            drop(_transition);
            defer_closed_seat_settlement(
                state.clone(),
                &session_id,
                &req.presentation_id,
                &detail,
                &retained,
            )
            .await;
        }
        return Ok(response);
    }
    // An exact signed control outcome is readback, not permission to append.
    // Terminal markers (or a newer incompatible proof scope) may correctly
    // prevent a fresh mutation proof without erasing that original outcome.
    let history = history?;
    let admission =
        history.authorize_history(&state, &session, &req.presentation_id, &req.thread_id)?;
    let (current_predecessor, _, anchor) = authenticated_history::control_predecessor(
        &state,
        &detail.chain_root_id,
        &detail.thread_id,
    )?;
    if current_predecessor != req.predecessor {
        return Err(HandlerError::Conflict("transition control predecessor changed".into()).into());
    }
    if current_predecessor != history.control_predecessor() || anchor != admission.action.anchor {
        return Err(HandlerError::Conflict(
            "transition signed control differs from completed history".into(),
        )
        .into());
    }
    let current = history
        .control()
        .filter(|control| {
            control.state == ProducerState::Active
                && control.producer_incarnation == req.producer_incarnation
        })
        .ok_or_else(|| {
            HandlerError::Conflict("presentation producer is not current and active".into())
        })?;
    let transition = SeatProducer {
        schema_version: SEAT_PRODUCER_SCHEMA.into(),
        request,
        operation_id: req.operation_id,
        claim: None,
        predecessor: Some(current.producer_incarnation.clone()),
        producer_incarnation: current.producer_incarnation,
        state: requested_state,
        next_engine_seq: history.next_engine_seq(),
    };
    append_control(
        &state,
        &session,
        &detail,
        &transition,
        &anchor,
        &admission.anchors,
        &admission.retired,
    )?;
    let response = producer_response(&req.presentation_id, &detail, &transition);
    if transition.state == ProducerState::Closed {
        let session_id = session.session_id.clone();
        drop(_transition);
        defer_closed_seat_settlement(
            state.clone(),
            &session_id,
            &req.presentation_id,
            &detail,
            &transition,
        )
        .await;
    }
    Ok(response)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TouchRequest {
    presentation_id: String,
    thread_id: String,
    producer_incarnation: String,
}

pub async fn handle_touch(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let ui = get_ui_state(&state).expect("UiState not set");
    let req: TouchRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid seat touch: {error}")))?;
    let initial_session = browser_session(&ctx, &state)?;
    let history = read_completed_seat_history(
        state.clone(),
        &initial_session,
        &req.presentation_id,
        &req.thread_id,
    )
    .await?;
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = browser_session(&ctx, &state)?;
    let admission =
        history.authorize_history(&state, &session, &req.presentation_id, &req.thread_id)?;
    if !canonical_digest(&req.producer_incarnation)
        || !history.control().is_some_and(|control| {
            control.state == ProducerState::Active
                && control.producer_incarnation == req.producer_incarnation
        })
    {
        return Err(HandlerError::Conflict(
            "presentation producer is not current and active".into(),
        )
        .into());
    }
    if !state.state_store.touch_seat_lease_if_mixed_cut(
        admission.action.anchor.chain_root_id(),
        &req.thread_id,
        &admission.action.anchor,
        &admission.anchors,
        &admission.retired,
        &admission.action.owner,
        &admission.action.item_ref,
        &admission.action.executor_ref,
    )? {
        return Err(HandlerError::Conflict("seat lease cut changed".into()).into());
    }
    Ok(json!({"presentation_id":req.presentation_id,"thread_id":req.thread_id,"touched":true}))
}

pub static OPEN_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/open",
    endpoint: "ui.seat.open",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_open(params, ctx, state)),
};

pub static APPEND_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/append",
    endpoint: "ui.seat.append",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_append(params, ctx, state)),
};

/// Transfer testimony has its own authenticated paired publication owner.
/// Ordinary append must never become an alternative transfer writer.
pub static TRANSFER_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/transfer",
    endpoint: "ui.seat.transfer",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(transfer_ingress::handle_transfer(params, ctx, state)),
};

pub static REPLAY_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/replay",
    endpoint: "ui.seat.replay",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_receipt_lookup(params, ctx, state)),
};

pub static CLOSE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/close",
    endpoint: "ui.seat.close",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_close(params, ctx, state)),
};

pub static TOUCH_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/seat/touch",
    endpoint: "ui.seat.touch",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(handle_touch(params, ctx, state)),
};

#[cfg(test)]
mod digest_tests {
    use super::*;

    #[test]
    fn move_freeze_closure_guard_covers_mounts_empty_sets_and_not_shared_docks() {
        use ryeos_client_base::ids::{
            PresentationId, RyeOsViewInstanceKey, SurfaceInstanceId, TileId, UiViewMountOwner,
            ViewSetId,
        };
        use ryeos_client_base::ui::admission_operation::{
            UiPreparedPresentationOperation, UiPreparedPresentationOperationBody,
        };
        use ryeos_client_base::ui::transfer_export::{
            UiTransferExportContent, UiTransferSetExport,
        };
        let (_directory, state, _session, seat) =
            authenticated_history::ordinary_session_replacement_fixture();
        let mut operation = state
            .state_store
            .thread_events_by_type(&seat.thread_id, PRESENTATION_PREPARED_EVENT)
            .unwrap()
            .into_iter()
            .filter_map(|event| {
                serde_json::from_value::<UiPreparedPresentationOperation>(
                    event.payload["payload"].clone(),
                )
                .ok()
            })
            .find(|operation| operation.move_freeze().is_some())
            .unwrap();
        operation.validate().unwrap();
        let closed = "99999999-9999-4999-8999-999999999999";
        let closures = std::collections::BTreeSet::from([closed.to_owned()]);
        assert!(
            reject_closed_origin(&operation, &closures).is_ok(),
            "shared dock outlives unrelated Surface closure"
        );
        let presentation_id = PresentationId::try_new(&seat.presentation_id).unwrap();
        let surface_instance_id = SurfaceInstanceId::try_new(closed).unwrap();
        let mut mount = (*operation.move_freeze().unwrap().export.mounts()[0]).clone();
        mount.source_instance = RyeOsViewInstanceKey::surface_tile(
            &presentation_id,
            &surface_instance_id,
            TileId::new(1),
        );
        // Isolate the closure-order predicate: full mutated export/digest
        // admission is still checked separately by the existing freeze owner.
        let UiPreparedPresentationOperationBody::MoveFreeze { request } = &mut operation.body
        else {
            unreachable!()
        };
        request.export.content = UiTransferExportContent::SurfaceMount {
            source_view_set_id: ViewSetId::new(1),
            mount,
        };
        assert!(matches!(
            reject_closed_origin(&operation, &closures),
            Err(HandlerError::Conflict(_))
        ));
        assert!(reject_closed_origin(&operation, &Default::default()).is_ok());
        let binding = operation.move_freeze().unwrap().export.mounts()[0]
            .binding
            .clone();
        let UiPreparedPresentationOperationBody::MoveFreeze { request } = &mut operation.body
        else {
            unreachable!()
        };
        request.export.content = UiTransferExportContent::ViewSets {
            active_set: Some("empty".into()),
            sets: vec![UiTransferSetExport {
                handle: "empty".into(),
                source_view_set_id: ViewSetId::new(1),
                source_owner: UiViewMountOwner::Surface {
                    presentation_id,
                    surface_instance_id,
                },
                binding,
                seed: ryeos_client_base::surface::view_sets::ViewSetSeedSpec {
                    schema: 2,
                    id: "empty".into(),
                    title: "Empty".into(),
                    root: None,
                },
                focused_view: None,
                mounts: vec![],
            }],
        };
        assert!(matches!(
            reject_closed_origin(&operation, &closures),
            Err(HandlerError::Conflict(_))
        ));
    }

    #[test]
    fn surface_closure_requires_exact_live_membership_not_sibling_binding() {
        let presentation_id = "11111111-1111-4111-8111-111111111111";
        let mut closure = ryeos_client_base::ui::seat::UiSurfaceClosure {
            presentation_id: presentation_id.into(),
            surface_instance_id: "22222222-2222-4222-8222-222222222222".into(),
            binding: ryeos_client_base::ui::UiBindingAttachmentCoordinate {
                binding_attachment_id: "binding".into(),
                binding_generation: 1,
                binding_digest: "digest".into(),
            },
        };
        let mut surface = crate::browser_session::SurfaceMembership {
            surface_instance_id: closure.surface_instance_id.clone(),
            creation_request_id: "33333333-3333-4333-8333-333333333333".into(),
            ordinal: 1,
            title: "Surface".into(),
            coordinate: BindingAttachmentCoordinate {
                binding_attachment_id: "binding".into(),
                binding_generation: 1,
                binding_digest: "digest".into(),
            },
            initial_seed: None,
            closed: false,
            particular_resume: None,
            saved_resume: None,
            tab_openings: std::collections::BTreeMap::new(),
            mount_manifest:
                ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest::allocate(
                    ryeos_client_base::ids::PresentationId::try_new(presentation_id).unwrap(),
                    ryeos_client_base::ids::SurfaceInstanceId::try_new(
                        &closure.surface_instance_id,
                    )
                    .unwrap(),
                    1,
                    Vec::new(),
                    None,
                )
                .unwrap(),
        };
        assert!(closure_matches_live_surface(
            &closure,
            presentation_id,
            Some(&surface)
        ));
        closure.binding.binding_generation = 2;
        assert!(!closure_matches_live_surface(
            &closure,
            presentation_id,
            Some(&surface)
        ));
        closure.binding.binding_generation = 1;
        surface.surface_instance_id = "44444444-4444-4444-8444-444444444444".into();
        assert!(
            !closure_matches_live_surface(&closure, presentation_id, Some(&surface)),
            "same attachment does not close sibling mount"
        );
        surface.surface_instance_id = closure.surface_instance_id.clone();
        surface.closed = true;
        assert!(!closure_matches_live_surface(
            &closure,
            presentation_id,
            Some(&surface)
        ));
        surface.closed = false;
        assert!(!closure_matches_live_surface(
            &closure,
            "55555555-5555-4555-8555-555555555555",
            Some(&surface)
        ));
    }

    fn event_batch(payload: Value) -> Value {
        json!([{
            "engine_seq": 0_u64,
            "event_type": "seat.facet",
            "payload": payload,
        }])
    }

    #[test]
    fn typed_seat_digest_matches_browser_vectors() {
        let one_microsecond: Value = serde_json::from_str("0.000001").unwrap();
        let one_tenth_microsecond: Value = serde_json::from_str("1e-7").unwrap();
        let vectors = [
            (
                event_batch(json!({"key":"float", "value":one_microsecond})),
                "84550fb1cf3501cfdde9221974751fe5085b5bc1aed9ce7329e88467aaebc950",
            ),
            (
                event_batch(json!({
                    "key":"thresholds",
                    // The browser's wire encoder normalizes -0 to integer zero.
                    "value":[one_tenth_microsecond, 1.5, 0_u64],
                })),
                "6d0e2ab6e500346d7a21fcc6baef8401bfa2b1ce0a13d8816bc53883709bdd3f",
            ),
            (
                event_batch(json!({"key":"u64", "value":u64::MAX})),
                "b714cf712cbfc663f9eb93095596465167564a7d6bfa738c723376ff5609b209",
            ),
            (
                event_batch(json!({
                    "key":"unicode",
                    "value": {
                        "😀":"é",
                        "𐀀":[null, true],
                        "":{"__proto__":"inert"},
                    },
                })),
                "9f001b4f3bc1e8ccc924b29d7280435dbcefbc1777d5395a6ac080d4494d7835",
            ),
        ];

        for (value, expected) in vectors {
            assert_eq!(seat_payload_digest(&value).unwrap(), expected);
        }
    }
}
