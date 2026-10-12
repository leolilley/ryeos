//! Retained terminal seat prefix, not a current head or absence proof.
//!
//! Only the existing retirement owner binds this object into an immutable
//! retired-identity marker after exhaustive lifecycle qualification. A shape
//! check or CAS hash alone supplies no provenance or retirement permission.

use ryeos_ui_presentation_contract::transfer::{TransferEvidence, TransferPhase, TransferRecord};
use serde::{Deserialize, Serialize};

/// Completed negative obligation under both exact prefixes. The immutable
/// retirement marker, not this data shape, authenticates its provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalTransferNegativeClosure {
    pub decision: TransferRecord,
    pub evidence: TransferEvidence,
    pub source_tip: TerminalTransferPrefixTip,
    pub destination_tip: TerminalTransferPrefixTip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalTransferPrefixTip {
    pub event_hash: String,
    pub thread_seq: u64,
    pub chain_seq: u64,
}

impl TerminalTransferNegativeClosure {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.decision.validate_shape().map_err(anyhow::Error::msg)?;
        for seat in [
            &self.decision.transfer.source,
            &self.decision.transfer.destination,
        ] {
            crate::recovery::validate_chain_root_id(&seat.chain_root_id)?;
            anyhow::ensure!(
                seat.thread_id == seat.chain_root_id,
                "negative retirement closure requires exact root seats"
            );
        }
        anyhow::ensure!(
            matches!(
                self.decision.phase,
                TransferPhase::Aborted { .. } | TransferPhase::OrphanCancelled { .. }
            ),
            "negative terminal closure requires a real abort or orphan decision"
        );
        anyhow::ensure!(
            lillux::cas::valid_hash(&self.evidence.event_hash)
                && !self
                    .evidence
                    .event_hash
                    .bytes()
                    .any(|byte| byte.is_ascii_uppercase())
                && self.evidence.chain_seq > 0,
            "negative closure evidence is invalid"
        );
        for tip in [&self.source_tip, &self.destination_tip] {
            anyhow::ensure!(
                lillux::cas::valid_hash(&tip.event_hash)
                    && !tip.event_hash.bytes().any(|byte| byte.is_ascii_uppercase())
                    && tip.thread_seq > 0
                    && tip.chain_seq >= tip.thread_seq,
                "negative terminal closure has invalid native prefix tip"
            );
        }
        let owner_tip = match self.decision.phase {
            TransferPhase::Aborted { .. } => &self.source_tip,
            TransferPhase::OrphanCancelled { .. } => &self.destination_tip,
            _ => unreachable!("negative phase vocabulary checked above"),
        };
        anyhow::ensure!(
            self.evidence.chain_seq <= owner_tip.chain_seq
                && (self.evidence.chain_seq != owner_tip.chain_seq
                    || self.evidence.event_hash == owner_tip.event_hash),
            "negative decision evidence exceeds or conflicts with its owning prefix tip"
        );
        Ok(())
    }
}

pub const TERMINAL_SEAT_RETIREMENT_KIND: &str = "terminal_seat_retirement";
pub const TERMINAL_SEAT_RETIREMENT_CLOSURE_PAGE_KIND: &str =
    "terminal_seat_retirement_closure_page";
pub const MAX_RETIREMENT_CLOSURE_PAGE_BYTES: u64 = 512 * 1024;

/// Bounded continuation under the same immutable retirement manifest owner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSeatRetirementClosurePage {
    pub schema: u32,
    pub kind: String,
    pub chain_root_id: String,
    pub counterpart_roots: Vec<String>,
    pub negative_transfer_closures: Vec<TerminalTransferNegativeClosure>,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub next_page_hash: Option<String>,
}

