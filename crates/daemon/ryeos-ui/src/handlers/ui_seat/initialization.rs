//! The existing seat journal owns the immutable initial admitted composition.
//! Never repair this record from the current mutable Surface membership.

use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::{CutAppendResult, ExactSeat, NewEventRecord, ThreadDetail};
use ryeos_client_base::ui::presentation_initialization::{
    PRESENTATION_INITIALIZED_EVENT, UiPresentationInitializationRecord,
};

use crate::browser_session::{AttachmentStoreError, BrowserSession, PresentationMembership};

pub(super) fn record(membership: &PresentationMembership) -> UiPresentationInitializationRecord {
    UiPresentationInitializationRecord {
        operation_id: membership.creation_request_id.clone(),
        initialization: membership.initialization.clone(),
    }
}

/// Complete the existing prepared seat without claiming a browser producer.
/// A retained admission can retry this after any interrupted journal write.
pub(crate) fn materialize_unclaimed(
    state: &AppState,
    session: &BrowserSession,
    mut membership: PresentationMembership,
) -> anyhow::Result<PresentationMembership> {
    use ryeos_app::thread_lifecycle::ThreadCreateParams;
    let ui = crate::state::get_ui_state(state).expect("UiState not set");
    let root = membership.root_coordinate.clone();
    let attachment = ui
        .browser_sessions
        .resolve_attachment(&session.session_id, &root)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let launcher = super::seat_launcher(session).to_owned();
    let project_access = crate::seat_auth::attachment_project_access(&attachment)?;
    let admission = {
        let _timing = crate::phase_timing::PhaseTimer::start(
            "seat_open_prepare",
            "materialize_unclaimed.admit_seat_root",
        );
        super::admit_seat_root(
            state,
            &attachment.root_ref,
            project_access.as_ref(),
            &launcher,
            session.granted_caps.clone(),
        )?
    };
    let seat_kind = super::seat_kind(state)?;
    let _dispatch = ui
        .browser_sessions
        .admit_attachment_dispatch(&session.session_id, &root)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    if membership.thread_id.is_none() {
        let thread = ryeos_app::thread_lifecycle::new_thread_id();
        membership = ui
            .browser_sessions
            .bind_presentation_seat(
                &session.session_id,
                &membership.presentation_id,
                &root,
                &thread,
                &thread,
                state.node_policy.generation_digest(),
            )
            .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    }
    let thread = membership
        .thread_id
        .as_deref()
        .ok_or_else(|| HandlerError::Internal("seat reservation absent".into()))?;
    let chain = membership
        .chain_root_id
        .as_deref()
        .ok_or_else(|| HandlerError::Internal("chain reservation absent".into()))?;
    if state.state_store.get_thread(thread)?.is_none() {
        let _timing = crate::phase_timing::PhaseTimer::start(
            "seat_open_prepare",
            "materialize_unclaimed.create_thread",
        );
        let site = state.threads.site_id().to_string();
        state.threads.create_non_execution_root_thread(
            &ThreadCreateParams {
                thread_id: thread.into(),
                chain_root_id: chain.into(),
                kind: seat_kind,
                item_ref: attachment.root_ref.clone(),
                executor_ref: super::presentation_executor_ref(&membership.presentation_id, &root),
                launch_mode: "wait".into(),
                current_site_id: site.clone(),
                origin_site_id: site,
                upstream_thread_id: None,
                requested_by: Some(launcher),
                project_root: admission.project_root().map(std::path::Path::to_path_buf),
                project_authority: admission.project_authority().clone(),
                base_project_snapshot_hash: None,
                usage_subject: None,
                usage_subject_asserted_by: None,
                captured_history_policy: None,
            },
            &admission,
        )?;
    }
    let (detail, _) = super::require_presentation_seat_for_read(
        state,
        session,
        &membership.presentation_id,
        Some(thread),
        Some(chain),
    )?;
    if detail.status != "running" {
        if detail.status != "created" {
            return Err(HandlerError::Conflict(
                "reserved presentation seat is terminal or unavailable".into(),
            )
            .into());
        }
        state.threads.mark_running(thread)?;
    }
    let (initialized, initial_anchor) =
        super::authenticated_history::initialization_record_with_anchor(state, chain, thread)?;
    let unclaimed_anchor = if initialized.is_some() {
        // Prepare retry is read-only for lifecycle after initialization. A
        // missing lease never licenses renewal of another document's owner.
        None
    } else {
        Some(initial_anchor.ok_or_else(|| {
            HandlerError::Conflict("prepared seat has no signed initialization anchor".into())
        })?)
    };
    if let Some(anchor) = unclaimed_anchor {
        // Even a non-UI lifecycle writer cannot advance this signed cut
        // between the absence proof and lease renewal.
        let holder = super::seat_holder(session);
        if !state.state_store.touch_seat_lease_if_anchors(
            &ExactSeat {
                root_id: chain,
                launcher: super::seat_launcher(session),
                holder: &holder,
                item_ref: &detail.item_ref,
                executor_ref: &detail.executor_ref,
            },
            &anchor,
            std::slice::from_ref(&anchor),
        )? {
            return Err(HandlerError::Conflict(
                "prepared seat changed before unclaimed lease renewal".into(),
            )
            .into());
        }
    }
    {
        let _timing = crate::phase_timing::PhaseTimer::start(
            "seat_open_prepare",
            "materialize_unclaimed.ensure_initialization",
        );
        ensure(state, session, &membership, &detail)?;
    }
    Ok(membership)
}

