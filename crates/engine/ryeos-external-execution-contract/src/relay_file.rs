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

/// An adapter-issued occurrence id is opaque, but the relay uses it as one
/// path component under the guest occurrences root. Admit only lowercase
/// ASCII letters, digits and interior hyphens, at most 128 bytes, so it can
/// never name a different directory or hide behind a leading or trailing
/// separator-like character.
pub fn require_relay_occurrence_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && !id.starts_with('-')
            && !id.ends_with('-')
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "relay occurrence identity is invalid"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayFileKind {
    Attach,
    Exchange,
    /// The guest owner's typed report of how its released supervisor ended.
    /// It is written once by the owner and flows only guest to controller.
    OwnerReport,
}

impl RelayFileKind {
    /// The sole mailbox filename for this slot, or none when the kind does
    /// not flow in that direction.
    pub fn file_name(self, direction: RelayFileDirection, sequence: u32) -> Option<String> {
        use RelayFileDirection::{ControllerToGuest, GuestToController};
        Some(match (self, direction) {
            (Self::Attach, GuestToController) => "attach.request.json".to_owned(),
            (Self::Attach, ControllerToGuest) => "attach.response.json".to_owned(),
            (Self::Exchange, GuestToController) => format!("exchange-{sequence:05}.request.json"),
            (Self::Exchange, ControllerToGuest) => format!("exchange-{sequence:05}.response.json"),
            (Self::OwnerReport, GuestToController) => OWNER_REPORT_FILE_NAME.to_owned(),
            (Self::OwnerReport, ControllerToGuest) => return None,
        })
    }

    /// Whether `sequence` is a valid slot number for this kind.
    pub fn accepts_sequence(self, sequence: u32) -> bool {
        match self {
            Self::Attach | Self::OwnerReport => sequence == 0,
            Self::Exchange => (1..=65_536).contains(&sequence),
        }
    }
}

/// The mailbox filename of the guest owner's report.
pub const OWNER_REPORT_FILE_NAME: &str = "owner-report.json";

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

/// The account that must own a delivered mailbox file so the guest's
/// supervisor can read it. Same wire shape as the guest-owner profile account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "implementation", rename_all = "snake_case", deny_unknown_fields)]
pub enum RelayDeliveryAccount {
    Unix { uid: u32, gid: u32 },
}

