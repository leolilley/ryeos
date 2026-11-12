//! Executing the effects the core emits: each effect becomes one daemon call
//! (or one host action) and exactly one result.
//!
//! Rules every host shares:
//!
//! * the current producer is read when the effect runs, not when it was
//!   submitted, so a fenced producer fails queued effects instead of
//!   dispatching under authority that is gone;
//! * request bounds are measured on the wrapped body the daemon receives
//!   (`max_request_bytes`) and, for free-text input, on the input itself
//!   (`max_input_bytes`);
//! * a daemon answer is unwrapped as `result.result ?? result ?? envelope`;
//! * a failure reports outcome `unknown` only for an effect that mutates
//!   remote state and failed at delivery or decoding; everything else is a
//!   definite refusal;
//! * effects the runtime cannot perform itself (hash navigation, clipboard,
//!   opening URLs, and navigating to a successor session) are handed to the
//!   host's [`AdapterHandler`]; independent presentation documents are
//!   prepared here and offered through [`DocumentOffers`] when the host
//!   installs them, else handed to the adapter too.
//!
//! A batch of effects runs concurrently and reports as one message, in
//! emission order.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Poll;

use ryeos_client_base::ui::binding::UiPresentationProducer;
use ryeos_client_base::ui::{
    BrowserViewport, RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind,
    RyeOsUiError,
};
use serde_json::{Value, json};

use crate::api::{
    ATTACHMENT_DETACH_PATH, INVOCATION_DISPATCH_PATH, SEAT_OPEN_PATH, SEAT_TRANSFER_PATH,
    detach_request, dispatch_request, observe_destination_request, transfer_contact_request,
    transfer_prepare_request,
};
use crate::codec::decode_strict;
use crate::documents::DocumentOffers;
use crate::error::RuntimeError;
use crate::inbox::{Inbox, Stream, StreamTag, result_kind_for};
use crate::platform::{Auth, Platform, ReplacementMode, Transport};
use crate::replay::CoreConfig;
use crate::seat::{SeatShared, random_uuid, replace_session_in_place};

/// What a host action produced: optional data for the result.
pub type AdapterFuture = Pin<Box<dyn Future<Output = Result<Option<Value>, RuntimeError>>>>;

/// The host's implementation of the effects the runtime cannot perform.
pub type AdapterHandler = Rc<dyn Fn(RyeOsEffect) -> AdapterFuture>;

/// The handler of a host that implements none of them: every such effect is
/// a definite failure naming the missing capability.
pub fn unsupported_adapter() -> AdapterHandler {
    Rc::new(|effect: RyeOsEffect| -> AdapterFuture {
        Box::pin(async move {
            let capability = match effect.kind {
                RyeOsEffectKind::OpenNewPresentationDocument { .. }
                | RyeOsEffectKind::ShowPreparedPresentationDocument { .. } => {
                    "independent document hosting"
                }
                RyeOsEffectKind::SetLocationHash { .. } => "location hash navigation",
                RyeOsEffectKind::CopyToClipboard { .. } => "clipboard copying",
                RyeOsEffectKind::OpenUrl { .. } => "external URL opening",
                RyeOsEffectKind::ReplaceSession { .. } => "session replacement by navigation",
                _ => "this effect",
            };
            Err(RuntimeError::BindingRequest(format!(
                "This host does not implement {capability}."
            )))
        })
    })
}

/// Everything an effect needs to run.
pub struct EffectEnv<P: Platform> {
    pub shared: Rc<SeatShared<P>>,
    pub adapter: AdapterHandler,
    /// Where independent presentation documents are offered, for a host that
    /// offers them; otherwise those effects go to the adapter.
    pub documents: Option<Rc<DocumentOffers>>,
    pub viewport: Rc<Cell<BrowserViewport>>,
    pub config: CoreConfig,
    pub inbox: Rc<Inbox>,
    /// The effect stream tag the batch was submitted under. A batch whose tag
    /// is no longer current fails without running.
    pub tag: StreamTag,
}

/// Remove the `result` envelope layers: `result.result`, else `result`, else
/// the envelope itself (a JSON `null` counts as absent).
pub fn unwrap_envelope(envelope: &Value) -> Value {
    let Some(map) = envelope.as_object() else {
        return envelope.clone();
    };
    match map.get("result") {
        Some(Value::Object(inner)) => match inner.get("result") {
            Some(deeper) if !deeper.is_null() => deeper.clone(),
            _ => Value::Object(inner.clone()),
        },
        Some(other) if !other.is_null() => other.clone(),
        _ => envelope.clone(),
    }
}

