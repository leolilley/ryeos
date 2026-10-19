//! The terminal effect executor: each `RyeOsEffect` from the core maps
//! to one daemon call; results come home over the loop's effect channel
//! and fold back into the core as `EffectResult` events. No ryeos state
//! lives here — this is the boundary where engine intent becomes
//! transport calls, and the boundary where the render loop stops
//! waiting: a round trip in flight never blocks a frame.

use std::sync::Arc;

use ryeos_client_base::ui::{
    RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind,
};

use crate::transport::daemon::{ClientError, DaemonClient};

/// Launch one generation of effects as a concurrent batch off the loop —
/// a startup burst of independent fetches costs one round trip, not one
/// per view. The joined results arrive as a single message in emission
/// order; the loop folds them and spawns any follow-up generation the
/// folds emit. Freshness is the core's job (per-key epochs), so batches
/// from different generations may resolve in any order.
pub fn spawn_effects(
    client: &Arc<DaemonClient>,
    effects: Vec<RyeOsEffect>,
    tx: &tokio::sync::mpsc::UnboundedSender<Vec<RyeOsEffectResult>>,
) {
    if effects.is_empty() {
        return;
    }
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let results = futures_util::future::join_all(
            effects.iter().map(|effect| run_effect(&client, effect)),
        )
        .await;
        let _ = tx.send(results);
    });
}

async fn run_effect(client: &DaemonClient, effect: &RyeOsEffect) -> RyeOsEffectResult {
    effect_result(effect, effect_data(client, &effect.kind).await)
}

fn effect_result(
    effect: &RyeOsEffect,
    outcome: Result<serde_json::Value, ClientError>,
) -> RyeOsEffectResult {
    let kind = result_kind_for(&effect.kind);
    match outcome {
        Ok(data) => RyeOsEffectResult {
            id: effect.id,
            ok: true,
            kind,
            data: Some(data),
            error: None,
        },
        Err(error) => RyeOsEffectResult {
            id: effect.id,
            ok: false,
            kind,
            data: None,
            error: Some(effect_error(&effect.kind, error)),
        },
    }
}

fn unsupported_native_effect(kind: &RyeOsEffectKind) -> ClientError {
    let capability = match kind {
        RyeOsEffectKind::OpenNewPresentationDocument { .. }
        | RyeOsEffectKind::ShowPreparedPresentationDocument { .. } => {
            "independent document hosting"
        }
        RyeOsEffectKind::SetLocationHash { .. } => "location hash navigation",
        RyeOsEffectKind::CopyToClipboard { .. } => "clipboard copying",
        RyeOsEffectKind::OpenUrl { .. } => "external URL opening",
        _ => unreachable!("only unimplemented native platform effects are classified here"),
    };
    ClientError::UiBindingRequest(format!("Native terminal does not implement {capability}."))
}

fn effect_error(kind: &RyeOsEffectKind, error: ClientError) -> ryeos_client_base::ui::RyeOsUiError {
    let mutation = matches!(
        kind,
        RyeOsEffectKind::InvokeBinding { .. }
            | RyeOsEffectKind::PrepareTransfer { .. }
            | RyeOsEffectKind::ContactTransfer { .. }
            | RyeOsEffectKind::InvokePresentationOperation { .. }
            | RyeOsEffectKind::ReplaceSession { .. }
            | RyeOsEffectKind::ReleaseBindingAttachment { .. }
    );
    let contact_unknown = matches!(
        &error,
        ClientError::Transport(_)
            | ClientError::DaemonDown { .. }
            | ClientError::Io(_)
            | ClientError::Json(_)
    );
    if mutation && contact_unknown {
        let code = match kind {
            RyeOsEffectKind::ReplaceSession { .. } => "session_replacement_outcome_unknown",
            RyeOsEffectKind::ReleaseBindingAttachment { .. } => {
                "attachment_release_outcome_unknown"
            }
            _ => "invocation_outcome_unknown",
        };
        ryeos_client_base::ui::RyeOsUiError::outcome_unknown(code, error.to_string())
    } else {
        ryeos_client_base::ui::RyeOsUiError::definite("platform_effect_failed", error.to_string())
    }
}