impl TerminalSeatRetirementClosurePage {
    pub fn from_value(value: &serde_json::Value) -> anyhow::Result<Self> {
        let page: Self = serde_json::from_value(value.clone())?;
        page.validate()?;
        Ok(page)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == 1 && self.kind == TERMINAL_SEAT_RETIREMENT_CLOSURE_PAGE_KIND,
            "unsupported retirement closure page"
        );
        crate::recovery::validate_chain_root_id(&self.chain_root_id)?;
        anyhow::ensure!(
            !self.counterpart_roots.is_empty() || !self.negative_transfer_closures.is_empty(),
            "empty retirement closure page"
        );
        if let Some(hash) = &self.next_page_hash {
            anyhow::ensure!(
                lillux::cas::valid_hash(hash)
                    && !hash.bytes().any(|byte| byte.is_ascii_uppercase()),
                "invalid retirement continuation hash"
            );
        }
        let mut prior: Option<&str> = None;
        for root in &self.counterpart_roots {
            crate::recovery::validate_chain_root_id(root)?;
            anyhow::ensure!(
                root != &self.chain_root_id && prior.is_none_or(|value| value < root.as_str()),
                "retirement counterparts duplicated or unordered"
            );
            prior = Some(root);
        }
        let mut prior: Option<&str> = None;
        for closure in &self.negative_transfer_closures {
            closure.validate()?;
            let transfer = &closure.decision.transfer;
            anyhow::ensure!(
                transfer.source.chain_root_id == self.chain_root_id
                    || transfer.destination.chain_root_id == self.chain_root_id,
                "negative closure does not touch page owner"
            );
            anyhow::ensure!(
                prior.is_none_or(|value| value < transfer.operation_id.as_str()),
                "negative closures duplicated or unordered"
            );
            prior = Some(&transfer.operation_id);
        }
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() as u64 <= MAX_RETIREMENT_CLOSURE_PAGE_BYTES,
            "retirement closure page exceeds byte ceiling"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSeatRetirement {
    pub schema: u32,
    pub kind: String,
    pub chain_root_id: String,
    /// Marker provenance only; deliberately not a retained full-head link.
    pub retired_head_hash: String,
    pub snapshot_hash: String,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub terminal_event_hash: Option<String>,
    pub terminal_thread_seq: u64,
    pub terminal_chain_seq: u64,
    /// Stable no-rebirth identities. These are not CAS hashes or phase absence.
    /// Exact sorted uniqueness makes conditional GC release deterministic.
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub closure_page_head: Option<String>,
}

impl TerminalSeatRetirement {
    pub fn from_value(value: &serde_json::Value) -> anyhow::Result<Self> {
        let manifest: Self = serde_json::from_value(value.clone())?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == 2 && self.kind == TERMINAL_SEAT_RETIREMENT_KIND,
            "terminal retirement manifest has unsupported schema/kind"
        );
        crate::recovery::validate_chain_root_id(&self.chain_root_id)?;
        for hash in [&self.retired_head_hash, &self.snapshot_hash]
            .into_iter()
            .chain(self.terminal_event_hash.iter())
        {
            anyhow::ensure!(
                lillux::cas::valid_hash(hash)
                    && !hash.bytes().any(|byte| byte.is_ascii_uppercase()),
                "terminal retirement manifest has a noncanonical hash"
            );
        }
        anyhow::ensure!(
            self.terminal_event_hash.is_some() == (self.terminal_thread_seq > 0)
                && self.terminal_chain_seq >= self.terminal_thread_seq
                && (self.terminal_thread_seq > 0 || self.terminal_chain_seq == 0),
            "terminal retirement manifest has inconsistent native positions"
        );
        if let Some(hash) = &self.closure_page_head {
            anyhow::ensure!(
                lillux::cas::valid_hash(hash)
                    && !hash.bytes().any(|byte| byte.is_ascii_uppercase()),
                "invalid retirement closure head"
            );
        }
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() <= super::MAX_THREAD_EVENT_SERIALIZED_BYTES,
            "terminal retirement manifest exceeds existing object byte ceiling"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_decision_shape_is_bounded_by_its_actual_actor_tip() {
        let decision: TransferRecord = serde_json::from_value(serde_json::json!({
            "schema":1,"transfer":{"operation_id":"11111111-1111-4111-8111-111111111111",
                "source":{"presentation_id":"22222222-2222-4222-8222-222222222222","thread_id":"T-source","chain_root_id":"T-source","producer_incarnation":"aa".repeat(32),"revision":0},
                "destination":{"presentation_id":"33333333-3333-4333-8333-333333333333","thread_id":"T-destination","chain_root_id":"T-destination","producer_incarnation":"bb".repeat(32),"revision":0},
                "export_digest":"cc".repeat(32)},
            "phase":{"kind":"aborted","preparation":{"event_hash":"dd".repeat(32),"chain_seq":1}}
        })).unwrap();
        let mut closure = TerminalTransferNegativeClosure {
            decision,
            evidence: TransferEvidence {
                event_hash: "ee".repeat(32),
                chain_seq: 3,
            },
            source_tip: TerminalTransferPrefixTip {
                event_hash: "ee".repeat(32),
                thread_seq: 2,
                chain_seq: 3,
            },
            destination_tip: TerminalTransferPrefixTip {
                event_hash: "ff".repeat(32),
                thread_seq: 2,
                chain_seq: 5,
            },
        };
        closure.validate().unwrap();
        closure.evidence.chain_seq = 4;
        assert!(closure.validate().is_err());
        closure.evidence.chain_seq = 3;
        closure.evidence.event_hash = "ab".repeat(32);
        assert!(closure.validate().is_err());
        closure.evidence.event_hash = closure.source_tip.event_hash.clone();
        closure.decision.phase = TransferPhase::OrphanCancelled {
            inbound: TransferEvidence {
                event_hash: "dd".repeat(32),
                chain_seq: 1,
            },
        };
        closure.validate().unwrap();
        closure.destination_tip.chain_seq = 2;
        assert!(closure.validate().is_err());
    }