/// All fallible content/size checks run before initial membership publication.
/// The fixed-width reserved thread spelling covers the actual UUID seat ID;
/// initialization itself consumes no producer event sequence.
pub(crate) fn preflight(
    state: &AppState,
    membership: &PresentationMembership,
) -> Result<(), AttachmentStoreError> {
    let record = record(membership);
    record
        .validate()
        .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
    if record
        .initialization
        .digest()
        .map_err(|_| AttachmentStoreError::PresentationInvalid)?
        != membership.initialization_digest
    {
        return Err(AttachmentStoreError::PresentationInvalid);
    }
    let root = record
        .initialization
        .binding_attachments
        .iter()
        .find(|descriptor| {
            descriptor.binding_attachment_id == membership.root_coordinate.binding_attachment_id
        })
        .ok_or(AttachmentStoreError::PresentationInvalid)?;
    let current = super::super::ui_launch_mint::binding_request_bounds(state)
        .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
    let producer = ryeos_client_base::ui::UiPresentationProducer {
        presentation_id: membership.presentation_id.clone(),
        thread_id: "T-00000000-0000-4000-8000-000000000000".into(),
        producer_incarnation: "f".repeat(64),
    };
    if !ryeos_client_base::ui::admission_operation::single_seat_append_fits(
        &producer,
        PRESENTATION_INITIALIZED_EVENT,
        &record,
        root.binding_request_bounds
            .max_seat_append_bytes
            .min(current.max_seat_append_bytes),
    ) {
        return Err(AttachmentStoreError::PresentationInvalid);
    }
    Ok(())
}

/// A missing row is repairable only by Prepare from its still-current retained
/// admission. An existing row must be identical; duplicates never pick a winner.
pub(super) fn ensure(
    state: &AppState,
    session: &BrowserSession,
    membership: &PresentationMembership,
    detail: &ThreadDetail,
) -> Result<(), HandlerError> {
    state
        .engine
        .with_checked_bundle_generation(|generation| -> anyhow::Result<()> {
            revalidate_current_admission(generation, state, &session.session_id, membership)?;
            ensure_current(generation, state, session, membership, detail).map_err(Into::into)
        })
        .map_err(|error| HandlerError::Conflict(format!("{error:#}")))
}

/// Re-resolve every immutable initial closure under one checked generation.
/// Live mutable Surface membership is never used to repair initial testimony.
pub(super) fn revalidate_current_admission(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session_id: &str,
    membership: &PresentationMembership,
) -> anyhow::Result<()> {
    let ui = crate::state::get_ui_state(state)
        .ok_or_else(|| HandlerError::Internal("UiState not set".into()))?;
    for descriptor in &membership.initialization.binding_attachments {
        let coordinate = crate::browser_session::BindingAttachmentCoordinate {
            binding_attachment_id: descriptor.binding_attachment_id.clone(),
            binding_generation: descriptor.binding_generation,
            binding_digest: descriptor.binding_digest.clone(),
        };
        let attachment = ui
            .browser_sessions
            .resolve_attachment(session_id, &coordinate)
            .map_err(|error| HandlerError::Conflict(error.to_string()))?;
        if attachment
            .compiled_binding
            .binding
            .node_policy_generation_digest
            != state.node_policy.generation_digest()
        {
            return Err(HandlerError::Conflict(
                "retained initialization policy generation is stale".into(),
            )
            .into());
        }
        match (
            attachment.project_authority.as_ref(),
            attachment.compiled_binding.binding.project_root.as_deref(),
            attachment.project_query_identity.as_deref(),
        ) {
            (None, None, None) => {}
            (Some(authority), Some(root), Some(query))
                if authority.path() == std::path::Path::new(root) && root == query =>
            {
                authority.ensure_path_binding()?;
            }
            _ => {
                return Err(HandlerError::Conflict(
                    "retained initialization project authority differs".into(),
                )
                .into());
            }
        }
        if generation.request_engine_generation_identity()
            != attachment
                .compiled_binding
                .binding
                .request_engine_generation_identity
        {
            return Err(HandlerError::Conflict(
                "retained initialization engine generation is stale".into(),
            )
            .into());
        }
        let path = attachment
            .project_authority
            .as_ref()
            .map(|authority| authority.descriptor_path())
            .transpose()?;
        super::super::ui_launch_mint::revalidate_binding_authority(
            generation,
            attachment.compiled_binding.as_ref(),
            path.as_deref(),
            attachment.project_authority.as_deref(),
        )?;
    }
    Ok(())
}

