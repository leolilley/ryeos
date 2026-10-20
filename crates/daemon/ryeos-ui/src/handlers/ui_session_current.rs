//! `ui.session.current` — return the authenticated session's context.
//!
//! The browser calls this once on load to discover the immutable authored
//! root attachment, exact presentation memberships, the currently admitted
//! binding descriptors, and the events URL. Descriptor order carries no
//! authority.
//!
//! Requires `browser_session` auth (cookie). Memberships whose seats carry
//! transfers, and memberships of retired seats, are repaired only from a
//! completed bounded paired cut before projection; this is retained-history
//! work, not another grant or cap intersection.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use crate::browser_session::transfer_reservation::{CompletedPairedCut, CompletedRetiredRoot};
use crate::state::get_ui_state;

/// Extract session_id from the handler context's fingerprint.
/// The browser_session invoker sets `id` to `session:<session_id>`.
fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    if ctx.fingerprint.starts_with("session:") {
        Some(
            ctx.fingerprint
                .strip_prefix("session:")
                .unwrap()
                .to_string(),
        )
    } else {
        None
    }
}

pub async fn handle(_params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let session_id = session_id_from_context(&ctx)
        .ok_or_else(|| HandlerError::Forbidden("no browser session".into()))?;

    let ui = get_ui_state(&state).expect("UiState not set");
    let (session, paired, retired_initial, initial_heads) = {
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        let session = ui
            .browser_sessions
            .get_session(&session_id)
            .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;
        let reconciliation = super::ui_seat::reconcile_session_surface_closures(&state, &session)?;
        // Ordinary closure reconciliation may have changed the session above.
        // Freeze the actual post-repair inventory, not the stale pre-repair
        // clone used to discover transfer-bearing roots.
        let session = ui
            .browser_sessions
            .get_session(&session_id)
            .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;
        let current = |presentation_id: &str| {
            session
                .presentations
                .get(presentation_id)
                .cloned()
                .ok_or_else(|| {
                    HandlerError::Conflict("session presentation inventory changed".into())
                })
        };
        let paired = reconciliation
            .paired
            .iter()
            .map(|membership| -> Result<_> {
                let membership = current(&membership.presentation_id)?;
                let (chain_root_id, thread_id) = seat_coordinates(&membership)?;
                let anchor = state
                    .state_store
                    .capture_thread_event_read_anchor(chain_root_id, thread_id)?
                    .ok_or_else(|| {
                        HandlerError::Conflict("paired bootstrap lost signed head".into())
                    })?;
                Ok((membership, anchor))
            })
            .collect::<Result<Vec<_>>>()?;
        let retired = reconciliation
            .retired
            .into_iter()
            .map(|(membership, evidence)| -> Result<_> {
                Ok((current(&membership.presentation_id)?, evidence))
            })
            .collect::<Result<Vec<_>>>()?;
        let heads = session
            .presentations
            .values()
            .filter(|membership| membership.thread_id.is_some())
            .map(|membership| -> Result<_> {
                let (chain_root_id, thread_id) = seat_coordinates(membership)?;
                let anchor = state
                    .state_store
                    .capture_thread_event_read_anchor(chain_root_id, thread_id)?;
                Ok((chain_root_id.to_owned(), thread_id.to_owned(), anchor))
            })
            .collect::<Result<Vec<_>>>()?;
        (session, paired, retired, heads)
    };

    let mut completed_roots = Vec::new();
    for (membership, anchor) in paired {
        let cut = super::ui_seat::paired_read::read_live_root_cut(
            state.clone(),
            &session,
            &membership,
            anchor.clone(),
        )
        .await?;
        let mut replay = super::ui_seat::FixedAnchorReplay::begin_at_anchor(
            &state,
            &membership.presentation_id,
            anchor.clone(),
            Some(cut.clone()),
        )?;
        loop {
            let read_state = state.clone();
            let (next, step) = tokio::task::spawn_blocking(move || {
                let step = replay.next_step(&read_state);
                (replay, step)
            })
            .await?;
            replay = next;
            match step? {
                super::ui_seat::FixedAnchorReplayStep::Pending => {}
                super::ui_seat::FixedAnchorReplayStep::Complete {
                    tip_hash,
                    thread_seq,
                } => {
                    replay.completed_paired_projection(&membership.initialization)?;
                    if anchor.tip() != Some((tip_hash.as_str(), thread_seq)) {
                        return Err(HandlerError::Conflict(
                            "paired bootstrap replay tip differs".into(),
                        )
                        .into());
                    }
                    completed_roots.push((membership, anchor, cut, replay));
                    break;
                }
            }
        }
    }
    let mut completed_retired = Vec::new();
    for (membership, evidence) in retired_initial {
        let root = super::ui_seat::paired_read::read_retired_root(
            state.clone(),
            &session,
            &membership,
            evidence,
        )
        .await?;
        completed_retired.push((membership, root));
    }

    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let current = ui
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;
    if current.presentations != session.presentations {
        return Err(HandlerError::Conflict(
            "session presentation inventory changed during paired bootstrap".into(),
        )
        .into());
    }
    for membership in session.presentations.values() {
        let settled = completed_roots
            .iter()
            .any(|(root, _, _, _)| root.presentation_id == membership.presentation_id)
            || completed_retired
                .iter()
                .any(|(retired, _)| retired.presentation_id == membership.presentation_id);
        if !settled && membership.thread_id.is_some() {
            super::ui_seat::initialization::require(&state, &session, &membership.presentation_id)?;
        }
    }
    let mut anchors = std::collections::BTreeMap::new();
    let mut fences =
        std::collections::BTreeMap::<String, ryeos_state::RetiredChainDependency>::new();
    for (_, root_anchor, cut, _) in &completed_roots {
        if !cut.current_anchors().contains(root_anchor) {
            return Err(
                HandlerError::Conflict("paired bootstrap root cut disappeared".into()).into(),
            );
        }
        include_cut(&mut anchors, &mut fences, cut)?;
    }
    for (_, root) in &completed_retired {
        match root.cut() {
            Some(cut) => {
                if !cut.retired_dependencies().contains(root.dependency()) {
                    return Err(HandlerError::Conflict(
                        "retired bootstrap cut omitted its original fence".into(),
                    )
                    .into());
                }
                include_cut(&mut anchors, &mut fences, cut)?;
            }
            None => include_fence(&mut fences, root.dependency())?,
        }
    }
    for (chain_root_id, thread_id, initial) in &initial_heads {
        if let Some(anchor) = initial {
            include_anchor(&mut anchors, anchor)?;
        } else if state
            .state_store
            .capture_thread_event_read_anchor(chain_root_id, thread_id)?
            .is_some()
        {
            // There is no signed chain lock to hold for an absent initial
            // head. Under the seat transition, a newly initialized seat must
            // restart rather than slip into the projected session.
            return Err(HandlerError::Conflict(
                "session seat acquired its first signed head during bootstrap".into(),
            )
            .into());
        }
    }
    if fences
        .keys()
        .any(|root| anchors.keys().any(|(anchor_root, _)| anchor_root == root))
    {
        return Err(
            HandlerError::Conflict("paired bootstrap mixes live and retired root".into()).into(),
        );
    }
    let anchors = anchors.into_values().collect::<Vec<_>>();
    let fences = fences.into_values().collect::<Vec<_>>();
    let expected = session.presentations.values().cloned().collect::<Vec<_>>();
    let policy_digest = state.node_policy.generation_digest();
    let projected = if anchors.is_empty() && fences.is_empty() {
        ui.browser_sessions
            .with_exact_presentation_read_snapshot(
                &session_id,
                &expected,
                policy_digest,
                |locked| project_current_session(&state, locked),
            )
            .map_err(|error| HandlerError::Forbidden(error.to_string()))??
    } else if completed_roots.is_empty() && completed_retired.is_empty() {
        ui.browser_sessions
            .with_exact_presentation_read_snapshot(
                &session_id,
                &expected,
                policy_digest,
                |locked| {
                    state
                        .state_store
                        .with_thread_event_read_anchors(&anchors, || {
                            project_current_session(&state, locked)
                        })
                },
            )
            .map_err(|error| HandlerError::Forbidden(error.to_string()))??
            .ok_or_else(|| HandlerError::Conflict("session signed cut advanced".into()))?
    } else {
        let roots = completed_roots
            .iter()
            .map(|(membership, _, cut, replay)| -> Result<_> {
                Ok((
                    membership,
                    replay.completed_paired_projection(&membership.initialization)?,
                    cut.as_ref() as &dyn CompletedPairedCut,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let retired_roots = completed_retired
            .iter()
            .map(|(membership, root)| {
                (
                    membership,
                    root as &dyn CompletedRetiredRoot,
                    root.dependency(),
                )
            })
            .collect::<Vec<_>>();
        ui.browser_sessions
            .with_reconciled_completed_roots_at_cut(
                &state.state_store,
                &anchors,
                &fences,
                &session_id,
                &expected,
                policy_digest,
                &roots,
                &retired_roots,
                |candidate| Ok(Some(project_current_session(&state, candidate)?)),
            )?
            .ok_or_else(|| HandlerError::Conflict("paired bootstrap signed cut advanced".into()))?
    };
    serde_json::to_value(projected).map_err(Into::into)
}

type AnchorKey = (String, String);

fn seat_coordinates(
    membership: &crate::browser_session::PresentationMembership,
) -> Result<(&str, &str), HandlerError> {
    let thread_id = membership
        .thread_id
        .as_deref()
        .ok_or_else(|| HandlerError::Conflict("session seat has no thread".into()))?;
    let chain_root_id = membership
        .chain_root_id
        .as_deref()
        .ok_or_else(|| HandlerError::Conflict("session seat has no chain root".into()))?;
    Ok((chain_root_id, thread_id))
}

fn include_anchor(
    anchors: &mut std::collections::BTreeMap<
        AnchorKey,
        ryeos_state::AuthoritativeThreadEventReadAnchor,
    >,
    anchor: &ryeos_state::AuthoritativeThreadEventReadAnchor,
) -> Result<()> {
    let key = (
        anchor.chain_root_id().to_owned(),
        anchor.thread_id().to_owned(),
    );
    if anchors
        .insert(key, anchor.clone())
        .is_some_and(|previous| previous != *anchor)
    {
        return Err(HandlerError::Conflict("bootstrap has conflicting signed cuts".into()).into());
    }
    Ok(())
}

fn include_fence(
    fences: &mut std::collections::BTreeMap<String, ryeos_state::RetiredChainDependency>,
    fence: &ryeos_state::RetiredChainDependency,
) -> Result<()> {
    if fences
        .insert(fence.chain_root_id.clone(), fence.clone())
        .is_some_and(|previous| previous != *fence)
    {
        return Err(HandlerError::Conflict("bootstrap has conflicting retired cuts".into()).into());
    }
    Ok(())
}

fn include_cut(
    anchors: &mut std::collections::BTreeMap<
        AnchorKey,
        ryeos_state::AuthoritativeThreadEventReadAnchor,
    >,
    fences: &mut std::collections::BTreeMap<String, ryeos_state::RetiredChainDependency>,
    cut: &super::ui_seat::paired_read::FixedTransferCut,
) -> Result<()> {
    for anchor in cut.current_anchors() {
        include_anchor(anchors, &anchor)?;
    }
    for fence in cut.retired_dependencies() {
        include_fence(fences, &fence)?;
    }
    Ok(())
}

/// Pure projection: no store lookup, seat mutation or lock acquisition.
pub(crate) fn project_current_session(
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
) -> Result<ryeos_client_base::ui::BrowserSession> {
    let bounds = super::ui_launch_mint::binding_request_bounds(state)?;

    // Private retained attachments outlive closing a Surface for history and
    // idempotent admission checks. Bootstrap exposes only live memberships and
    // the immutable launch root, not those historical descriptors as current
    // choices. Hidden transfer batches are not session attachments yet.
    let current_bindings = std::iter::once(session.root_attachment_id.as_str())
        .chain(session.presentations.values().flat_map(|membership| {
            std::iter::once(membership.root_coordinate.binding_attachment_id.as_str())
                .chain(
                    membership
                        .activated_transfer_reservations
                        .values()
                        .flat_map(|reservation| {
                            reservation
                                .current_coordinates()
                                .map(|coordinate| coordinate.binding_attachment_id.as_str())
                        }),
                )
                .chain(
                    membership
                        .surfaces
                        .values()
                        .filter(|surface| !surface.closed)
                        .flat_map(|surface| {
                            std::iter::once(surface.coordinate.binding_attachment_id.as_str())
                                .chain(
                                    surface
                                        .saved_resume
                                        .iter()
                                        .flat_map(|receipt| receipt.coordinates())
                                        .map(|coordinate| {
                                            coordinate.binding_attachment_id.as_str()
                                        }),
                                )
                        }),
                )
                .chain(
                    membership
                        .surfaces
                        .values()
                        .flat_map(|surface| surface.tab_openings.values())
                        .flat_map(|opening| opening.current_coordinates.iter())
                        .map(|coordinate| coordinate.binding_attachment_id.as_str()),
                )
        }))
        .collect::<std::collections::BTreeSet<_>>();
    let mut binding_attachments = session
        .attachments
        .values()
        .filter(|attachment| current_bindings.contains(attachment.binding_attachment_id.as_str()))
        .map(|attachment| attachment.public_descriptor(bounds))
        .collect::<Vec<_>>();
    binding_attachments
        .sort_by(|left, right| left.binding_attachment_id.cmp(&right.binding_attachment_id));
    let mut presentations = session
        .presentations
        .values()
        .map(|membership| membership.public_descriptor())
        .collect::<Vec<_>>();
    presentations.sort_by(|left, right| left.presentation_id.cmp(&right.presentation_id));
    let response = ryeos_client_base::ui::BrowserSession {
        ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_string(),
        session_id: session.session_id.clone(),
        root_attachment_id: session.root_attachment_id.clone(),
        presentations,
        binding_attachments,
        user_principal_id: session.user_principal_id.clone(),
        events_url: Some(format!("/ui/events/session/{}", session.session_id)),
    };

    enforce_bootstrap_bound(state, &response)?;
    Ok(response)
}

fn enforce_bootstrap_bound(
    state: &AppState,
    response: &ryeos_client_base::ui::BrowserSession,
) -> Result<()> {
    let policy = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>()?;
    validate_bootstrap_bytes(response, policy.max_session_bootstrap_bytes)
}

fn validate_bootstrap_bytes(
    response: &ryeos_client_base::ui::BrowserSession,
    maximum: u64,
) -> Result<()> {
    let envelope = serde_json::json!({"result": response});
    let bytes = serde_json::to_vec(&envelope)?;
    if u64::try_from(bytes.len())? > maximum {
        return Err(
            HandlerError::Forbidden("session bootstrap exceeds signed byte limit".into()).into(),
        );
    }
    Ok(())
}

/// Validate the complete prospective bootstrap before the session owner
/// commits. The caller holds the existing transition/store ownership; this
/// helper never reenters it.
pub fn preflight_initial_publication(
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
    candidate: &crate::browser_session::PresentationMembership,
    attachments: &[Arc<crate::browser_session::AdmittedBindingAttachment>],
) -> std::result::Result<(), crate::browser_session::AttachmentStoreError> {
    let mut projected = session.clone();
    let mut membership = candidate.clone();
    // Seat ids are generated thread-id coordinates. Reserve their full wire
    // length before durable seat creation, without granting an invented seat
    // authority.
    let reserved_seat_id = ryeos_app::thread_lifecycle::new_thread_id();
    membership.thread_id = Some(reserved_seat_id.clone());
    membership.chain_root_id = Some(reserved_seat_id);
    projected
        .presentations
        .insert(membership.presentation_id.clone(), membership);
    for attachment in attachments {
        projected
            .attachments
            .insert(attachment.binding_attachment_id.clone(), attachment.clone());
    }
    project_current_session(state, &projected)
        .map(|_| ())
        .map_err(|_| crate::browser_session::AttachmentStoreError::Capacity)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/session/current",
    endpoint: "ui.session.current",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};