fn is_mutation(kind: &RyeOsEffectKind) -> bool {
    matches!(
        kind,
        RyeOsEffectKind::InvokeBinding { .. }
            | RyeOsEffectKind::PrepareTransfer { .. }
            | RyeOsEffectKind::ContactTransfer { .. }
            | RyeOsEffectKind::InvokePresentationOperation { .. }
            | RyeOsEffectKind::ReplaceSession { .. }
            | RyeOsEffectKind::ReleaseBindingAttachment { .. }
    )
}

/// The result of an effect that failed with `error`.
pub fn failure_result(effect: &RyeOsEffect, error: &RuntimeError) -> RyeOsEffectResult {
    let unknown = is_mutation(&effect.kind) && error.is_unknown_delivery();
    let ui_error = if unknown {
        let code = match &effect.kind {
            RyeOsEffectKind::ReplaceSession { .. } => "session_replacement_outcome_unknown",
            RyeOsEffectKind::ReleaseBindingAttachment { .. } => {
                "attachment_release_outcome_unknown"
            }
            _ => "invocation_outcome_unknown",
        };
        RyeOsUiError::outcome_unknown(code, error.to_string())
    } else {
        RyeOsUiError::definite("platform_effect_failed", error.to_string())
    };
    RyeOsEffectResult {
        id: effect.id,
        ok: false,
        kind: result_kind_for(effect),
        data: None,
        error: Some(ui_error),
    }
}

fn success_result(
    effect: &RyeOsEffect,
    kind: RyeOsEffectResultKind,
    data: Option<Value>,
) -> RyeOsEffectResult {
    RyeOsEffectResult {
        id: effect.id,
        ok: true,
        kind,
        data,
        error: None,
    }
}

async fn post<P: Platform>(
    platform: &P,
    path: &str,
    encoded: String,
) -> Result<Value, RuntimeError> {
    let text = platform.transport().post_encoded(path, encoded).await?;
    let envelope = decode_strict(&text).map_err(|_| RuntimeError::UnreadableResponse {
        path: path.to_owned(),
    })?;
    Ok(unwrap_envelope(&envelope))
}

fn require_producer(
    producer: Option<UiPresentationProducer>,
    message: &str,
) -> Result<UiPresentationProducer, RuntimeError> {
    producer.ok_or_else(|| RuntimeError::BindingRequest(message.to_owned()))
}