fn ensure_current(
    generation: &ryeos_engine::engine::CheckedEngineGeneration<'_>,
    state: &AppState,
    session: &BrowserSession,
    membership: &PresentationMembership,
    detail: &ThreadDetail,
) -> Result<(), HandlerError> {
    preflight(state, membership).map_err(|error| HandlerError::Conflict(error.to_string()))?;
    let (existing, anchor) = retained(state, membership, detail)?;
    if existing.is_some() {
        return Ok(());
    }
    // The complete signed absence read also refuses pre-existing producer or
    // seat activity. Its exact head must still be current at append time:
    // two processes may otherwise publish duplicate initialization records.
    let anchor = anchor.ok_or_else(|| {
        HandlerError::Conflict("initialization seat has no signed thread head".into())
    })?;
    let payload = serde_json::to_value(record(membership))
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let holder = super::seat_holder(session);
    // The initialization record is durable and is never withdrawn. Verify the
    // installed bundles once more here, after the admission was re-resolved
    // and immediately before the append, so the record is never written from
    // an admission resolved on bundles that have since changed.
    generation
        .verify_before_commit()
        .map_err(|error| HandlerError::Conflict(error.to_string()))?;
    let written = state
        .state_store
        .append_seat_events_and_renew_if_anchor(
            &ExactSeat {
                root_id: &detail.chain_root_id,
                launcher: super::seat_launcher(session),
                holder: &holder,
                item_ref: &detail.item_ref,
                executor_ref: &detail.executor_ref,
            },
            &[NewEventRecord {
                event_type: PRESENTATION_INITIALIZED_EVENT.into(),
                storage_class: "indexed".into(),
                payload,
            }],
            &anchor,
        )
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    let written = match written {
        CutAppendResult::Committed(records) => records,
        CutAppendResult::Conflict => {
            return Err(HandlerError::Conflict(
                "initialization seat head changed; retry from its current signed history".into(),
            ));
        }
        CutAppendResult::NotRunning => {
            return Err(HandlerError::Conflict(
                "initialization seat is no longer running".into(),
            ));
        }
    };
    if written.len() != 1 || retained(state, membership, detail)?.0.is_none() {
        return Err(HandlerError::Conflict(
            "seat initialization acknowledgement is missing or contradictory".into(),
        ));
    }
    Ok(())
}

fn retained(
    state: &AppState,
    membership: &PresentationMembership,
    detail: &ThreadDetail,
) -> Result<
    (
        Option<UiPresentationInitializationRecord>,
        Option<ryeos_state::AuthoritativeThreadEventReadAnchor>,
    ),
    HandlerError,
> {
    let (retained, anchor) = super::authenticated_history::initialization_record_with_anchor(
        state,
        &detail.chain_root_id,
        &detail.thread_id,
    )?;
    let Some(retained) = retained else {
        return Ok((None, anchor));
    };
    if retained != record(membership)
        || retained
            .initialization
            .digest()
            .map_err(HandlerError::Internal)?
            != membership.initialization_digest
    {
        return Err(HandlerError::Conflict(
            "seat initialization differs from original retained admission".into(),
        ));
    }
    Ok((Some(retained), anchor))
}

/// Refuse a claim whose retained admission no longer holds.
///
/// Prepare publishes the membership and writes the initialization record
/// before its closing check, so a prepare that answered an error may have left
/// both behind. A claim therefore re-resolves every initial closure under one
/// checked generation, exactly as prepare does, and is refused as a typed
/// conflict when the admission is stale or the installed bundles changed.
pub(crate) fn require_current_admission(
    state: &AppState,
    session: &BrowserSession,
    presentation_id: &str,
) -> Result<(), HandlerError> {
    let membership = session
        .presentations
        .get(presentation_id)
        .ok_or_else(|| HandlerError::Forbidden("presentation is absent".into()))?;
    state
        .engine
        .with_checked_bundle_generation(|generation| {
            revalidate_current_admission(generation, state, &session.session_id, membership)
        })
        .map_err(|error| match error.downcast::<HandlerError>() {
            Ok(typed) => typed,
            Err(other) => HandlerError::Conflict(format!(
                "retained presentation admission no longer holds: {other:#}"
            )),
        })
}

pub(crate) fn require(
    state: &AppState,
    session: &BrowserSession,
    presentation_id: &str,
) -> Result<(), HandlerError> {
    let membership = session
        .presentations
        .get(presentation_id)
        .ok_or_else(|| HandlerError::Forbidden("presentation is absent".into()))?;
    let detail = super::require_presentation_seat(state, session, presentation_id, None, None)?;
    super::composition::read_history(state, &detail, &membership.initialization)
        .map_err(|error| HandlerError::Conflict(format!(
            "presentation initialization is not authenticated; retry its original Prepare: {error:#}"
        )))?;
    Ok(())
}
