//! Closed UI transfer testimony on existing source/destination seat chains.
//! References retain exact ThreadEvents, never grants or emitted effects.
use crate::payload_digest::seat_payload_digest;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Entire testimony family, including lifecycle versions. Unknown members
/// must not be treated as unrelated seat activity by historical readers.
pub const TRANSFER_FAMILY_PREFIX: &str = "contract.ryeos_ui.presentation_transfer";
pub const TRANSFER_EVENT: &str = "contract.ryeos_ui.presentation_transfer.v1";
pub const TRANSFER_SCHEMA: u32 = 1;

pub fn is_transfer_testimony(event_type: &str) -> bool {
    event_type.starts_with(TRANSFER_FAMILY_PREFIX)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferSeat {
    pub presentation_id: String,
    pub thread_id: String,
    pub chain_root_id: String,
    pub producer_incarnation: String,
    /// Acknowledged composition revision before reservation.
    pub revision: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferIdentity {
    pub operation_id: String,
    pub source: TransferSeat,
    pub destination: TransferSeat,
    /// Exact exported data commitment, NOT a CAS object edge.
    pub export_digest: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferEvidence {
    pub event_hash: String,
    pub chain_seq: u64,
}
/// Exact source-seat ordinary producer events establishing the retained MOVE
/// obligation. These links are data only; the reader must authenticate both
/// events, their producer receipts and their order before admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveFreezeEvidence {
    pub preparation: TransferEvidence,
    pub completion: TransferEvidence,
}

fn required_optional_freeze<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<MoveFreezeEvidence>, D::Error> {
    Option::<MoveFreezeEvidence>::deserialize(deserializer)
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransferPhase {
    /// Destination-owned, signed visibility of a possible inbound transfer.
    /// A lone commitment after a crash is inert: it reserves and imports nothing.
    InboundCommitment,
    /// Destination lifecycle cancellation of a destination-only crash orphan.
    /// This is data-only testimony, not browser admission or source abort.
    OrphanCancelled {
        inbound: TransferEvidence,
    },
    Prepared {
        /// Exact destination event published before this source preparation.
        inbound: TransferEvidence,
        /// Required null for COPY, exact ordered source-seat links for MOVE.
        /// A pair of hashes alone is never proof of receipt or authority.
        #[serde(deserialize_with = "required_optional_freeze")]
        freeze: Option<MoveFreezeEvidence>,
        /// Data only. Client export decoder and daemon admission independently
        /// validate this value. Never execute or restore authority from it.
        export: Value,
    },
    Accepted {
        preparation: TransferEvidence,
        /// Data-only destination reservation/allocation description. Runtime
        /// admission independently validates it before recording acceptance.
        reservation: Value,
        /// Destination-local admitted import/allocation commitment.
        import_digest: String,
    },
    Committed {
        preparation: TransferEvidence,
        acceptance: TransferEvidence,
        import_digest: String,
    },
    Aborted {
        preparation: TransferEvidence,
    },
    Activated {
        /// Destination publication depends on the ONE source-seat commit.
        commit: TransferEvidence,
        import_digest: String,
    },
    Retained {
        /// Historical data-only settlement of the ONE source-seat commit.
        /// This does not publish or restore a live destination attachment,
        /// mount, membership, grant or execution authority.
        commit: TransferEvidence,
        import_digest: String,
    },
    Released {
        /// Exact terminal source abort permitting destination reservation release.
        abort: TransferEvidence,
        /// Exact destination acceptance whose reservation is released.
        acceptance: TransferEvidence,
        import_digest: String,
    },
    SettlementObserved {
        /// Source-owned link to the exact destination Activated, Retained or
        /// Released event. This is not another source decision or, by itself,
        /// authorization to retire either seat.
        settlement: TransferEvidence,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferRecord {
    pub schema: u32,
    pub transfer: TransferIdentity,
    pub phase: TransferPhase,
}
/// Existing producer-event wrapper, not a second journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferEventPayload {
    pub seq: u64,
    pub operation_id: String,
    pub payload: TransferRecord,
}

impl TransferEventPayload {
    /// Pure equality with an independently authenticated seat frontier. This
    /// is a non-advancing witness, not a receipt or producer authorization.
    /// The immutable origin revision/incarnation apply only at preparation;
    /// later phases may follow edits or a new admitted producer.
    pub fn validate_current_witness(
        &self,
        current_next_engine_seq: u64,
        current_revision: u64,
        current_presentation_id: &str,
        current_producer_incarnation: &str,
    ) -> Result<(), String> {
        let owner = self.payload.event_owner();
        let preparation = matches!(
            &self.payload.phase,
            TransferPhase::InboundCommitment | TransferPhase::Prepared { .. }
        );
        if self.seq != current_next_engine_seq
            || owner.presentation_id != current_presentation_id
            || (preparation
                && (owner.revision != current_revision
                    || owner.producer_incarnation != current_producer_incarnation))
        {
            return Err("transfer witness differs from authenticated producer frontier".into());
        }
        Ok(())
    }
}

fn canonical_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}
fn seat_valid(seat: &TransferSeat) -> bool {
    canonical_uuid(&seat.presentation_id)
        && canonical_hash(&seat.producer_incarnation)
        && [&seat.thread_id, &seat.chain_root_id]
            .into_iter()
            .all(|id| !id.is_empty() && id.trim() == id && !id.chars().any(char::is_control))
}
impl TransferRecord {
    pub fn validate_shape(&self) -> Result<(), String> {
        if self.schema != TRANSFER_SCHEMA
            || !canonical_uuid(&self.transfer.operation_id)
            || !seat_valid(&self.transfer.source)
            || !seat_valid(&self.transfer.destination)
            || self.transfer.source.presentation_id == self.transfer.destination.presentation_id
            || self.transfer.source.thread_id == self.transfer.destination.thread_id
            || self.transfer.source.chain_root_id == self.transfer.destination.chain_root_id
            || !canonical_hash(&self.transfer.export_digest)
        {
            return Err("transfer identity is not the exact current two-seat contract".into());
        }
        if self
            .evidence_links()
            .iter()
            .any(|link| !canonical_hash(&link.event_hash))
        {
            return Err("transfer evidence hash is not canonical".into());
        }
        match &self.phase {
            TransferPhase::Prepared { export, freeze, .. } => {
                if !export.is_object()
                    || seat_payload_digest(export).map_err(|e| e.to_string())?
                        != self.transfer.export_digest
                {
                    return Err("prepared export contradicts its exact commitment".into());
                }
                match (export.get("mode").and_then(Value::as_str), freeze) {
                    (Some("copy"), None) => {}
                    (Some("move"), Some(links))
                        if links.preparation.chain_seq < links.completion.chain_seq
                            && links.preparation.event_hash != links.completion.event_hash => {}
                    _ => {
                        return Err(
                            "prepared freeze links differ from exact export mode or source order"
                                .into(),
                        );
                    }
                }
            }
            TransferPhase::Accepted {
                reservation,
                import_digest,
                ..
            } if !reservation.is_object()
                || seat_payload_digest(reservation).map_err(|e| e.to_string())?
                    != *import_digest =>
            {
                return Err("accepted reservation contradicts its exact import commitment".into());
            }
            TransferPhase::Committed { import_digest, .. }
            | TransferPhase::Activated { import_digest, .. }
            | TransferPhase::Retained { import_digest, .. }
            | TransferPhase::Released { import_digest, .. }
                if !canonical_hash(import_digest) =>
            {
                return Err("transfer import commitment is not canonical".into());
            }
            TransferPhase::Committed {
                preparation,
                acceptance,
                ..
            } if preparation.event_hash == acceptance.event_hash => {
                return Err("preparation and acceptance must be distinct evidence".into());
            }
            TransferPhase::Released {
                abort, acceptance, ..
            } if abort.event_hash == acceptance.event_hash => {
                return Err("abort and acceptance must be distinct evidence".into());
            }
            _ => {}
        }
        Ok(())
    }
    /// Supplied by the selected existing seat policy, not a new quota owner.
    pub fn validate_bounds(&self, max_record_bytes: u64) -> Result<(), String> {
        self.validate_shape()?;
        let bytes = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        if max_record_bytes == 0 || bytes.len() as u64 > max_record_bytes {
            return Err("transfer record exceeds its selected seat bound".into());
        }
        Ok(())
    }
    pub fn event_owner(&self) -> &TransferSeat {
        match &self.phase {
            TransferPhase::InboundCommitment
            | TransferPhase::OrphanCancelled { .. }
            | TransferPhase::Accepted { .. }
            | TransferPhase::Activated { .. }
            | TransferPhase::Retained { .. }
            | TransferPhase::Released { .. } => &self.transfer.destination,
            _ => &self.transfer.source,
        }
    }
    /// Transfer-phase dependencies only. Ordinary MOVE freeze events are
    /// deliberately excluded from transfer-only lifecycle scheduling.
    pub fn phase_evidence_links(&self) -> Vec<&TransferEvidence> {
        match &self.phase {
            TransferPhase::InboundCommitment => vec![],
            TransferPhase::OrphanCancelled { inbound } => vec![inbound],
            TransferPhase::Prepared { inbound, .. } => vec![inbound],
            TransferPhase::Accepted { preparation, .. }
            | TransferPhase::Aborted { preparation } => vec![preparation],
            TransferPhase::Committed {
                preparation,
                acceptance,
                ..
            } => vec![preparation, acceptance],
            TransferPhase::Activated { commit, .. } | TransferPhase::Retained { commit, .. } => {
                vec![commit]
            }
            TransferPhase::Released {
                abort, acceptance, ..
            } => vec![abort, acceptance],
            TransferPhase::SettlementObserved { settlement } => vec![settlement],
        }
    }

    /// Ordinary source-seat producer events retained by MOVE preparation.
    /// These require separate receipt and event-position verification.
    pub fn freeze_event_links(&self) -> Vec<&TransferEvidence> {
        match &self.phase {
            TransferPhase::Prepared {
                freeze: Some(links),
                ..
            } => {
                vec![&links.preparation, &links.completion]
            }
            _ => vec![],
        }
    }

    /// Every typed retained edge, including ordinary freeze evidence for GC
    /// retention and canonical hash validation. Not a transfer-only graph.
    pub fn evidence_links(&self) -> Vec<&TransferEvidence> {
        let mut links = self.phase_evidence_links();
        links.extend(self.freeze_event_links());
        links
    }
}

/// State validates its ordinary ThreadEvent envelope first. Domain registration
/// also checks exact phase-owner placement and the indexed producer wrapper.
/// This is not proof that referenced testimony was authorized or settled. In
/// particular, accepting a settlement shape does not authorize a browser to
/// write it or provide the lifecycle writer's separate attribution and fence.
pub fn from_event_value(event: &Value) -> Result<TransferEventPayload, String> {
    if event.get("event_type").and_then(Value::as_str) != Some(TRANSFER_EVENT) {
        return Err("not a current presentation transfer event".into());
    }
    let wrapper: TransferEventPayload = serde_json::from_value(
        event
            .get("payload")
            .cloned()
            .ok_or("transfer event omitted its payload")?,
    )
    .map_err(|e| format!("invalid typed transfer payload: {e}"))?;
    wrapper.payload.validate_shape()?;
    let owner = wrapper.payload.event_owner();
    if wrapper.operation_id != wrapper.payload.transfer.operation_id
        || event.get("thread_id").and_then(Value::as_str) != Some(owner.thread_id.as_str())
        || event.get("chain_root_id").and_then(Value::as_str) != Some(owner.chain_root_id.as_str())
    {
        return Err("transfer phase is not recorded on its exact owning seat".into());
    }
    Ok(wrapper)
}