async fn run<P: Platform>(
    env: &EffectEnv<P>,
    effect: &RyeOsEffect,
) -> Result<(RyeOsEffectResultKind, Option<Value>), RuntimeError> {
    let platform = &*env.shared.platform;
    // Read at execution time: a producer fenced since submission fails here.
    let producer = env.shared.current_producer();
    match &effect.kind {
        RyeOsEffectKind::FetchSource {
            request,
            request_bounds,
            ..
        } => {
            let producer = require_producer(
                producer,
                "UI dispatch requires an acknowledged active presentation producer",
            )?;
            let encoded = dispatch_request(&producer, request, None, *request_bounds)?;
            let data = post(platform, INVOCATION_DISPATCH_PATH, encoded).await?;
            Ok((RyeOsEffectResultKind::SourceData, Some(data)))
        }
        RyeOsEffectKind::InvokeBinding {
            request,
            request_bounds,
            ..
        } => {
            let producer = require_producer(
                producer,
                "UI dispatch requires an acknowledged active presentation producer",
            )?;
            let encoded = dispatch_request(&producer, request, None, *request_bounds)?;
            let data = post(platform, INVOCATION_DISPATCH_PATH, encoded).await?;
            Ok((RyeOsEffectResultKind::BindingInvoked, Some(data)))
        }
        RyeOsEffectKind::InvokePresentationOperation { operation } => {
            let admission = operation.admission().ok_or_else(|| {
                RuntimeError::BindingRequest(
                    "composition operations are not executable invocations".to_owned(),
                )
            })?;
            operation
                .validate()
                .map_err(|message| RuntimeError::BindingRequest(message.to_owned()))?;
            let producer = require_producer(
                producer,
                "UI dispatch requires an acknowledged active presentation producer",
            )?;
            let encoded = dispatch_request(
                &producer,
                &admission.request,
                Some(&operation.operation_id),
                operation.request_bounds,
            )?;
            let data = post(platform, INVOCATION_DISPATCH_PATH, encoded).await?;
            Ok((RyeOsEffectResultKind::BindingInvoked, Some(data)))
        }
        RyeOsEffectKind::ObserveTransferDestination {
            destination_presentation_id,
            destination_thread_id,
            destination_chain_root_id,
            source_producer,
            request_bounds,
            ..
        } => {
            if producer.as_ref() != Some(source_producer) {
                return Err(RuntimeError::BindingRequest(
                    "transfer observation has another current source producer".to_owned(),
                ));
            }
            let encoded = observe_destination_request(
                destination_presentation_id,
                destination_thread_id,
                destination_chain_root_id,
                *request_bounds,
            )?;
            let observation = post(platform, SEAT_OPEN_PATH, encoded).await?;
            Ok((
                RyeOsEffectResultKind::TransferDestinationObserved,
                Some(json!({
                    "operation_id": random_uuid(platform),
                    "observation": observation,
                })),
            ))
        }
        RyeOsEffectKind::ContactTransfer {
            request,
            request_bounds,
        } => {
            let producer = require_producer(
                producer,
                "transfer contact requires an acknowledged active presentation producer",
            )?;
            let actor = request.actor_seat();
            if producer.presentation_id != actor.presentation_id
                || producer.thread_id != actor.thread_id
            {
                return Err(RuntimeError::BindingRequest(
                    "transfer contact has another active producer".to_owned(),
                ));
            }
            // The transfer's incarnation is original provenance, not the
            // current lease; the seat owner authenticates the current one.
            let encoded = transfer_contact_request(request, *request_bounds)?;
            let data = post(platform, SEAT_TRANSFER_PATH, encoded).await?;
            Ok((RyeOsEffectResultKind::BindingInvoked, Some(data)))
        }
        RyeOsEffectKind::PrepareTransfer {
            transfer,
            export,
            freeze,
            request_bounds,
        } => {
            let producer = require_producer(
                producer,
                "transfer preparation requires an acknowledged active presentation producer",
            )?;
            if producer.presentation_id != transfer.source.presentation_id
                || producer.thread_id != transfer.source.thread_id
                || producer.producer_incarnation != transfer.source.producer_incarnation
            {
                return Err(RuntimeError::BindingRequest(
                    "transfer preparation has another active producer".to_owned(),
                ));
            }
            let encoded = transfer_prepare_request(transfer, export, freeze, *request_bounds)?;
            let data = post(platform, SEAT_TRANSFER_PATH, encoded).await?;
            Ok((RyeOsEffectResultKind::BindingInvoked, Some(data)))
        }
        RyeOsEffectKind::ReleaseBindingAttachment {
            binding_attachment_id,
            binding_generation,
            binding_digest,
        } => {
            let producer = require_producer(
                producer,
                "attachment release requires an acknowledged active presentation producer",
            )?;
            let encoded = detach_request(
                &producer,
                binding_attachment_id,
                *binding_generation,
                binding_digest,
            )?;
            let data = post(platform, ATTACHMENT_DETACH_PATH, encoded).await?;
            Ok((RyeOsEffectResultKind::BrowserOnly, Some(data)))
        }
        RyeOsEffectKind::ReplaceSession {
            session_id,
            launch_url,
        } => match platform.auth().replacement() {
            ReplacementMode::InPlace => {
                let data = replace_session_in_place(
                    &env.shared,
                    session_id,
                    launch_url,
                    env.viewport.get(),
                    env.config,
                )
                .await?;
                Ok((RyeOsEffectResultKind::BrowserOnly, Some(data)))
            }
            ReplacementMode::Navigate => {
                let data = (env.adapter)(effect.clone()).await?;
                Ok((RyeOsEffectResultKind::BrowserOnly, data))
            }
        },
        RyeOsEffectKind::OpenNewPresentationDocument {
            expected_session_id,
            root_binding,
        } => match &env.documents {
            Some(documents) => {
                documents
                    .prepare_new(&*env.shared.platform, expected_session_id, root_binding)
                    .await?;
                Ok((RyeOsEffectResultKind::BrowserOnly, None))
            }
            None => {
                let data = (env.adapter)(effect.clone()).await?;
                Ok((RyeOsEffectResultKind::BrowserOnly, data))
            }
        },
        RyeOsEffectKind::ShowPreparedPresentationDocument { presentation_id } => {
            match &env.documents {
                Some(documents) => {
                    documents.offer_prepared(presentation_id)?;
                    Ok((RyeOsEffectResultKind::BrowserOnly, None))
                }
                None => {
                    let data = (env.adapter)(effect.clone()).await?;
                    Ok((RyeOsEffectResultKind::BrowserOnly, data))
                }
            }
        }
        RyeOsEffectKind::SetLocationHash { .. }
        | RyeOsEffectKind::CopyToClipboard { .. }
        | RyeOsEffectKind::OpenUrl { .. } => {
            let data = (env.adapter)(effect.clone()).await?;
            Ok((RyeOsEffectResultKind::BrowserOnly, data))
        }
    }
}