async fn effect_data(
    client: &DaemonClient,
    kind: &RyeOsEffectKind,
) -> Result<serde_json::Value, ClientError> {
    match kind {
        RyeOsEffectKind::FetchSource {
            request,
            request_bounds,
            ..
        } => {
            // Polling sources must not enter ordinary execution admission.
            // That path pins a project snapshot before it can inspect a
            // service's `record_thread: false` contract, so a live threads
            // view can otherwise starve real work with snapshot/store churn.
            // The UI invocation lane enforces the resolved service's
            // read-only policy without admitting a service thread.
            request
                .validate_bounds(*request_bounds)
                .map_err(|error| ClientError::UiBindingRequest(error.to_string()))?;
            let body = serde_json::to_value(request)?;
            let envelope = client
                .signed_post("/ui/api/invocations/dispatch", &body)
                .await?;
            Ok(envelope
                .pointer("/result/result")
                .or_else(|| envelope.get("result"))
                .cloned()
                .unwrap_or(envelope))
        }
        RyeOsEffectKind::InvokeBinding {
            request,
            request_bounds,
            ..
        } => {
            request
                .validate_bounds(*request_bounds)
                .map_err(|error| ClientError::UiBindingRequest(error.to_string()))?;
            let body = serde_json::to_value(request)?;
            let envelope = client
                .signed_post("/ui/api/invocations/dispatch", &body)
                .await?;
            Ok(envelope
                .pointer("/result/result")
                .or_else(|| envelope.get("result"))
                .cloned()
                .unwrap_or(envelope))
        }
        RyeOsEffectKind::ObserveTransferDestination {
            destination_presentation_id,
            destination_thread_id,
            destination_chain_root_id,
            source_producer,
            request_bounds,
            ..
        } => {
            let producer = client
                .ui_producer
                .read()
                .expect("producer lock poisoned")
                .clone();
            if producer.as_ref() != Some(source_producer) {
                return Err(ClientError::UiBindingRequest(
                    "transfer observation has another current source producer".into(),
                ));
            }
            let body = serde_json::json!({"action":"observe",
                "presentation_id":destination_presentation_id,
                "thread_id":destination_thread_id,
                "chain_root_id":destination_chain_root_id});
            if request_bounds.max_request_bytes == 0
                || serde_json::to_vec(&body)?.len() as u64 > request_bounds.max_request_bytes
            {
                return Err(ClientError::UiBindingRequest(
                    "transfer observation exceeds admitted request bound".into(),
                ));
            }
            let envelope = client
                .signed_post("/ui/api/session/seat/open", &body)
                .await?;
            let observation = envelope.get("result").cloned().unwrap_or(envelope);
            Ok(
                serde_json::json!({"operation_id":uuid::Uuid::new_v4().to_string(), "observation":observation}),
            )
        }
        RyeOsEffectKind::ContactTransfer {
            request,
            request_bounds,
        } => {
            let producer = client
                .ui_producer
                .read()
                .expect("producer lock poisoned")
                .clone()
                .ok_or_else(|| {
                    ClientError::UiBindingRequest("transfer contact producer is not ready".into())
                })?;
            let actor = request.actor_seat();
            if producer.presentation_id != actor.presentation_id
                || producer.thread_id != actor.thread_id
            {
                return Err(ClientError::UiBindingRequest(
                    "transfer contact has another active producer".into(),
                ));
            }
            // The immutable transfer incarnation records original provenance.
            // Current producer/control authentication belongs to the seat owner.
            let body = transfer_contact_body(request, *request_bounds)?;
            let envelope = client
                .signed_post("/ui/api/session/seat/transfer", &body)
                .await?;
            Ok(envelope.get("result").cloned().unwrap_or(envelope))
        }
        RyeOsEffectKind::PrepareTransfer {
            transfer,
            export,
            freeze,
            request_bounds,
        } => {
            let producer = client
                .ui_producer
                .read()
                .expect("producer lock poisoned")
                .clone()
                .ok_or_else(|| {
                    ClientError::UiBindingRequest(
                        "transfer preparation producer is not ready".into(),
                    )
                })?;
            if producer.presentation_id != transfer.source.presentation_id
                || producer.thread_id != transfer.source.thread_id
                || producer.producer_incarnation != transfer.source.producer_incarnation
            {
                return Err(ClientError::UiBindingRequest(
                    "transfer preparation has another active producer".into(),
                ));
            }
            let body = transfer_prepare_body(transfer, export, freeze, *request_bounds)?;
            let envelope = client
                .signed_post("/ui/api/session/seat/transfer", &body)
                .await?;
            Ok(envelope.get("result").cloned().unwrap_or(envelope))
        }
        RyeOsEffectKind::InvokePresentationOperation { operation } => {
            let admission = operation.admission().ok_or_else(|| {
                ClientError::UiBindingRequest(
                    "composition operations are not executable invocations".into(),
                )
            })?;
            operation
                .validate()
                .map_err(|error| ClientError::UiBindingRequest(error.to_owned()))?;
            admission
                .request
                .validate_bounds(operation.request_bounds)
                .map_err(|error| ClientError::UiBindingRequest(error.to_string()))?;
            let envelope = client
                .invoke_presentation_operation(&admission.request, &operation.operation_id)
                .await?;
            Ok(envelope
                .pointer("/result/result")
                .or_else(|| envelope.get("result"))
                .cloned()
                .unwrap_or(envelope))
        }
        RyeOsEffectKind::OpenNewPresentationDocument { .. }
        | RyeOsEffectKind::ShowPreparedPresentationDocument { .. }
        | RyeOsEffectKind::SetLocationHash { .. }
        | RyeOsEffectKind::CopyToClipboard { .. }
        | RyeOsEffectKind::OpenUrl { .. } => Err(unsupported_native_effect(kind)),
        RyeOsEffectKind::ReplaceSession {
            session_id,
            launch_url,
        } => {
            {
                let delivery = client.seat_delivery.lock().await;
                if delivery.pending.is_some() || delivery.blocked {
                    return Err(ClientError::UiBindingRequest(
                        "unresolved seat delivery prevents session replacement".into(),
                    ));
                }
            }
            *client.ui_producer.write().expect("producer lock poisoned") = None;
            let redeemed = client.redeem_ui_session(session_id, launch_url).await?;
            let binding = super::root_seat_binding(&redeemed).map_err(ClientError::Io)?;
            let bootstrap = super::seat::bootstrap_seat(client, &binding)
                .await
                .map_err(ClientError::UiBindingRequest)?;
            let producer = bootstrap.producer.clone();
            if client
                .ui_producer
                .read()
                .expect("producer lock poisoned")
                .as_ref()
                != Some(&producer)
            {
                return Err(ClientError::UiBindingRequest(
                    "replacement producer is not current".into(),
                ));
            }
            let session = match client.current_ui_session().await {
                Ok(session) => session,
                Err(error) => {
                    super::seat::fence_failed_bootstrap(client, &producer).await;
                    return Err(error);
                }
            };
            let matches = session
                .presentations
                .iter()
                .filter(|membership| membership.presentation_id == producer.presentation_id)
                .collect::<Vec<_>>();
            if session.session_id != redeemed.session_id
                || matches.len() != 1
                || matches[0].thread_id.as_deref() != Some(producer.thread_id.as_str())
                || matches[0].chain_root_id.as_deref() != Some(bootstrap.chain_root_id.as_str())
                || matches[0].root_binding.binding_attachment_id != binding.binding_attachment_id
                || matches[0].root_binding.binding_generation != binding.binding_generation
                || matches[0].root_binding.binding_digest != binding.binding_digest
            {
                super::seat::fence_failed_bootstrap(client, &producer).await;
                return Err(ClientError::UiBindingRequest(
                    "replacement membership changed before core construction".into(),
                ));
            }
            let replayed = bootstrap.replayed;
            let handoff = super::seat::LiveSeatHandoff {
                producer: bootstrap.producer,
                stream: bootstrap.live_history,
                tip_seq: bootstrap.replay_tip_seq,
                tip_hash: bootstrap.replay_tip_hash,
                claim_request: bootstrap.claim_request,
                claim_next_engine_seq: bootstrap.claim_next_engine_seq,
            };
            let mut slot = client.seat_live_handoff.lock().await;
            if super::seat::reserve_live_handoff(&mut *slot, handoff).is_err() {
                drop(slot);
                super::seat::fence_failed_bootstrap(client, &producer).await;
                return Err(ClientError::UiBindingRequest(
                    "prior replacement live history was not consumed".into(),
                ));
            }
            Ok(
                serde_json::json!({"session":session,"presentation_id":producer.presentation_id,"replayed":replayed}),
            )
        }
        RyeOsEffectKind::ReleaseBindingAttachment {
            binding_attachment_id,
            binding_generation,
            binding_digest,
        } => {
            let envelope = client
                .signed_post(
                    "/ui/api/session/attachments/detach",
                    &serde_json::json!({
                        "binding_attachment_id": binding_attachment_id,
                        "binding_generation": binding_generation,
                        "binding_digest": binding_digest,
                    }),
                )
                .await?;
            Ok(envelope.get("result").cloned().unwrap_or(envelope))
        }
    }
}

