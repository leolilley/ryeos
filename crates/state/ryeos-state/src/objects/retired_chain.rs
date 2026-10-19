//! Retained prefix of a retired chain, not a current head or an absence proof.
//!
//! Only the retirement owner binds this object into an immutable fence after
//! lifecycle qualification. A shape check or CAS hash alone supplies no
//! provenance or retirement permission. The objects carry coordinates only:
//! which counterpart chains were involved and which events of which chains
//! were retained as evidence. What the events mean is decided by the caller's
//! evidence validator when the manifest is stored.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const RETIRED_CHAIN_MANIFEST_KIND: &str = "retired_chain_manifest";
pub const RETIRED_CHAIN_CLOSURE_PAGE_KIND: &str = "retired_chain_closure_page";
pub const MAX_RETIRED_CHAIN_CLOSURE_PAGE_BYTES: u64 = 512 * 1024;
/// Counterparts plus evidence coordinates one closure page may carry.
pub const MAX_RETIRED_CHAIN_CLOSURE_PAGE_ENTRIES: usize = 128;
const MANIFEST_SCHEMA: u32 = 1;
const PAGE_SCHEMA: u32 = 1;

/// Chain kinds that retire through a fence, supplied by the caller. State
/// treats the names as opaque: it never interprets what a kind means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FencedChainKinds(BTreeSet<String>);

impl FencedChainKinds {
    pub fn new(kinds: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        let kinds: BTreeSet<String> = kinds.into_iter().collect();
        for kind in &kinds {
            anyhow::ensure!(
                !kind.is_empty() && kind.trim() == kind,
                "fenced chain kind name is empty or padded"
            );
        }
        Ok(Self(kinds))
    }

    pub fn contains(&self, kind: &str) -> bool {
        self.0.contains(kind)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

fn canonical_hash(hash: &str) -> bool {
    lillux::cas::valid_hash(hash) && !hash.bytes().any(|byte| byte.is_ascii_uppercase())
}

/// Position of one event in a chain's native sequence domains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefixTip {
    pub event_hash: String,
    pub thread_seq: u64,
    pub chain_seq: u64,
}

impl PrefixTip {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            canonical_hash(&self.event_hash)
                && self.thread_seq > 0
                && self.chain_seq >= self.thread_seq,
            "retained prefix tip is invalid"
        );
        Ok(())
    }
}

/// One retained event of the owner or a counterpart, with both chains' prefix
/// tips at retirement. The event hash is the evidence; the tips bound it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceCoordinate {
    pub counterpart_root: String,
    pub event_hash: String,
    pub chain_seq: u64,
    pub owner_tip: PrefixTip,
    pub counterpart_tip: PrefixTip,
}

impl EvidenceCoordinate {
    fn validate(&self) -> anyhow::Result<()> {
        crate::recovery::validate_chain_root_id(&self.counterpart_root)?;
        anyhow::ensure!(
            canonical_hash(&self.event_hash) && self.chain_seq > 0,
            "retained evidence coordinate is invalid"
        );
        self.owner_tip.validate()?;
        self.counterpart_tip.validate()
    }

    /// Strict ordering key shared by every page of one closure.
    pub(crate) fn order_key(&self) -> (&str, &str) {
        (&self.counterpart_root, &self.event_hash)
    }
}

/// Bounded continuation under one immutable retirement manifest owner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredChainClosurePage {
    pub schema: u32,
    pub kind: String,
    pub chain_root_id: String,
    pub counterpart_roots: Vec<String>,
    pub retained_evidence: Vec<EvidenceCoordinate>,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub next_page_hash: Option<String>,
}

impl RetiredChainClosurePage {
    pub(crate) fn new(owner: &str, next_page_hash: Option<String>) -> Self {
        Self {
            schema: PAGE_SCHEMA,
            kind: RETIRED_CHAIN_CLOSURE_PAGE_KIND.to_string(),
            chain_root_id: owner.to_string(),
            counterpart_roots: Vec::new(),
            retained_evidence: Vec::new(),
            next_page_hash,
        }
    }

