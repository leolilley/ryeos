//! Occurrence-scoped bootstrap and exact external execution channel attachment.

use std::sync::Arc;

use anyhow::{Result, ensure};
#[cfg(test)]
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

use ryeos_state::external_execution::transport::{
    EXTERNAL_CHANNEL_TRANSPORT_SCHEMA, ExternalChannelAttachResponse,
    ExternalChannelExchangeResponse, ExternalChannelResponseFrame,
};
pub use ryeos_state::external_execution::transport::{
    ExternalChannelAttachRequest as Request, ExternalChannelExchangeRequest as ExchangeRequest,
};

fn require_occurrence_principal(req: &Request, ctx: &HandlerContext) -> Result<()> {
    ctx.require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(
        ctx.fingerprint == req.principal_id(),
        "external attachment principal changed its occurrence"
    );
    Ok(())
}

fn require_exchange_principal(req: &ExchangeRequest, ctx: &HandlerContext) -> Result<()> {
    ctx.require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    ensure!(
        ctx.fingerprint == req.principal_id(),
        "external exchange principal changed its occurrence"
    );
    Ok(())
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    req.validate_shape()?;
    require_occurrence_principal(&req, &ctx)?;
    let authenticated = ryeos_app::external_placement::authenticate_external_channel_bootstrap(
        &state,
        &req.placement_thread_id,
        &req.occurrence_id,
        &req.bootstrap_capability,
    )?;
    let binding = ryeos_app::external_placement::attach_external_execution_channel(
        &state,
        &authenticated,
        &req.supervisor_public_key,
    )?;
    Ok(serde_json::to_value(ExternalChannelAttachResponse {
        schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
        binding_digest: binding.digest()?,
        binding,
    })?)
}

pub async fn handle_exchange(
    req: ExchangeRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    req.validate_shape()?;
    require_exchange_principal(&req, &ctx)?;
    // The exchange blocks on durable state and, for a lease not yet admitted,
    // on the readiness judgement, so it runs off the async workers.
    tokio::task::spawn_blocking(move || exchange_blocking(&req, &state))
        .await
        .map_err(|error| anyhow::anyhow!("external channel exchange task failed: {error}"))?
}

fn exchange_blocking(req: &ExchangeRequest, state: &AppState) -> Result<Value> {
    if ryeos_app::external_placement::external_channel_exchanges_on_stream(
        state,
        &req.placement_thread_id,
    )? {
        return Err(stream_selected_refusal().into());
    }
    let wire = req.decode_frame()?;
    let exchanged =
        exchange_frame_response(state, &req.placement_thread_id, &req.occurrence_id, &wire);
    let response = match exchanged {
        Ok(response) => response,
        Err(error) => {
            // A frame that refuses the lease records the refusal durably;
            // the standing it left names the verdict.
            if let Err(ryeos_app::external_placement::ExternalChannelStreamRefusal::OccurrenceRefused(
                _,
            )) = ryeos_app::external_placement::external_channel_stream_admission(
                state,
                &req.placement_thread_id,
                &req.occurrence_id,
            ) {
                return Err(occurrence_refused().into());
            }
            return Err(error);
        }
    };
    Ok(serde_json::to_value(response)?)
}

/// The occurrence's lease was durably refused: the exchange never serves it
/// again, on either carrier.
pub const EXCHANGE_OCCURRENCE_REFUSED: &str = "external_channel_stream_occurrence_refused";

fn occurrence_refused() -> ryeos_app::handler_error::HandlerError {
    ryeos_app::handler_error::HandlerError::Structured {
        code: EXCHANGE_OCCURRENCE_REFUSED.to_owned(),
        status: 410,
        body: serde_json::json!({ "error": EXCHANGE_OCCURRENCE_REFUSED }),
    }
}

/// A binding whose signed contract selects the channel stream exchanges only
/// there; its request/response exchange is refused, never served as a second
/// carrier. Attachment stays on this route under either contract.
pub const EXCHANGE_STREAM_SELECTED: &str = "external_channel_exchange_stream_selected";

fn stream_selected_refusal() -> ryeos_app::handler_error::HandlerError {
    ryeos_app::handler_error::HandlerError::Structured {
        code: EXCHANGE_STREAM_SELECTED.to_owned(),
        status: 409,
        body: serde_json::json!({ "error": EXCHANGE_STREAM_SELECTED }),
    }
}