impl RelayDeliveryAccount {
    fn validate(&self) -> Result<()> {
        let Self::Unix { uid, gid } = *self;
        ensure!(
            uid != 0 && gid != 0 && uid != u32::MAX && gid != u32::MAX,
            "relay delivery account must be a non-root account"
        );
        Ok(())
    }
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
    /// Owner of the delivered file: required for an upload, absent for a
    /// download.
    pub delivery_account: Option<RelayDeliveryAccount>,
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
            self.schema == 2 && self.protocol == RELAY_FILE_PROTOCOL,
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
            require_relay_occurrence_id(&self.occurrence_id).is_ok(),
            "relay occurrence identity is invalid"
        );
        ensure!(
            self.kind.accepts_sequence(self.sequence),
            "relay sequence is invalid"
        );
        let name = self
            .kind
            .file_name(self.direction, self.sequence)
            .ok_or_else(|| anyhow::anyhow!("relay kind does not flow in this direction"))?;
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
        ensure!(
            self.delivery_account.is_some() == (self.effect == RelayFileEffect::UploadOnce),
            "relay delivery account must accompany exactly an upload"
        );
        if let Some(account) = &self.delivery_account {
            account.validate()?;
        }
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
    /// Exact HTTP 404 from this occurrence's signed file GET route. This is
    /// transport status only; it does not prove that the file or sandbox is
    /// absent. The raw provider status needs an installed missing-file probe.
    DownloadHttpNotFound {
        request_digest: String,
    },
    /// This read did not get an answer about the file: a lost connection or a
    /// provider status that says nothing about it. A download has no effect to
    /// repeat, so the caller may read again until its own deadline.
    DownloadUnavailable {
        request_digest: String,
        /// Short diagnostic code, never provider text.
        reason: String,
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
                Self::DownloadHttpNotFound {
                    request_digest: actual,
                },
            ) => ensure!(
                actual == request_digest,
                "relay file 404 changed request identity"
            ),
            (
                RelayFileEffect::Download,
                Self::DownloadUnavailable {
                    request_digest: actual,
                    reason,
                },
            ) => {
                ensure!(
                    actual == request_digest,
                    "relay file read changed request identity"
                );
                crate::diagnostic_reason(reason)?;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn upload() -> RelayFileAdapterRequest {
        let root = "/ryeos/occurrences/sbx-a/".to_owned() + &"a".repeat(64);
        RelayFileAdapterRequest {
            schema: 2,
            protocol: RELAY_FILE_PROTOCOL.into(),
            binding_hash: "a".repeat(64),
            settings_digest: "b".repeat(64),
            placement_thread_id: "T-1".into(),
            occurrence_id: "sbx-a".into(),
            allocation_request_digest: "c".repeat(64),
            bootstrap_digest: "d".repeat(64),
            remote_path: format!("{root}/attach.response.json"),
            mailbox_root: root,
            kind: RelayFileKind::Attach,
            sequence: 0,
            direction: RelayFileDirection::ControllerToGuest,
            maximum_file_bytes: 1024,
            deadline_ms: 1,
            effect: RelayFileEffect::UploadOnce,
            expected_sha256: Some("e".repeat(64)),
            expected_bytes: Some(10),
            delivery_account: Some(RelayDeliveryAccount::Unix {
                uid: 65534,
                gid: 65534,
            }),
        }
    }

    #[test]
    fn occurrence_id_is_one_safe_path_component_with_no_provider_prefix() {
        for ok in ["a", "occ-1", "0abc", "sbx-a", "x".repeat(128).as_str()] {
            require_relay_occurrence_id(ok).unwrap();
        }
        let too_long = "x".repeat(129);
        for bad in [
            "", "a/b", "..", "a.b", "Upper", "-a", "a-", "a b", "a\0b", "é", &too_long,
        ] {
            assert!(require_relay_occurrence_id(bad).is_err(), "{bad:?}");
        }
        let mut request = upload();
        request.occurrence_id = "occ-1".into();
        request.validate().unwrap();
        request.occurrence_id = "-occ".into();
        assert!(request.validate().is_err());
    }

    #[test]
    fn upload_requires_a_non_root_delivery_account() {
        upload().validate().unwrap();
        let mut missing = upload();
        missing.delivery_account = None;
        assert!(missing.validate().is_err());
        for (uid, gid) in [(0, 65534), (65534, 0)] {
            let mut root = upload();
            root.delivery_account = Some(RelayDeliveryAccount::Unix { uid, gid });
            assert!(root.validate().is_err());
        }
    }

    #[test]
    fn download_carries_no_delivery_account() {
        let mut download = upload();
        download.direction = RelayFileDirection::GuestToController;
        download.remote_path = format!("{}/attach.request.json", download.mailbox_root);
        download.expected_sha256 = None;
        download.expected_bytes = None;
        download.effect = RelayFileEffect::Download;
        assert!(download.validate().is_err());
        download.delivery_account = None;
        download.validate().unwrap();
    }

    #[test]
    fn only_a_download_may_be_read_again_and_its_reason_stays_a_token() {
        let digest = "d".repeat(64);
        let response = |reason: &str| RelayFileAdapterResponse::DownloadUnavailable {
            request_digest: digest.clone(),
            reason: reason.into(),
        };
        response("read_status:503")
            .validate_for(&upload(), &digest)
            .unwrap_err();
        let mut download = upload();
        download.direction = RelayFileDirection::GuestToController;
        download.remote_path = format!("{}/attach.request.json", download.mailbox_root);
        download.effect = RelayFileEffect::Download;
        download.delivery_account = None;
        download.expected_sha256 = None;
        download.expected_bytes = None;
        response("read_status:503")
            .validate_for(&download, &digest)
            .unwrap();
        response("provider said: secret")
            .validate_for(&download, &digest)
            .unwrap_err();
    }

    #[test]
    fn owner_report_is_a_guest_to_controller_download_only() {
        let mut report = upload();
        report.kind = RelayFileKind::OwnerReport;
        report.direction = RelayFileDirection::GuestToController;
        report.remote_path = format!("{}/owner-report.json", report.mailbox_root);
        report.expected_sha256 = None;
        report.expected_bytes = None;
        report.effect = RelayFileEffect::Download;
        report.delivery_account = None;
        report.validate().unwrap();
        let mut sequenced = report.clone();
        sequenced.sequence = 1;
        assert!(sequenced.validate().is_err());
        let mut named = report.clone();
        named.remote_path = format!("{}/attach.request.json", named.mailbox_root);
        assert!(named.validate().is_err());
        let mut inbound = report;
        inbound.direction = RelayFileDirection::ControllerToGuest;
        inbound.remote_path = format!("{}/owner-report.json", inbound.mailbox_root);
        inbound.expected_sha256 = Some("e".repeat(64));
        inbound.expected_bytes = Some(10);
        inbound.effect = RelayFileEffect::UploadOnce;
        inbound.delivery_account = Some(RelayDeliveryAccount::Unix {
            uid: 65534,
            gid: 65534,
        });
        assert!(inbound.validate().is_err());
    }

    #[test]
    fn older_request_schema_is_refused() {
        let mut old = upload();
        old.schema = 1;
        assert!(old.validate().is_err());
    }
}