    pub fn from_value(value: &serde_json::Value) -> anyhow::Result<Self> {
        let page: Self = serde_json::from_value(value.clone())?;
        page.validate()?;
        Ok(page)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == PAGE_SCHEMA && self.kind == RETIRED_CHAIN_CLOSURE_PAGE_KIND,
            "unsupported retired chain closure page"
        );
        crate::recovery::validate_chain_root_id(&self.chain_root_id)?;
        anyhow::ensure!(
            !self.counterpart_roots.is_empty() || !self.retained_evidence.is_empty(),
            "empty retired chain closure page"
        );
        anyhow::ensure!(
            self.counterpart_roots.len() + self.retained_evidence.len()
                <= MAX_RETIRED_CHAIN_CLOSURE_PAGE_ENTRIES,
            "retired chain closure page exceeds its entry ceiling"
        );
        if let Some(hash) = &self.next_page_hash {
            anyhow::ensure!(
                canonical_hash(hash),
                "invalid retired chain continuation hash"
            );
        }
        let mut prior: Option<&str> = None;
        for root in &self.counterpart_roots {
            crate::recovery::validate_chain_root_id(root)?;
            anyhow::ensure!(
                root != &self.chain_root_id && prior.is_none_or(|value| value < root.as_str()),
                "retired chain counterparts duplicated, unordered or include the owner"
            );
            prior = Some(root);
        }
        let mut prior: Option<(&str, &str)> = None;
        for coordinate in &self.retained_evidence {
            coordinate.validate()?;
            anyhow::ensure!(
                coordinate.counterpart_root != self.chain_root_id,
                "retained evidence names the owner as its own counterpart"
            );
            let key = coordinate.order_key();
            anyhow::ensure!(
                prior.is_none_or(|value| value < key),
                "retained evidence duplicated or unordered"
            );
            prior = Some(key);
        }
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() as u64 <= MAX_RETIRED_CHAIN_CLOSURE_PAGE_BYTES,
            "retired chain closure page exceeds byte ceiling"
        );
        Ok(())
    }
}

/// Immutable record of what a retired chain retained. The fence, not this data
/// shape, authenticates its provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredChainManifest {
    pub schema: u32,
    pub kind: String,
    pub chain_root_id: String,
    /// Fence provenance only; deliberately not a retained full-head link.
    pub retired_head_hash: String,
    pub snapshot_hash: String,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub terminal_event_hash: Option<String>,
    pub terminal_thread_seq: u64,
    pub terminal_chain_seq: u64,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub closure_page_head: Option<String>,
}

impl RetiredChainManifest {
    pub(crate) fn new(
        chain_root_id: String,
        retired_head_hash: String,
        snapshot_hash: String,
        terminal_event_hash: Option<String>,
        terminal_thread_seq: u64,
        terminal_chain_seq: u64,
        closure_page_head: Option<String>,
    ) -> Self {
        Self {
            schema: MANIFEST_SCHEMA,
            kind: RETIRED_CHAIN_MANIFEST_KIND.to_string(),
            chain_root_id,
            retired_head_hash,
            snapshot_hash,
            terminal_event_hash,
            terminal_thread_seq,
            terminal_chain_seq,
            closure_page_head,
        }
    }

    pub fn from_value(value: &serde_json::Value) -> anyhow::Result<Self> {
        let manifest: Self = serde_json::from_value(value.clone())?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == MANIFEST_SCHEMA && self.kind == RETIRED_CHAIN_MANIFEST_KIND,
            "retired chain manifest has unsupported schema or kind"
        );
        crate::recovery::validate_chain_root_id(&self.chain_root_id)?;
        for hash in [&self.retired_head_hash, &self.snapshot_hash]
            .into_iter()
            .chain(self.terminal_event_hash.iter())
            .chain(self.closure_page_head.iter())
        {
            anyhow::ensure!(
                canonical_hash(hash),
                "retired chain manifest has a noncanonical hash"
            );
        }
        anyhow::ensure!(
            self.terminal_event_hash.is_some() == (self.terminal_thread_seq > 0)
                && self.terminal_chain_seq >= self.terminal_thread_seq
                && (self.terminal_thread_seq > 0 || self.terminal_chain_seq == 0),
            "retired chain manifest has inconsistent native positions"
        );
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() <= super::MAX_THREAD_EVENT_SERIALIZED_BYTES,
            "retired chain manifest exceeds the object byte ceiling"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tip(byte: &str, thread_seq: u64, chain_seq: u64) -> PrefixTip {
        PrefixTip {
            event_hash: byte.repeat(32),
            thread_seq,
            chain_seq,
        }
    }

    fn coordinate(root: &str, hash: &str) -> EvidenceCoordinate {
        EvidenceCoordinate {
            counterpart_root: root.to_string(),
            event_hash: hash.repeat(32),
            chain_seq: 3,
            owner_tip: tip("ee", 2, 3),
            counterpart_tip: tip("ff", 2, 5),
        }
    }