fn result_kind_for(kind: &RyeOsEffectKind) -> RyeOsEffectResultKind {
    match kind {
        RyeOsEffectKind::ObserveTransferDestination { .. } => {
            RyeOsEffectResultKind::TransferDestinationObserved
        }
        RyeOsEffectKind::FetchSource { .. } => RyeOsEffectResultKind::SourceData,
        RyeOsEffectKind::InvokeBinding { .. } => RyeOsEffectResultKind::BindingInvoked,
        RyeOsEffectKind::PrepareTransfer { .. } => RyeOsEffectResultKind::BindingInvoked,
        RyeOsEffectKind::ContactTransfer { .. } => RyeOsEffectResultKind::BindingInvoked,
        RyeOsEffectKind::InvokePresentationOperation { .. } => {
            RyeOsEffectResultKind::BindingInvoked
        }
        RyeOsEffectKind::OpenNewPresentationDocument { .. }
        | RyeOsEffectKind::ShowPreparedPresentationDocument { .. }
        | RyeOsEffectKind::SetLocationHash { .. }
        | RyeOsEffectKind::CopyToClipboard { .. }
        | RyeOsEffectKind::OpenUrl { .. }
        | RyeOsEffectKind::ReplaceSession { .. }
        | RyeOsEffectKind::ReleaseBindingAttachment { .. } => RyeOsEffectResultKind::BrowserOnly,
    }
}