/// Authenticate one supervisor frame against the occurrence binding and
/// exchange it. The request/response route and the channel stream both
/// answer with exactly this response, so a guest driver cannot tell them
/// apart beyond the transport.
pub fn exchange_frame_response(
    state: &AppState,
    placement_thread_id: &str,
    occurrence_id: &str,
    wire: &[u8],
) -> Result<ExternalChannelExchangeResponse> {
    let authenticated = ryeos_app::external_placement::authenticate_external_channel_frame(
        state,
        placement_thread_id,
        occurrence_id,
        wire,
    )?;
    let result = ryeos_app::external_placement::exchange_external_channel_frame(
        state,
        &authenticated,
        wire,
    )?;
    let frames = result
        .outbound()
        .iter()
        .map(|frame| {
            ExternalChannelResponseFrame::new(
                frame.sequence(),
                frame.frame_digest().to_owned(),
                frame.canonical_wire(),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let urgent_revocation = result
        .urgent_revocation()
        .map(|frame| {
            ExternalChannelResponseFrame::new(
                frame.sequence(),
                frame.frame_digest().to_owned(),
                frame.canonical_wire(),
            )
        })
        .transpose()?;
    let response = ExternalChannelExchangeResponse {
        schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
        incoming_new: result.incoming_new(),
        incoming_sequence: authenticated.sequence(),
        incoming_frame_digest: authenticated.frame_digest().to_owned(),
        acknowledgement_frame_digest: result.acknowledgement_digest().map(str::to_owned),
        outbound_frames: frames,
        urgent_revocation_frame: urgent_revocation,
    };
    response.validate_shape()?;
    Ok(response)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-execution/channel-attach",
    endpoint: "external-execution.channel-attach",
    availability: ServiceAvailability::DaemonOnly,
    // The occurrence verifier grants no general RyeOS scope, so this service
    // has no static capability requirement. The handler independently requires
    // its exact synthetic occurrence principal; an ordinary /execute principal
    // cannot authorize attachment even if it can resolve the service item.
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

pub const EXCHANGE_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-execution/channel-exchange",
    endpoint: "external-execution.channel-exchange",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ExchangeRequest = crate::handler_error::parse_request(params)?;
            handle_exchange(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_selected_exchange_is_refused_by_its_own_name() {
        let error: anyhow::Error = stream_selected_refusal().into();
        match ryeos_app::handler_error::extract_handler_error(&error) {
            Some(ryeos_app::handler_error::HandlerError::Structured { code, status, body }) => {
                assert_eq!(code, "external_channel_exchange_stream_selected");
                assert_eq!(status, 409);
                assert_eq!(body["error"], "external_channel_exchange_stream_selected");
            }
            other => panic!("expected the named stream-selected refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_refused_occurrence_is_gone_by_its_own_name() {
        let error: anyhow::Error = occurrence_refused().into();
        match ryeos_app::handler_error::extract_handler_error(&error) {
            Some(ryeos_app::handler_error::HandlerError::Structured { code, status, body }) => {
                assert_eq!(code, "external_channel_stream_occurrence_refused");
                assert_eq!(status, 410);
                assert_eq!(body["error"], "external_channel_stream_occurrence_refused");
            }
            other => panic!("expected the named occurrence-refused refusal, got {other:?}"),
        }
    }

    fn fixture_request() -> Request {
        Request {
            schema: 1,
            placement_thread_id: "T-placement".into(),
            occurrence_id: "occurrence-one".into(),
            bootstrap_capability: "A".repeat(43) + "=",
            supervisor_public_key: ryeos_state::external_execution::encode_channel_public_key(
                &lillux::crypto::SigningKey::from_bytes(&[31; 32]).verifying_key(),
            )
            .unwrap(),
        }
    }

    #[test]
    fn attachment_shape_is_closed_and_occurrence_scoped() {
        let request = fixture_request();
        request.validate_shape().unwrap();
        assert_eq!(request.principal_id(), "external-occurrence:occurrence-one");
        let mut invalid = fixture_request();
        invalid.schema = 2;
        assert!(invalid.validate_shape().is_err());
        let mut invalid = fixture_request();
        invalid.supervisor_public_key = "A".repeat(44);
        assert!(invalid.validate_shape().is_err());
    }

    #[test]
    fn unknown_request_fields_refuse() {
        let value = serde_json::json!({
            "schema":1,
            "placement_thread_id":"T-placement",
            "occurrence_id":"occurrence-one",
            "bootstrap_capability":"A".repeat(43) + "=",
            "supervisor_public_key":fixture_request().supervisor_public_key,
            "extra":true,
        });
        assert!(serde_json::from_value::<Request>(value).is_err());
    }

    #[test]
    fn ordinary_or_unverified_service_principals_cannot_attach() {
        let request = fixture_request();
        assert!(
            require_occurrence_principal(
                &request,
                &HandlerContext::new("operator-fingerprint".into(), Vec::new(), true),
            )
            .is_err()
        );
        assert!(
            require_occurrence_principal(
                &request,
                &HandlerContext::new(request.principal_id(), Vec::new(), false),
            )
            .is_err()
        );
        require_occurrence_principal(
            &request,
            &HandlerContext::new(request.principal_id(), Vec::new(), true),
        )
        .unwrap();
    }

    #[test]
    fn exchange_shape_is_bounded_closed_and_occurrence_scoped() {
        let request = ExchangeRequest {
            schema: 1,
            placement_thread_id: "T-placement".into(),
            occurrence_id: "occurrence-one".into(),
            frame_base64: STANDARD.encode(b"{}"),
        };
        request.validate_shape().unwrap();
        assert_eq!(request.decode_frame().unwrap(), b"{}");
        assert_eq!(request.principal_id(), "external-occurrence:occurrence-one");
        require_exchange_principal(
            &request,
            &HandlerContext::new(request.principal_id(), Vec::new(), true),
        )
        .unwrap();
        assert!(
            require_exchange_principal(
                &request,
                &HandlerContext::new("operator".into(), Vec::new(), true),
            )
            .is_err()
        );
        let unknown = serde_json::json!({
            "schema":1,
            "placement_thread_id":"T-placement",
            "occurrence_id":"occurrence-one",
            "frame_base64":STANDARD.encode(b"{}"),
            "extra":true,
        });
        assert!(serde_json::from_value::<ExchangeRequest>(unknown).is_err());
    }

    #[test]
    fn exchange_rejects_noncanonical_or_oversized_frame_encoding() {
        let mut request = ExchangeRequest {
            schema: 1,
            placement_thread_id: "T-placement".into(),
            occurrence_id: "occurrence-one".into(),
            frame_base64: "e30".into(),
        };
        assert!(request.validate_shape().is_err());
        request.frame_base64 =
            "A".repeat(ryeos_state::external_execution::MAX_FRAME_BYTES.div_ceil(3) * 4 + 1);
        assert!(request.validate_shape().is_err());
    }
}
