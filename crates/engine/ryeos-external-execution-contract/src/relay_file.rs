//! One journal-claimed occurrence file contact. This wire shape is transport
//! evidence only; it cannot attach, acknowledge, or settle a guest channel.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::canonical_json;

pub const MAX_RELAY_FILE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_RELAY_REQUEST_BYTES: usize = 16 * 1024;
// Base64 of the largest signed file plus JSON metadata and one newline.
pub const MAX_RELAY_RESPONSE_BYTES: usize = 3 * 1024 * 1024;
pub const LIFECYCLE_RELAY_UPLOAD_FD_ENV: &str = "RYEOS_LIFECYCLE_RELAY_UPLOAD_FD";
pub const RELAY_FILE_PROTOCOL: &str = "ryeos.external-execution.relay-file.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayFileKind {
    Attach,
    Exchange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayFileDirection {
    GuestToController,
    ControllerToGuest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayFileEffect {
    UploadOnce,
    Download,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayFileAdapterRequest {
    pub schema: u32,
    pub protocol: String,
    pub binding_hash: String,
    pub settings_digest: String,
    pub placement_thread_id: String,
    pub occurrence_id: String,
    pub allocation_request_digest: String,
    pub bootstrap_digest: String,
    pub mailbox_root: String,
    pub kind: RelayFileKind,
    pub sequence: u32,
    pub direction: RelayFileDirection,
    pub remote_path: String,
    pub maximum_file_bytes: u64,
    pub deadline_ms: i64,
    pub effect: RelayFileEffect,
    pub expected_sha256: Option<String>,
    pub expected_bytes: Option<u64>,
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl RelayFileAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.protocol == RELAY_FILE_PROTOCOL,
            "unsupported relay file request"
        );
        ensure!(
            [
                &self.binding_hash,
                &self.settings_digest,
                &self.allocation_request_digest,
                &self.bootstrap_digest,
            ]
            .into_iter()
            .all(|value| digest(value)),
            "relay file identity is invalid"
        );
        ensure!(
            !self.placement_thread_id.is_empty() && self.placement_thread_id.len() <= 256,
            "relay placement identity is invalid"
        );
        ensure!(
            self.occurrence_id.starts_with("sbx-")
                && self.occurrence_id.len() <= 128
                && self
                    .occurrence_id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "relay occurrence identity is invalid"
        );
        ensure!(
            (self.kind == RelayFileKind::Attach && self.sequence == 0)
                || (self.kind == RelayFileKind::Exchange && (1..=65_536).contains(&self.sequence)),
            "relay sequence is invalid"
        );
        let name = match (self.kind, self.direction) {
            (RelayFileKind::Attach, RelayFileDirection::GuestToController) => {
                "attach.request.json".to_owned()
            }
            (RelayFileKind::Attach, RelayFileDirection::ControllerToGuest) => {
                "attach.response.json".to_owned()
            }
            (RelayFileKind::Exchange, RelayFileDirection::GuestToController) => {
                format!("exchange-{:05}.request.json", self.sequence)
            }
            (RelayFileKind::Exchange, RelayFileDirection::ControllerToGuest) => {
                format!("exchange-{:05}.response.json", self.sequence)
            }
        };
        ensure!(
            self.mailbox_root.starts_with('/')
                && !self.mailbox_root.starts_with("//")
                && self.mailbox_root.len() <= 2048
                && self
                    .mailbox_root
                    .split('/')
                    .skip(1)
                    .all(|part| !part.is_empty()
                        && part != "."
                        && part != ".."
                        && part
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
                && self.remote_path == format!("{}/{name}", self.mailbox_root)
                && self.remote_path.len() <= 2048,
            "relay path changed its exact mailbox slot"
        );
        ensure!(
            (1..=MAX_RELAY_FILE_BYTES as u64).contains(&self.maximum_file_bytes)
                && self.deadline_ms > 0,
            "relay signed file bound or deadline is invalid"
        );
        ensure!(
            self.expected_sha256.is_some() == self.expected_bytes.is_some()
                && self.expected_sha256.is_some()
                    == (self.direction == RelayFileDirection::ControllerToGuest),
            "relay directional byte identity is invalid"
        );
        if let (Some(hash), Some(bytes)) = (&self.expected_sha256, self.expected_bytes) {
            ensure!(
                digest(hash) && (1..=self.maximum_file_bytes).contains(&bytes),
                "relay committed upload identity is invalid"
            );
        }
        ensure!(
            self.effect != RelayFileEffect::UploadOnce
                || self.direction == RelayFileDirection::ControllerToGuest,
            "relay guest request cannot upload from controller"
        );
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = canonical_json(self)?;
        ensure!(
            bytes.len() <= MAX_RELAY_REQUEST_BYTES,
            "relay request exceeds bound"
        );
        Ok(bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelayFileAdapterResponse {
    UploadTransportObserved {
        request_digest: String,
    },
    Downloaded {
        request_digest: String,
        bytes_base64: String,
        sha256: String,
        bytes: u64,
    },
}

impl RelayFileAdapterResponse {
    pub fn validate_for(
        &self,
        request: &RelayFileAdapterRequest,
        request_digest: &str,
    ) -> Result<()> {
        request.validate()?;
        ensure!(digest(request_digest), "relay request digest is invalid");
        match (request.effect, self) {
            (
                RelayFileEffect::UploadOnce,
                Self::UploadTransportObserved {
                    request_digest: actual,
                },
            ) => ensure!(
                actual == request_digest,
                "relay upload changed request identity"
            ),
            (
                RelayFileEffect::Download,
                Self::Downloaded {
                    request_digest: actual,
                    bytes_base64,
                    sha256,
                    bytes,
                },
            ) => {
                use base64::Engine as _;
                let raw = base64::engine::general_purpose::STANDARD.decode(bytes_base64)?;
                ensure!(
                    actual == request_digest
                        && *bytes == raw.len() as u64
                        && (1..=request.maximum_file_bytes).contains(bytes)
                        && digest(sha256)
                        && sha256 == &hex::encode(Sha256::digest(&raw))
                        && base64::engine::general_purpose::STANDARD.encode(&raw) == *bytes_base64,
                    "relay download changed bounded transport bytes"
                );
            }
            _ => anyhow::bail!("relay effect response has wrong outcome"),
        }
        Ok(())
    }
}
