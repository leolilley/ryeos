//! `ui.session.current` — return the authenticated session's context.
//!
//! The browser calls this once on load to discover the immutable authored
//! root attachment, exact presentation memberships, admitted bindings, and the
//! events URL. Descriptor order carries no authority.
//!
//! Requires `browser_session` auth (cookie). Transfer-bearing memberships
//! are repaired only from a completed bounded paired cut before projection;
//! this is retained-history work, not another grant or cap intersection.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

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
    let (paired, retired_initial, initial_memberships, initial_heads) = {
        let _transition = ui
            .lock_seat_transition()
            .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
        let session = ui
            .browser_sessions
            .get_session(&session_id)
            .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;
        let (paired, retired_initial) =
            super::ui_seat::reconcile_session_surface_closures(&state, &session)?;
        let paired = paired
            .into_iter()
            .map(|membership| -> Result<_> {
                let chain_root_id = membership.chain_root_id.as_deref().ok_or_else(|| {
                    HandlerError::Conflict("paired bootstrap has no chain root".into())
                })?;
                let thread_id = membership.thread_id.as_deref().ok_or_else(|| {
                    HandlerError::Conflict("paired bootstrap has no thread".into())
                })?;
                let anchor = state
                    .state_store
                    .capture_thread_event_read_anchor(chain_root_id, thread_id)?
                    .ok_or_else(|| {
                        HandlerError::Conflict("paired bootstrap lost signed head".into())
                    })?;
                Ok((membership, anchor))
            })
            .collect::<Result<Vec<_>>>()?;
        // Ordinary closure reconciliation may have changed the session above.
        // Freeze the actual post-repair inventory, not the stale pre-repair
        // clone used to discover transfer-bearing roots.
        let session = ui
            .browser_sessions
            .get_session(&session_id)
            .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;
        let initial = session.presentations.values().cloned().collect::<Vec<_>>();
        let heads = initial
            .iter()
            .filter_map(|membership| {
                membership
                    .thread_id
                    .as_deref()
                    .map(|thread_id| (membership.chain_root_id.as_deref(), thread_id))
            })
            .map(|(chain_root_id, thread_id)| -> Result<_> {
                let chain_root_id = chain_root_id.ok_or_else(|| {
                    HandlerError::Conflict("session seat has no chain root".into())
                })?;
                let anchor = state
                    .state_store
                    .capture_thread_event_read_anchor(chain_root_id, thread_id)?;
                Ok((chain_root_id.to_owned(), thread_id.to_owned(), anchor))
            })
            .collect::<Result<Vec<_>>>()?;
        (paired, retired_initial, initial, heads)
    };
    let mut completed_roots = Vec::new();
    for (membership, anchor) in paired {
        let chain_root_id = membership
            .chain_root_id
            .as_deref()
            .expect("checked paired root");
        let thread_id = membership
            .thread_id
            .as_deref()
            .expect("checked paired thread");
        let discovered = super::ui_seat::read_replay_transfer_cut_at_anchor(
            state.clone(),
            &session_id,
            &membership.presentation_id,
            chain_root_id,
            thread_id,
            anchor.clone(),
        )
        .await?;
        let super::ui_seat::DiscoveredRootTransferCut::Transfers {
            membership: discovered_membership,
            anchor: discovered_anchor,
            cut,
            ..
        } = discovered
        else {
            return Err(HandlerError::Conflict(
                "transfer-bearing bootstrap has no completed paired cut".into(),
            )
            .into());
        };
        if discovered_membership != membership || discovered_anchor != anchor {
            return Err(
                HandlerError::Conflict("paired bootstrap root membership changed".into()).into(),
            );
        }
        let cut = Arc::new(cut);
        let mut replay = super::ui_seat::FixedAnchorReplay::begin_at_anchor(
            &state,
            &membership.presentation_id,
            chain_root_id,
            thread_id,
            anchor.clone(),
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
                super::ui_seat::FixedAnchorReplayStep::Batch { .. } => {}
                super::ui_seat::FixedAnchorReplayStep::RequiresPairedTransfer {
                    anchor: required,
                } => {
                    if required != anchor {
                        return Err(HandlerError::Conflict(
                            "paired bootstrap changed original cut".into(),
                        )
                        .into());
                    }
                    replay.attach_paired_cut(cut.clone())?;
                }
                super::ui_seat::FixedAnchorReplayStep::Complete {
                    tip_hash,
                    thread_seq,
                } => {
                    replay.completed_paired_projection()?;
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
    for (membership, prefix) in &retired_initial {
        super::ui_seat::require_retired_prefix(state.clone(), membership, prefix).await?;
        let (discovered, requested, cut) =
            super::ui_seat::read_replay_transfer_cut_at_retired_prefix(
                state.clone(),
                &session_id,
                &membership.presentation_id,
                prefix.clone(),
            )
            .await?;
        if &discovered != membership || requested.is_empty() != cut.is_none() {
            return Err(HandlerError::Conflict(
                "retired bootstrap discovery changed its original scope".into(),
            )
            .into());
        }
        let cut = cut.map(Arc::new);
        let mut replay = super::ui_seat::FixedAnchorReplay::begin_at_retired_prefix(
            &state,
            &membership.presentation_id,
            prefix.clone(),
            &membership.initialization,
            cut.clone(),
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
                super::ui_seat::FixedAnchorReplayStep::Batch { .. } => {}
                super::ui_seat::FixedAnchorReplayStep::RequiresPairedTransfer { .. } => {
                    return Err(HandlerError::Conflict(
                        "retired replay lacks its complete historical cut".into(),
                    )
                    .into());
                }
                super::ui_seat::FixedAnchorReplayStep::Complete {
                    tip_hash,
                    thread_seq,
                } => {
                    if prefix.tip().map(|(hash, seq, _)| (hash, seq))
                        != Some((tip_hash.as_str(), thread_seq))
                    {
                        return Err(HandlerError::Conflict(
                            "retired replay changed marker-bound tip".into(),
                        )
                        .into());
                    }
                    replay.completed_composition_frontier(&membership.initialization)?;
                    if !replay.matches_completed_retired_prefix(prefix) {
                        return Err(HandlerError::Conflict(
                            "retired replay did not exhaust original prefix".into(),
                        )
                        .into());
                    }
                    completed_retired.push((membership.clone(), prefix.clone(), cut, replay));
                    break;
                }
            }
        }
    }
    let _transition = ui
        .lock_seat_transition()
        .map_err(|_| HandlerError::Internal("seat transition lock poisoned".into()))?;
    let session = ui
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;
    if session.presentations.len() != initial_memberships.len()
        || initial_memberships
            .iter()
            .any(|initial| session.presentations.get(&initial.presentation_id) != Some(initial))
    {
        return Err(HandlerError::Conflict(
            "session presentation inventory changed during paired bootstrap".into(),
        )
        .into());
    }
    for membership in session.presentations.values() {
        if let Some((_, _, _, replay)) = completed_retired
            .iter()
            .find(|(retired, _, _, _)| retired.presentation_id == membership.presentation_id)
        {
            replay.completed_composition_frontier(&membership.initialization)?;
            continue;
        }
        if let Some((_, _, _, replay)) = completed_roots
            .iter()
            .find(|(root, _, _, _)| root.presentation_id == membership.presentation_id)
        {
            replay.completed_composition_frontier(&membership.initialization)?;
        } else {
            super::ui_seat::require_presentation_initialization(
                &state,
                &session,
                &membership.presentation_id,
            )?;
        }
    }
    let mut distinct = std::collections::BTreeMap::new();
    let mut retired = std::collections::BTreeMap::new();
    for (_, root_anchor, cut, _) in &completed_roots {
        let (cut_anchors, cut_retired) = cut.publication_cut(&state, &session)?;
        if !cut_anchors.contains(root_anchor) {
            return Err(
                HandlerError::Conflict("paired bootstrap root cut disappeared".into()).into(),
            );
        }
        for anchor in cut_anchors {
            let key = (
                anchor.chain_root_id().to_owned(),
                anchor.thread_id().to_owned(),
            );
            if let Some(previous) = distinct.insert(key, anchor.clone())
                && previous != anchor
            {
                return Err(HandlerError::Conflict(
                    "paired bootstrap has conflicting signed cuts".into(),
                )
                .into());
            }
        }
        for prefix in cut_retired {
            let root = prefix.chain_root_id().to_owned();
            if retired.insert(root, prefix.clone()).is_some_and(
                |prior: ryeos_state::RetiredSeatPositivePrefix| !prior.same_cut(&prefix),
            ) {
                return Err(HandlerError::Conflict(
                    "paired bootstrap has conflicting retired cuts".into(),
                )
                .into());
            }
        }
    }
    for (_, root_prefix, _cut, replay) in &completed_retired {
        let (cut_anchors, cut_retired) = if let Some(cut) = replay.completed_paired_cut() {
            cut.publication_cut(&state, &session)?
        } else {
            (Vec::new(), vec![root_prefix.clone()])
        };
        if !cut_retired
            .iter()
            .any(|prefix| prefix.same_cut(root_prefix))
        {
            return Err(HandlerError::Conflict(
                "retired bootstrap cut omitted its original marker".into(),
            )
            .into());
        }
        for anchor in cut_anchors {
            let key = (
                anchor.chain_root_id().to_owned(),
                anchor.thread_id().to_owned(),
            );
            if let Some(previous) = distinct.insert(key, anchor.clone())
                && previous != anchor
            {
                return Err(HandlerError::Conflict(
                    "retired bootstrap has conflicting signed cuts".into(),
                )
                .into());
            }
        }
        for prefix in cut_retired {
            let root = prefix.chain_root_id().to_owned();
            if retired.insert(root, prefix.clone()).is_some_and(
                |prior: ryeos_state::RetiredSeatPositivePrefix| !prior.same_cut(&prefix),
            ) {
                return Err(HandlerError::Conflict(
                    "retired bootstrap has conflicting marker cuts".into(),
                )
                .into());
            }
        }
    }
    for (chain_root_id, thread_id, initial) in &initial_heads {
        if let Some(anchor) = initial {
            let key = (chain_root_id.clone(), thread_id.clone());
            if let Some(previous) = distinct.insert(key, anchor.clone())
                && previous != *anchor
            {
                return Err(HandlerError::Conflict(
                    "paired bootstrap differs from initial signed seat head".into(),
                )
                .into());
            }
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
    let anchors = distinct.into_values().collect::<Vec<_>>();
    for (membership, prefix) in &retired_initial {
        let matching = retired
            .get(prefix.chain_root_id())
            .is_some_and(|proven| proven.same_cut(prefix));
        if !matching || session.presentations.get(&membership.presentation_id) != Some(membership) {
            return Err(HandlerError::Conflict(
                "retired initialization lacks completed paired endpoint".into(),
            )
            .into());
        }
    }
    if retired.keys().any(|root| {
        anchors
            .iter()
            .any(|anchor| anchor.chain_root_id() == root.as_str())
    }) {
        return Err(
            HandlerError::Conflict("paired bootstrap mixes live and retired root".into()).into(),
        );
    }
    let retired = retired.into_values().collect::<Vec<_>>();
    let expected = &initial_memberships;
    if anchors.is_empty() && retired.is_empty() {
        let projected = ui
            .browser_sessions
            .with_exact_presentation_read_snapshot(
                &session_id,
                expected,
                state.node_policy.generation_digest(),
                |locked| project_current_session(&state, locked),
            )
            .map_err(|error| HandlerError::Forbidden(error.to_string()))??;
        return serde_json::to_value(projected).map_err(Into::into);
    }
    let projected = if completed_roots.is_empty() && completed_retired.is_empty() {
        ui.browser_sessions
            .with_exact_presentation_read_snapshot(
                &session_id,
                expected,
                state.node_policy.generation_digest(),
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
            .map(|(membership, _, cut, replay)| {
                Ok((
                    membership,
                    replay.completed_paired_projection()?,
                    cut.as_ref(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let retired_roots = completed_retired
            .iter()
            .map(|(membership, prefix, _cut, replay)| (membership, replay, prefix))
            .collect::<Vec<_>>();
        ui.browser_sessions
            .with_reconciled_completed_roots_at_cut(
                &state.state_store,
                &anchors,
                &retired,
                &session_id,
                expected,
                state.node_policy.generation_digest(),
                &roots,
                &retired_roots,
                |candidate| Ok(Some(project_current_session(&state, candidate)?)),
            )?
            .ok_or_else(|| HandlerError::Conflict("paired bootstrap signed cut advanced".into()))?
    };
    serde_json::to_value(projected).map_err(Into::into)
}

/// Pure projection: no store lookup, seat mutation or lock acquisition.
pub(crate) fn project_current_session(
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
) -> Result<ryeos_client_base::ui::BrowserSession> {
    let route_max = state
        .node_config
        .routes
        .iter()
        .find(|route| {
            route.response.source.as_deref()
                == Some(super::ui_invocations_dispatch::DESCRIPTOR.service_ref)
        })
        .map(|route| route.limits.body_bytes_max)
        .ok_or_else(|| HandlerError::Internal("UI binding dispatch route is absent".into()))?;

    // Private retained attachments outlive closing a Surface for history and
    // idempotent admission checks. Bootstrap exposes only live memberships and
    // immutable roots, not those historical descriptors as current choices.
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
    let append_max = super::ui_launch_mint::binding_request_bounds(&state)?.max_seat_append_bytes;
    let mut binding_attachments = session
        .attachments
        .values()
        .filter(|attachment| current_bindings.contains(attachment.binding_attachment_id.as_str()))
        .map(|attachment| {
            attachment.public_descriptor(ryeos_client_base::ui::UiBindingRequestBounds {
                max_request_bytes: route_max,
                max_input_bytes: route_max,
                max_seat_append_bytes: append_max,
            })
        })
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
        .require::<ryeos_app::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy>(
    )?;
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

/// Validate the complete prospective bootstrap before the session owner commits.
/// The caller holds the existing transition/store ownership; this helper never reenters it.
pub(crate) fn preflight_initial_publication(
    state: &AppState,
    session: &crate::browser_session::BrowserSession,
    candidate: &crate::browser_session::PresentationMembership,
    attachments: &[Arc<crate::browser_session::AdmittedBindingAttachment>],
) -> std::result::Result<(), crate::browser_session::AttachmentStoreError> {
    let mut projected = session.clone();
    let mut membership = candidate.clone();
    // Seat IDs are generated UUID coordinates. Reserve their full wire length
    // before durable seat creation, without granting an invented seat authority.
    const RESERVED_SEAT_ID: &str = "T-ffffffff-ffff-ffff-ffff-ffffffffffff";
    membership.thread_id = Some(RESERVED_SEAT_ID.into());
    membership.chain_root_id = Some(RESERVED_SEAT_ID.into());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_limit_charges_the_complete_result_envelope() {
        let response = ryeos_client_base::ui::BrowserSession {
            ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.into(),
            session_id: "session".into(),
            root_attachment_id: "root".into(),
            presentations: Vec::new(),
            binding_attachments: Vec::new(),
            user_principal_id: Some("principal".into()),
            events_url: None,
        };
        let exact = serde_json::to_vec(&serde_json::json!({"result": &response}))
            .unwrap()
            .len() as u64;
        assert!(validate_bootstrap_bytes(&response, exact).is_ok());
        assert!(validate_bootstrap_bytes(&response, exact - 1).is_err());
        let bare = serde_json::to_vec(&response).unwrap().len() as u64;
        assert!(validate_bootstrap_bytes(&response, bare).is_err());
    }
}