/// Run one effect to its result. Never fails: every error becomes a failed
/// result for the same effect id.
pub async fn execute_effect<P: Platform>(
    env: &EffectEnv<P>,
    effect: &RyeOsEffect,
) -> RyeOsEffectResult {
    if env.inbox.current_tag(Stream::Effects) != env.tag {
        return failure_result(
            effect,
            &RuntimeError::BindingRequest(
                "presentation bootstrap did not admit platform effects".to_owned(),
            ),
        );
    }
    match run(env, effect).await {
        Ok((kind, data)) => success_result(effect, kind, data),
        Err(error) => failure_result(effect, &error),
    }
}

/// Run a batch concurrently; results come back in emission order.
pub async fn execute_batch<P: Platform>(
    env: &EffectEnv<P>,
    effects: &[RyeOsEffect],
) -> Vec<RyeOsEffectResult> {
    let mut pending: Vec<Option<Pin<Box<dyn Future<Output = RyeOsEffectResult> + '_>>>> = effects
        .iter()
        .map(|effect| {
            let future: Pin<Box<dyn Future<Output = RyeOsEffectResult> + '_>> =
                Box::pin(execute_effect(env, effect));
            Some(future)
        })
        .collect();
    let mut results: Vec<Option<RyeOsEffectResult>> = effects.iter().map(|_| None).collect();
    std::future::poll_fn(|context| {
        let mut waiting = false;
        for (slot, result) in pending.iter_mut().zip(results.iter_mut()) {
            if let Some(future) = slot.as_mut() {
                match future.as_mut().poll(context) {
                    Poll::Ready(value) => {
                        *result = Some(value);
                        *slot = None;
                    }
                    Poll::Pending => waiting = true,
                }
            }
        }
        if waiting {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    results.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_unwrapping_prefers_the_innermost_result() {
        assert_eq!(
            unwrap_envelope(&json!({"result": {"result": {"a": 1}}})),
            json!({"a": 1})
        );
        assert_eq!(
            unwrap_envelope(&json!({"result": {"a": 1}})),
            json!({"a": 1})
        );
        assert_eq!(unwrap_envelope(&json!({"result": [1]})), json!([1]));
        assert_eq!(unwrap_envelope(&json!({"a": 1})), json!({"a": 1}));
        assert_eq!(
            unwrap_envelope(&json!({"result": null, "a": 1})),
            json!({"result": null, "a": 1})
        );
        assert_eq!(unwrap_envelope(&json!(7)), json!(7));
    }

    #[test]
    fn only_mutations_report_an_unknown_outcome() {
        use ryeos_client_base::ui::RyeOsEffectOutcome;
        let network = RuntimeError::Network {
            path: "/p".into(),
            message: "down".into(),
        };
        let hash = RyeOsEffect {
            id: 1,
            kind: RyeOsEffectKind::SetLocationHash { hash: "#x".into() },
        };
        let release = RyeOsEffect {
            id: 2,
            kind: RyeOsEffectKind::ReleaseBindingAttachment {
                binding_attachment_id: "a".into(),
                binding_generation: 1,
                binding_digest: "d".into(),
            },
        };
        let refused = failure_result(&hash, &network).error.unwrap();
        assert_eq!(refused.outcome, RyeOsEffectOutcome::Refused);
        assert_eq!(refused.code, "platform_effect_failed");
        let unknown = failure_result(&release, &network).error.unwrap();
        assert_eq!(unknown.outcome, RyeOsEffectOutcome::Unknown);
        assert_eq!(unknown.code, "attachment_release_outcome_unknown");
    }
}