    fn manifest() -> RetiredChainManifest {
        RetiredChainManifest::new(
            "T-owner".into(),
            "a".repeat(64),
            "b".repeat(64),
            Some("c".repeat(64)),
            4,
            7,
            Some("d".repeat(64)),
        )
    }

    #[test]
    fn fenced_kinds_are_an_opaque_validated_set() {
        let kinds = FencedChainKinds::new(["alpha".to_string(), "beta".to_string()]).unwrap();
        assert!(kinds.contains("alpha") && !kinds.contains("gamma"));
        assert_eq!(kinds.iter().collect::<Vec<_>>(), ["alpha", "beta"]);
        assert!(FencedChainKinds::default().is_empty());
        assert!(FencedChainKinds::new([String::new()]).is_err());
        assert!(FencedChainKinds::new([" padded".to_string()]).is_err());
    }

    #[test]
    fn manifest_round_trips_closed_and_keeps_native_sequence_domains() {
        let valid = manifest();
        valid.validate().unwrap();
        let value = serde_json::to_value(&valid).unwrap();
        assert_eq!(RetiredChainManifest::from_value(&value).unwrap(), valid);
        for field in ["terminal_event_hash", "closure_page_head"] {
            let mut value = value.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(RetiredChainManifest::from_value(&value).is_err());
        }
        let mut extra = value.clone();
        extra["full_head_authority"] = serde_json::json!(true);
        assert!(RetiredChainManifest::from_value(&extra).is_err());
        let mut schema = value;
        schema["schema"] = serde_json::json!(2);
        assert!(RetiredChainManifest::from_value(&schema).is_err());
        for (thread, chain, hash) in [(4, 3, Some("c".repeat(64))), (0, 7, None), (1, 1, None)] {
            let mut changed = valid.clone();
            changed.terminal_thread_seq = thread;
            changed.terminal_chain_seq = chain;
            changed.terminal_event_hash = hash;
            assert!(changed.validate().is_err());
        }
        let mut upper = valid;
        upper.snapshot_hash = "B".repeat(64);
        assert!(upper.validate().is_err());
    }

    #[test]
    fn closure_page_round_trips_coordinate_only_content() {
        let mut page = RetiredChainClosurePage::new("T-owner", Some("9".repeat(64)));
        page.counterpart_roots = vec!["T-a".into(), "T-b".into()];
        page.retained_evidence = vec![coordinate("T-a", "11"), coordinate("T-a", "22")];
        let value = serde_json::to_value(&page).unwrap();
        assert_eq!(RetiredChainClosurePage::from_value(&value).unwrap(), page);
        let text = value.to_string();
        for forbidden in ["seat", "transfer", "presentation", "phase", "payload"] {
            assert!(!text.contains(forbidden), "page leaks `{forbidden}`");
        }
        let mut leaked = value;
        leaked["retained_evidence"][0]["decision"] = serde_json::json!({});
        assert!(RetiredChainClosurePage::from_value(&leaked).is_err());
    }

    #[test]
    fn closure_page_refuses_unordered_duplicate_owner_and_oversized_shapes() {
        let page = |roots: &[&str], evidence: Vec<EvidenceCoordinate>| {
            let mut page = RetiredChainClosurePage::new("T-owner", None);
            page.counterpart_roots = roots.iter().map(|root| root.to_string()).collect();
            page.retained_evidence = evidence;
            page
        };
        for roots in [
            vec!["T-z", "T-a"],
            vec!["T-a", "T-a"],
            vec!["T-owner"],
            vec!["../other"],
        ] {
            assert!(page(&roots, Vec::new()).validate().is_err());
        }
        assert!(page(&[], Vec::new()).validate().is_err());
        assert!(
            page(
                &["T-a"],
                vec![coordinate("T-a", "22"), coordinate("T-a", "11")]
            )
            .validate()
            .is_err()
        );
        assert!(
            page(&["T-a"], vec![coordinate("T-owner", "11")])
                .validate()
                .is_err()
        );
        let mut backwards = coordinate("T-a", "11");
        backwards.owner_tip = tip("ee", 3, 2);
        assert!(page(&["T-a"], vec![backwards]).validate().is_err());
        let many: Vec<String> = (0..=MAX_RETIRED_CHAIN_CLOSURE_PAGE_ENTRIES)
            .map(|index| format!("T-{index:04}"))
            .collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert!(page(&many, Vec::new()).validate().is_err());
    }
}