    fn manifest() -> TerminalSeatRetirement {
        TerminalSeatRetirement {
            schema: 2,
            kind: TERMINAL_SEAT_RETIREMENT_KIND.into(),
            chain_root_id: "T-source".into(),
            retired_head_hash: "a".repeat(64),
            snapshot_hash: "b".repeat(64),
            terminal_event_hash: Some("c".repeat(64)),
            terminal_thread_seq: 4,
            terminal_chain_seq: 7,
            closure_page_head: Some("d".repeat(64)),
        }
    }

    #[test]
    fn terminal_prefix_shape_is_closed_and_keeps_native_sequence_domains() {
        let valid = manifest();
        valid.validate().unwrap();
        let mut value = serde_json::to_value(&valid).unwrap();
        value.as_object_mut().unwrap().remove("terminal_event_hash");
        assert!(TerminalSeatRetirement::from_value(&value).is_err());
        let mut value = serde_json::to_value(&valid).unwrap();
        value.as_object_mut().unwrap().remove("closure_page_head");
        assert!(TerminalSeatRetirement::from_value(&value).is_err());
        let mut value = serde_json::to_value(&valid).unwrap();
        value["schema"] = serde_json::json!(1);
        assert!(TerminalSeatRetirement::from_value(&value).is_err());
        let mut value = serde_json::to_value(&valid).unwrap();
        value["full_head_authority"] = serde_json::json!(true);
        assert!(TerminalSeatRetirement::from_value(&value).is_err());
        for (thread, chain, hash) in [(4, 3, Some("c".repeat(64))), (0, 7, None), (1, 1, None)] {
            let mut changed = valid.clone();
            changed.terminal_thread_seq = thread;
            changed.terminal_chain_seq = chain;
            changed.terminal_event_hash = hash;
            assert!(changed.validate().is_err());
        }
    }

    #[test]
    fn counterpart_set_cannot_reassociate_duplicate_or_include_its_owner() {
        for roots in [
            vec!["T-z", "T-a"],
            vec!["T-a", "T-a"],
            vec!["T-source"],
            vec!["../other"],
        ] {
            let changed = TerminalSeatRetirementClosurePage {
                schema: 1,
                kind: TERMINAL_SEAT_RETIREMENT_CLOSURE_PAGE_KIND.into(),
                chain_root_id: "T-source".into(),
                counterpart_roots: roots.into_iter().map(str::to_owned).collect(),
                negative_transfer_closures: Vec::new(),
                next_page_hash: None,
            };
            assert!(changed.validate().is_err());
        }
        let mut changed = manifest();
        changed.snapshot_hash = "B".repeat(64);
        assert!(changed.validate().is_err());
    }
}