fn transfer_contact_body(
    request: &ryeos_client_base::ui::effect::UiTransferContactRequest,
    bounds: ryeos_client_base::ui::binding::UiBindingRequestBounds,
) -> Result<serde_json::Value, ClientError> {
    let body = serde_json::to_value(request)?;
    if bounds.max_request_bytes == 0
        || serde_json::to_vec(&body)?.len() as u64 > bounds.max_request_bytes
    {
        return Err(ClientError::UiBindingRequest(
            "transfer contact exceeds admitted request bound".into(),
        ));
    }
    Ok(body)
}

fn transfer_prepare_body(
    transfer: &ryeos_ui_presentation_contract::transfer::TransferIdentity,
    export: &ryeos_client_base::ui::transfer_export::UiTransferExport,
    freeze: &Option<ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence>,
    bounds: ryeos_client_base::ui::binding::UiBindingRequestBounds,
) -> Result<serde_json::Value, ClientError> {
    let body =
        serde_json::json!({"action":"prepare","transfer":transfer,"export":export,"freeze":freeze});
    if bounds.max_request_bytes == 0
        || serde_json::to_vec(&body)?.len() as u64 > bounds.max_request_bytes
    {
        return Err(ClientError::UiBindingRequest(
            "transfer preparation exceeds admitted request bound".into(),
        ));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_contact_is_flat_and_exactly_byte_bounded() {
        let request = serde_json::from_value::<ryeos_client_base::ui::effect::UiTransferContactRequest>(
            serde_json::json!({
                "action": "commit",
                "transfer": {
                    "operation_id": "c".repeat(64),
                    "export_digest": "d".repeat(64),
                    "source": {"presentation_id":"source", "thread_id":"source", "chain_root_id":"source", "producer_incarnation":"a".repeat(64), "revision":7},
                    "destination": {"presentation_id":"destination", "thread_id":"destination", "chain_root_id":"destination", "producer_incarnation":"b".repeat(64), "revision":9}
                }
            }),
        ).unwrap();
        let encoded = serde_json::to_vec(&request).unwrap();
        let bounds = ryeos_client_base::ui::binding::UiBindingRequestBounds {
            max_request_bytes: encoded.len() as u64,
            max_input_bytes: 65536,
            max_seat_append_bytes: 65536,
        };
        let body = transfer_contact_body(&request, bounds).unwrap();
        assert_eq!(body["action"], "commit");
        assert_eq!(body["transfer"]["source"]["revision"], 7);
        assert!(body.get("request").is_none());
        assert_eq!(serde_json::to_vec(&body).unwrap().len(), encoded.len());
        assert!(
            transfer_contact_body(
                &request,
                ryeos_client_base::ui::binding::UiBindingRequestBounds {
                    max_request_bytes: bounds.max_request_bytes - 1,
                    ..bounds
                }
            )
            .is_err()
        );
        assert!(
            transfer_contact_body(
                &request,
                ryeos_client_base::ui::binding::UiBindingRequestBounds {
                    max_request_bytes: 0,
                    ..bounds
                }
            )
            .is_err()
        );
    }

    #[test]
    fn unsupported_native_capabilities_are_definite_failures_not_success() {
        for kind in [
            RyeOsEffectKind::OpenNewPresentationDocument {
                expected_session_id: "fixture-session".into(),
                root_binding: ryeos_client_base::ui::UiBindingAttachmentCoordinate {
                    binding_attachment_id: "fixture-root".into(),
                    binding_generation: 1,
                    binding_digest: "ab".repeat(32),
                },
            },
            RyeOsEffectKind::ShowPreparedPresentationDocument {
                presentation_id: "11111111-1111-4111-8111-111111111111".into(),
            },
            RyeOsEffectKind::SetLocationHash {
                hash: "#fixture".into(),
            },
            RyeOsEffectKind::CopyToClipboard {
                text: "fixture".into(),
            },
            RyeOsEffectKind::OpenUrl {
                url: "https://example.invalid/".into(),
            },
        ] {
            let effect = RyeOsEffect { id: 42, kind };
            let result = effect_result(&effect, Err(unsupported_native_effect(&effect.kind)));
            assert_eq!(result.id, effect.id);
            assert_eq!(result.kind, RyeOsEffectResultKind::BrowserOnly);
            assert!(!result.ok);
            assert!(result.data.is_none());
            let error = result.error.unwrap();
            assert_eq!(error.code, "platform_effect_failed");
            assert!(error.message.contains("does not implement"));
        }
    }
}
