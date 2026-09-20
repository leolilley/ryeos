//! Occurrence-scoped bootstrap and exact external execution channel attachment.

use std::sync::Arc;

use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema: u32,
    pub placement_thread_id: String,
    pub occurrence_id: String,
    pub bootstrap_capability: String,
    pub supervisor_public_key: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeRequest {
    pub schema: u32,
    pub placement_thread_id: String,
    pub occurrence_id: String,
    pub frame_base64: String,
}

impl Request {
    pub fn validate_shape(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported external attachment schema");
        ensure!(
            !self.placement_thread_id.is_empty() && self.placement_thread_id.len() <= 256,
            "external attachment placement is invalid"
        );
        ensure!(
            !self.occurrence_id.is_empty() && self.occurrence_id.len() <= 512,
            "external attachment occurrence is invalid"
        );
        ensure!(
            self.bootstrap_capability.len() == 44,
            "external attachment capability has the wrong length"
        );
        let capability = STANDARD
            .decode(&self.bootstrap_capability)
            .map_err(|_| anyhow::anyhow!("external attachment capability is invalid"))?;
        ensure!(
            capability.len() == 32 && STANDARD.encode(capability) == self.bootstrap_capability,
            "external attachment capability is not canonical"
        );
        ryeos_state::external_execution::validate_channel_public_key(&self.supervisor_public_key)?;
        Ok(())
    }

    pub fn principal_id(&self) -> String {
        format!("external-occurrence:{}", self.occurrence_id)
    }
}

impl ExchangeRequest {
    pub fn validate_shape(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported external exchange schema");
        ensure!(
            !self.placement_thread_id.is_empty() && self.placement_thread_id.len() <= 256,
            "external exchange placement is invalid"
        );
        ensure!(
            !self.occurrence_id.is_empty() && self.occurrence_id.len() <= 512,
            "external exchange occurrence is invalid"
        );
        self.decode_frame().map(|_| ())
    }

    pub fn decode_frame(&self) -> Result<Vec<u8>> {
        ensure!(
            !self.frame_base64.is_empty()
                && self.frame_base64.len()
                    <= ryeos_state::external_execution::MAX_FRAME_BYTES.div_ceil(3) * 4,
            "external exchange frame exceeds its encoded bound"
        );
        let wire = STANDARD
            .decode(&self.frame_base64)
            .map_err(|_| anyhow::anyhow!("external exchange frame is invalid base64"))?;
        ensure!(
            wire.len() <= ryeos_state::external_execution::MAX_FRAME_BYTES
                && STANDARD.encode(&wire) == self.frame_base64,
            "external exchange frame is not canonical"
        );
        Ok(wire)
    }

    pub fn principal_id(&self) -> String {
        format!("external-occurrence:{}", self.occurrence_id)
    }
}

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
    Ok(serde_json::json!({
        "schema": 1,
        "binding": binding,
        "binding_digest": binding.digest()?,
    }))
}

pub async fn handle_exchange(
    req: ExchangeRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    req.validate_shape()?;
    require_exchange_principal(&req, &ctx)?;
    let wire = req.decode_frame()?;
    let authenticated = ryeos_app::external_placement::authenticate_external_channel_frame(
        &state,
        &req.placement_thread_id,
        &req.occurrence_id,
        &wire,
    )?;
    let result = ryeos_app::external_placement::exchange_external_channel_frame(
        &state,
        &authenticated,
        &wire,
    )?;
    let frames = result
        .outbound()
        .iter()
        .map(|frame| {
            serde_json::json!({
                "sequence": frame.sequence(),
                "frame_digest": frame.frame_digest(),
                "frame_base64": STANDARD.encode(frame.canonical_wire()),
            })
        })
        .collect::<Vec<_>>();
    let urgent_revocation = result.urgent_revocation().map(|frame| {
        serde_json::json!({
            "sequence": frame.sequence(),
            "frame_digest": frame.frame_digest(),
            "frame_base64": STANDARD.encode(frame.canonical_wire()),
        })
    });
    Ok(serde_json::json!({
        "schema": 1,
        "incoming_new": result.incoming_new(),
        "incoming_sequence": authenticated.sequence(),
        "incoming_frame_digest": authenticated.frame_digest(),
        "acknowledgement_frame_digest": result.acknowledgement_digest(),
        "outbound_frames": frames,
        "urgent_revocation_frame": urgent_revocation,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-execution/channel-attach",
    endpoint: "external_execution.channel_attach",
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
    endpoint: "external_execution.channel_exchange",
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
