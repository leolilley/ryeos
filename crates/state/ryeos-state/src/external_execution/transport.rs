//! Closed JSON wire contract for occurrence bootstrap and frame exchange.
//!
//! Transport completion is not application evidence. These types only prevent
//! the daemon route and protected guest client from interpreting different
//! request/response shapes.

use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use super::{ExecutionChannelBinding, MAX_FRAME_BYTES, validate_channel_public_key};

pub const EXTERNAL_CHANNEL_TRANSPORT_SCHEMA: u32 = 1;
pub const MAX_PLACEMENT_ID_BYTES: usize = 256;
pub const MAX_OCCURRENCE_ID_BYTES: usize = 512;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChannelAttachRequest {
    pub schema: u32,
    pub placement_thread_id: String,
    pub occurrence_id: String,
    pub bootstrap_capability: String,
    pub supervisor_public_key: String,
}

impl ExternalChannelAttachRequest {
    pub fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.schema == EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            "unsupported external attachment schema"
        );
        bounded_text(
            &self.placement_thread_id,
            MAX_PLACEMENT_ID_BYTES,
            "external attachment placement",
        )?;
        bounded_text(
            &self.occurrence_id,
            MAX_OCCURRENCE_ID_BYTES,
            "external attachment occurrence",
        )?;
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
        validate_channel_public_key(&self.supervisor_public_key)?;
        Ok(())
    }

    pub fn principal_id(&self) -> String {
        format!("external-occurrence:{}", self.occurrence_id)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChannelAttachResponse {
    pub schema: u32,
    pub binding: ExecutionChannelBinding,
    pub binding_digest: String,
}

impl ExternalChannelAttachResponse {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            "unsupported external attachment response schema"
        );
        self.binding.validate()?;
        ensure!(
            self.binding.digest()? == self.binding_digest,
            "external attachment response changed its binding digest"
        );
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChannelExchangeRequest {
    pub schema: u32,
    pub placement_thread_id: String,
    pub occurrence_id: String,
    pub frame_base64: String,
}

impl ExternalChannelExchangeRequest {
    pub fn from_frame(
        placement_thread_id: impl Into<String>,
        occurrence_id: impl Into<String>,
        canonical_frame: &[u8],
    ) -> Result<Self> {
        ensure!(
            !canonical_frame.is_empty() && canonical_frame.len() <= MAX_FRAME_BYTES,
            "external exchange frame exceeds its wire bound"
        );
        let request = Self {
            schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            placement_thread_id: placement_thread_id.into(),
            occurrence_id: occurrence_id.into(),
            frame_base64: STANDARD.encode(canonical_frame),
        };
        request.validate_shape()?;
        Ok(request)
    }

    pub fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.schema == EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            "unsupported external exchange schema"
        );
        bounded_text(
            &self.placement_thread_id,
            MAX_PLACEMENT_ID_BYTES,
            "external exchange placement",
        )?;
        bounded_text(
            &self.occurrence_id,
            MAX_OCCURRENCE_ID_BYTES,
            "external exchange occurrence",
        )?;
        self.decode_frame().map(|_| ())
    }

    pub fn decode_frame(&self) -> Result<Vec<u8>> {
        ensure!(
            !self.frame_base64.is_empty()
                && self.frame_base64.len() <= MAX_FRAME_BYTES.div_ceil(3) * 4,
            "external exchange frame exceeds its encoded bound"
        );
        let wire = STANDARD
            .decode(&self.frame_base64)
            .map_err(|_| anyhow::anyhow!("external exchange frame is invalid base64"))?;
        ensure!(
            wire.len() <= MAX_FRAME_BYTES && STANDARD.encode(&wire) == self.frame_base64,
            "external exchange frame is not canonical"
        );
        Ok(wire)
    }

    pub fn principal_id(&self) -> String {
        format!("external-occurrence:{}", self.occurrence_id)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChannelResponseFrame {
    pub sequence: u64,
    pub frame_digest: String,
    pub frame_base64: String,
}

impl ExternalChannelResponseFrame {
    pub fn new(sequence: u64, frame_digest: String, canonical_frame: &[u8]) -> Result<Self> {
        ensure!(
            !canonical_frame.is_empty() && canonical_frame.len() <= MAX_FRAME_BYTES,
            "external response frame exceeds its wire bound"
        );
        let frame = Self {
            sequence,
            frame_digest,
            frame_base64: STANDARD.encode(canonical_frame),
        };
        frame.decode_frame()?;
        Ok(frame)
    }

    pub fn decode_frame(&self) -> Result<Vec<u8>> {
        ensure!(
            self.sequence > 0,
            "external response frame sequence is zero"
        );
        super::hash(&self.frame_digest)?;
        ensure!(
            !self.frame_base64.is_empty()
                && self.frame_base64.len() <= MAX_FRAME_BYTES.div_ceil(3) * 4,
            "external response frame exceeds its encoded bound"
        );
        let wire = STANDARD
            .decode(&self.frame_base64)
            .map_err(|_| anyhow::anyhow!("external response frame is invalid base64"))?;
        ensure!(
            !wire.is_empty()
                && wire.len() <= MAX_FRAME_BYTES
                && STANDARD.encode(&wire) == self.frame_base64,
            "external response frame is not canonical"
        );
        Ok(wire)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalChannelExchangeResponse {
    pub schema: u32,
    pub incoming_new: bool,
    pub incoming_sequence: u64,
    pub incoming_frame_digest: String,
    pub acknowledgement_frame_digest: Option<String>,
    pub outbound_frames: Vec<ExternalChannelResponseFrame>,
    pub urgent_revocation_frame: Option<ExternalChannelResponseFrame>,
}

impl ExternalChannelExchangeResponse {
    pub fn validate_shape(&self) -> Result<()> {
        ensure!(
            self.schema == EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            "unsupported external exchange response schema"
        );
        ensure!(
            self.incoming_sequence > 0,
            "external exchange response incoming sequence is zero"
        );
        super::hash(&self.incoming_frame_digest)?;
        if let Some(digest) = self.acknowledgement_frame_digest.as_deref() {
            super::hash(digest)?;
        }
        ensure!(
            self.outbound_frames.len() <= 16,
            "external exchange response exceeds its route frame bound"
        );
        let mut bytes = 0_usize;
        for frame in &self.outbound_frames {
            bytes = bytes
                .checked_add(frame.decode_frame()?.len())
                .ok_or_else(|| anyhow::anyhow!("external response byte count overflow"))?;
        }
        ensure!(
            bytes <= 1024 * 1024,
            "external exchange response exceeds its route byte bound"
        );
        if let Some(frame) = self.urgent_revocation_frame.as_ref() {
            ensure!(
                frame.decode_frame()?.len() <= super::TERMINAL_CONTROL_BYTES as usize,
                "external urgent response exceeds its terminal-control bound"
            );
        }
        Ok(())
    }
}

fn bounded_text(value: &str, maximum: usize, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum,
        "{label} is invalid"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_contract_is_closed_canonical_and_bounded() {
        let request = ExternalChannelExchangeRequest::from_frame(
            "T-placement",
            "occurrence-one",
            b"signed-frame",
        )
        .unwrap();
        assert_eq!(request.decode_frame().unwrap(), b"signed-frame");
        let mut value = serde_json::to_value(&request).unwrap();
        value["ambient_url"] = serde_json::json!("https://wrong.invalid");
        assert!(serde_json::from_value::<ExternalChannelExchangeRequest>(value).is_err());

        let frame = ExternalChannelResponseFrame::new(
            1,
            lillux::sha256_hex(b"signed-frame"),
            b"signed-frame",
        )
        .unwrap();
        assert_eq!(frame.decode_frame().unwrap(), b"signed-frame");
        let invalid = ExternalChannelResponseFrame {
            frame_base64: "not canonical".into(),
            ..frame
        };
        assert!(invalid.decode_frame().is_err());
    }
}
