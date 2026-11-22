//! Storing and reading the retained closure of a retired chain.
//!
//! Storing builds bounded immutable pages and a manifest in CAS from
//! coordinates only. State checks that every coordinate names real events of
//! the right chains inside the stated prefix bounds, then hands each retained
//! event to the caller's validator, which alone knows what the events mean.
//! The manifest hash is what the retirement fence later binds.

use std::collections::BTreeSet;

use anyhow::{Context, Result};

use super::{CasMutationGuard, RecoveryStore, RetiredChainFence};
use crate::objects::thread_event::hash_event;
use crate::objects::{
    EvidenceCoordinate, MAX_RETIRED_CHAIN_CLOSURE_PAGE_BYTES,
    MAX_RETIRED_CHAIN_CLOSURE_PAGE_ENTRIES, MAX_THREAD_EVENT_SERIALIZED_BYTES, PrefixTip,
    RetiredChainClosurePage, RetiredChainManifest, ThreadEvent, ThreadSnapshot,
};

/// Judges one retained event on the caller's behalf. Called by State, with the
/// owner's chain lock held, for each retained evidence event before the
/// manifest is stored. An error refuses the whole retirement closure.
pub type EvidenceValidator<'a> = &'a dyn Fn(&ThreadEvent) -> Result<()>;

/// Everything a retirement closure records, before it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredChainClosureDraft {
    pub chain_root_id: String,
    pub retired_head_hash: String,
    pub snapshot_hash: String,
    pub terminal_event_hash: Option<String>,
    pub terminal_thread_seq: u64,
    pub terminal_chain_seq: u64,
    /// Counterparts the caller discovered. Evidence obligation holders are
    /// added when the closure is stored.
    pub counterpart_roots: BTreeSet<String>,
    pub retained_evidence: Vec<EvidenceCoordinate>,
}

/// A stored closure read back in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredChainClosure {
    pub manifest: RetiredChainManifest,
    pub counterpart_roots: BTreeSet<String>,
    pub retained_evidence: Vec<EvidenceCoordinate>,
}

fn load_object(
    cas: &lillux::CasStore,
    hash: &str,
    maximum: u64,
    what: &str,
) -> Result<serde_json::Value> {
    let value = cas
        .get_object_bounded(hash, maximum)?
        .ok_or_else(|| anyhow::anyhow!("{what} is absent from CAS"))?;
    anyhow::ensure!(
        lillux::sha256_hex(lillux::canonical_json(&value)?.as_bytes()) == hash,
        "{what} hash differs from its content"
    );
    Ok(value)
}

fn load_event(cas: &lillux::CasStore, hash: &str) -> Result<ThreadEvent> {
    let value = load_object(
        cas,
        hash,
        MAX_THREAD_EVENT_SERIALIZED_BYTES as u64,
        "retained event",
    )?;
    let event: ThreadEvent = serde_json::from_value(value).context("decode retained event")?;
    event.validate()?;
    anyhow::ensure!(hash_event(&event)? == hash, "retained event hash differs");
    Ok(event)
}

fn check_tip(cas: &lillux::CasStore, chain_root_id: &str, tip: &PrefixTip) -> Result<()> {
    let event = load_event(cas, &tip.event_hash)?;
    anyhow::ensure!(
        event.chain_root_id == chain_root_id
            && event.thread_seq == tip.thread_seq
            && event.chain_seq == tip.chain_seq,
        "retained prefix tip coordinates differ from the event"
    );
    Ok(())
}

/// Build the pages back to front so each carries its successor's hash.
fn store_pages(
    cas: &lillux::CasStore,
    owner: &str,
    counterparts: &BTreeSet<String>,
    evidence: &[EvidenceCoordinate],
) -> Result<Option<String>> {
    let placeholder = || RetiredChainClosurePage::new(owner, Some("f".repeat(64)));
    let fits = |page: &RetiredChainClosurePage| -> Result<bool> {
        Ok(page.counterpart_roots.len() + page.retained_evidence.len()
            <= MAX_RETIRED_CHAIN_CLOSURE_PAGE_ENTRIES
            && serde_json::to_vec(page)?.len() as u64 <= MAX_RETIRED_CHAIN_CLOSURE_PAGE_BYTES)
    };
    let mut pages = Vec::new();
    let mut page = placeholder();
    for root in counterparts {
        page.counterpart_roots.push(root.clone());
        if !fits(&page)? {
            page.counterpart_roots.pop();
            let overflowed = page.counterpart_roots.is_empty() && page.retained_evidence.is_empty();
            anyhow::ensure!(
                !overflowed,
                "one retirement counterpart exceeds the page bound"
            );
            pages.push(std::mem::replace(&mut page, placeholder()));
            page.counterpart_roots.push(root.clone());
        }
    }
    for coordinate in evidence {
        page.retained_evidence.push(coordinate.clone());
        if !fits(&page)? {
            page.retained_evidence.pop();
            let overflowed = page.counterpart_roots.is_empty() && page.retained_evidence.is_empty();
            anyhow::ensure!(
                !overflowed,
                "one retained evidence coordinate exceeds the page bound"
            );
            pages.push(std::mem::replace(&mut page, placeholder()));
            page.retained_evidence.push(coordinate.clone());
        }
    }
    if !page.counterpart_roots.is_empty() || !page.retained_evidence.is_empty() {
        pages.push(page);
    }
    let mut next = None;
    for mut page in pages.into_iter().rev() {
        page.next_page_hash = next;
        page.validate()?;
        next = Some(cas.store_object(&serde_json::to_value(page)?)?);
    }
    Ok(next)
}

impl RecoveryStore {
    /// Store a retired chain's closure pages and manifest and return the
    /// manifest hash, which the fence then binds. The owner's chain lock is
    /// held, so the obligation holders read here are the complete set: each is
    /// added to the counterparts. The caller publishes the fence before
    /// preparing the head removal; the fence is what roots the manifest, and
    /// an unfenced manifest is ordinary garbage for the next collection.
    pub fn store_retired_chain_manifest(
        &self,
        cas: &lillux::CasStore,
        cas_mutation_guard: &CasMutationGuard,
        owner_lock: &crate::chain::ChainLock,
        draft: RetiredChainClosureDraft,
        validator: EvidenceValidator<'_>,
    ) -> Result<String> {
        self.ensure_guard(cas_mutation_guard)?;
        self.ensure_chain_lock(owner_lock, &draft.chain_root_id)?;
        let owner = draft.chain_root_id.as_str();
        let mut counterparts = draft.counterpart_roots.clone();
        counterparts.extend(self.evidence_obligation_holders(owner_lock, owner)?);
        anyhow::ensure!(
            !counterparts.contains(owner),
            "a retired chain cannot be its own counterpart"
        );
        let mut evidence = draft.retained_evidence.clone();
        evidence.sort_by(|left, right| left.order_key().cmp(&right.order_key()));
        anyhow::ensure!(
            evidence
                .windows(2)
                .all(|pair| pair[0].order_key() != pair[1].order_key()),
            "retained evidence coordinates are duplicated"
        );
        // The tips are trusted as stated: the retirement driver derives them
        // from the live chains under the owner lock. Here they are only
        // checked against the retained events they name.
        for coordinate in &evidence {
            let owner_only = coordinate.counterpart_root == owner;
            anyhow::ensure!(
                owner_only || counterparts.contains(&coordinate.counterpart_root),
                "retained evidence counterpart {:?} is not a retained counterpart",
                coordinate.counterpart_root
            );
            anyhow::ensure!(
                !owner_only || coordinate.counterpart_tip == coordinate.owner_tip,
                "owner-only retained evidence carries a counterpart tip other than the owner's"
            );
            check_tip(cas, owner, &coordinate.owner_tip)?;
            check_tip(
                cas,
                &coordinate.counterpart_root,
                &coordinate.counterpart_tip,
            )?;
            let event = load_event(cas, &coordinate.event_hash)?;
            let tip = if event.chain_root_id == owner {
                &coordinate.owner_tip
            } else {
                anyhow::ensure!(
                    event.chain_root_id == coordinate.counterpart_root,
                    "retained evidence belongs to neither the owner nor its counterpart"
                );
                &coordinate.counterpart_tip
            };
            anyhow::ensure!(
                event.chain_seq == coordinate.chain_seq
                    && event.chain_seq <= tip.chain_seq
                    && (event.chain_seq != tip.chain_seq
                        || coordinate.event_hash == tip.event_hash),
                "retained evidence exceeds or conflicts with its chain's prefix tip"
            );
            validator(&event).with_context(|| {
                format!(
                    "evidence validator refused retained event {}",
                    coordinate.event_hash
                )
            })?;
        }
        // The manifest links to both, so a dangling or mistyped one would
        // break the collection closure walk.
        let snapshot = load_object(
            cas,
            &draft.snapshot_hash,
            MAX_THREAD_EVENT_SERIALIZED_BYTES as u64,
            "retired chain snapshot",
        )?;
        let snapshot = ThreadSnapshot::from_current_value(snapshot)
            .context("retired chain snapshot is not a thread snapshot")?;
        anyhow::ensure!(
            snapshot.chain_root_id == owner,
            "retired chain snapshot belongs to another chain"
        );
        if let Some(hash) = &draft.terminal_event_hash {
            let event = load_event(cas, hash)?;
            anyhow::ensure!(
                event.chain_root_id == owner
                    && event.thread_seq == draft.terminal_thread_seq
                    && event.chain_seq == draft.terminal_chain_seq,
                "retired chain terminal event disagrees with the stated tip"
            );
        }
        let closure_page_head = store_pages(cas, owner, &counterparts, &evidence)?;
        let manifest = RetiredChainManifest::new(
            draft.chain_root_id.clone(),
            draft.retired_head_hash,
            draft.snapshot_hash,
            draft.terminal_event_hash,
            draft.terminal_thread_seq,
            draft.terminal_chain_seq,
            closure_page_head,
        );
        manifest.validate()?;
        cas.store_object(&serde_json::to_value(&manifest)?)
    }

    /// Release a retired chain's closure from what its stored manifest lists
    /// plus every evidence obligation holder. Already-released chains only
    /// finish removing leftover obligations, so a manifest the sweep already
    /// collected is never needed again.
    pub fn release_retired_chain(
        &self,
        cas: &lillux::CasStore,
        cas_mutation_guard: &CasMutationGuard,
        owner_lock: &crate::chain::ChainLock,
        owner_chain_root_id: &str,
    ) -> Result<()> {
        let counterparts = match self.retired_chain_fence(owner_chain_root_id)? {
            RetiredChainFence::Absent => {
                anyhow::bail!("chain {owner_chain_root_id:?} has no retirement fence to release")
            }
            RetiredChainFence::Released { .. } => BTreeSet::new(),
            RetiredChainFence::Retired { manifest_hash } => {
                read_retired_chain_closure(cas, owner_chain_root_id, &manifest_hash)?
                    .counterpart_roots
            }
        };
        self.publish_retired_chain_release(
            cas_mutation_guard,
            owner_lock,
            owner_chain_root_id,
            &counterparts,
        )
    }
}

/// Read a stored closure: the manifest, then every page, each hash-checked and
/// bound to the owner. Strict ordering across pages means a cycle or a repeated
/// page cannot make the walk loop.
pub fn read_retired_chain_closure(
    cas: &lillux::CasStore,
    owner: &str,
    manifest_hash: &str,
) -> Result<RetiredChainClosure> {
    let value = load_object(
        cas,
        manifest_hash,
        MAX_THREAD_EVENT_SERIALIZED_BYTES as u64,
        "retired chain manifest",
    )?;
    let manifest = RetiredChainManifest::from_value(&value)?;
    anyhow::ensure!(
        manifest.chain_root_id == owner,
        "retired chain manifest belongs to another owner"
    );
    let mut counterparts = BTreeSet::new();
    let mut evidence: Vec<EvidenceCoordinate> = Vec::new();
    let mut previous_root: Option<String> = None;
    let mut next = manifest.closure_page_head.clone();
    while let Some(hash) = next {
        let value = load_object(
            cas,
            &hash,
            MAX_RETIRED_CHAIN_CLOSURE_PAGE_BYTES,
            "retired chain closure page",
        )?;
        let page = RetiredChainClosurePage::from_value(&value)?;
        anyhow::ensure!(
            page.chain_root_id == owner,
            "retired chain closure page belongs to another owner"
        );
        if let (Some(prior), Some(first)) = (&previous_root, page.counterpart_roots.first()) {
            anyhow::ensure!(
                prior < first,
                "retired chain counterpart order repeats across pages"
            );
        }
        if let Some(last) = page.counterpart_roots.last() {
            previous_root = Some(last.clone());
        }
        if let (Some(prior), Some(first)) = (evidence.last(), page.retained_evidence.first()) {
            anyhow::ensure!(
                prior.order_key() < first.order_key(),
                "retained evidence order repeats across pages"
            );
        }
        counterparts.extend(page.counterpart_roots);
        evidence.extend(page.retained_evidence);
        next = page.next_page_hash;
    }
    anyhow::ensure!(
        evidence
            .iter()
            .all(|coordinate| coordinate.counterpart_root == owner
                || counterparts.contains(&coordinate.counterpart_root)),
        "retained evidence counterpart is not a retained counterpart"
    );
    Ok(RetiredChainClosure {
        manifest,
        counterpart_roots: counterparts,
        retained_evidence: evidence,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;

    use super::*;
    use crate::chain::ChainLock;
    use crate::objects::PrefixTip;
    use crate::objects::thread_event::NewEvent;

    struct Fixture {
        temp: tempfile::TempDir,
        store: RecoveryStore,
        guard: CasMutationGuard,
        cas: lillux::CasStore,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        CasMutationGuard::ensure_anchor(temp.path()).unwrap();
        let guard = CasMutationGuard::acquire_exclusive(temp.path()).unwrap();
        let cas = lillux::CasStore::new(temp.path().join("objects"));
        Fixture {
            temp,
            store,
            guard,
            cas,
        }
    }

    impl Fixture {
        fn lock(&self, id: &str) -> ChainLock {
            ChainLock::acquire(&self.temp.path().join("refs"), id).unwrap()
        }

        fn head_path(&self, id: &str) -> PathBuf {
            self.temp
                .path()
                .join("refs/generic/chains")
                .join(id)
                .join("head")
        }

        fn set_head(&self, id: &str) {
            drop(self.lock(id));
            std::fs::write(self.head_path(id), b"head").unwrap();
        }

        fn unlink_head(&self, id: &str) {
            std::fs::remove_file(self.head_path(id)).unwrap();
        }

        /// Store one event of `root` and return its coordinates as a tip.
        fn event(&self, root: &str, thread_seq: u64, chain_seq: u64) -> PrefixTip {
            let event = NewEvent::new(root, root, "retained_fact")
                .thread_seq(thread_seq)
                .chain_seq(chain_seq)
                .build_with_ts("2026-01-01T00:00:00Z".to_string());
            let event_hash = self.cas.store_object(&event.to_value()).unwrap();
            PrefixTip {
                event_hash,
                thread_seq,
                chain_seq,
            }
        }

        fn snapshot(&self, owner: &str) -> String {
            let snapshot = crate::objects::thread_snapshot::ThreadSnapshotBuilder::new(
                owner,
                owner,
                "directive",
                "directive:test",
                "directive-runtime",
            )
            .captured_history_policy(Some(test_policy()))
            .status(crate::objects::thread_snapshot::ThreadStatus::Completed)
            .created_at("2026-01-01T00:00:00Z".to_string())
            .updated_at("2026-01-01T00:00:00Z".to_string())
            .finished_at(Some("2026-01-01T00:00:00Z".to_string()))
            .build();
            self.cas.store_object(&snapshot.to_value()).unwrap()
        }

        fn draft(
            &self,
            counterparts: &[&str],
            evidence: Vec<EvidenceCoordinate>,
        ) -> RetiredChainClosureDraft {
            RetiredChainClosureDraft {
                chain_root_id: "T-owner".to_string(),
                retired_head_hash: "a".repeat(64),
                snapshot_hash: self.snapshot("T-owner"),
                terminal_event_hash: None,
                terminal_thread_seq: 0,
                terminal_chain_seq: 0,
                counterpart_roots: counterparts.iter().map(|root| root.to_string()).collect(),
                retained_evidence: evidence,
            }
        }

        fn store_closure(
            &self,
            draft: RetiredChainClosureDraft,
            validator: EvidenceValidator<'_>,
        ) -> Result<String> {
            let lock = self.lock("T-owner");
            self.store
                .store_retired_chain_manifest(&self.cas, &self.guard, &lock, draft, validator)
        }

        fn fence_owner(&self, manifest_hash: &str) {
            let lock = self.lock("T-owner");
            self.store
                .publish_retired_chain_fence(&self.guard, &lock, "T-owner", manifest_hash)
                .unwrap();
        }

        fn release_owner(&self) -> Result<()> {
            let lock = self.lock("T-owner");
            self.store
                .release_retired_chain(&self.cas, &self.guard, &lock, "T-owner")
        }
    }

    fn test_policy() -> crate::objects::CapturedThreadHistoryPolicy {
        use crate::objects::{
            CapturedEffectiveTrustClass, CapturedItemTrustClass,
            CapturedNodeHistoryPolicyProvenance, CapturedPolicyProvenance,
            CapturedThreadHistoryPolicy, ThreadHistoryRetention,
        };
        let hash = "a".repeat(64);
        CapturedThreadHistoryPolicy {
            retention: ThreadHistoryRetention::TerminalFor { seconds: 60 },
            canonical_item_ref: "directive:test".to_string(),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: CapturedPolicyProvenance::ItemAuthored {
                composed_path: "history".to_string(),
                requested_seconds: 60,
                effective_trust_class: CapturedEffectiveTrustClass::TrustedBundle,
                minimum_clamp: None,
                node_policy: CapturedNodeHistoryPolicyProvenance::test_policy(),
            },
        }
    }

    #[test]
    fn manifest_links_must_resolve_to_the_right_objects() {
        let f = fixture();
        let mut dangling = f.draft(&["T-peer"], Vec::new());
        dangling.snapshot_hash = "9".repeat(64);
        assert!(f.store_closure(dangling, &accept).is_err());
        let mut other_chain = f.draft(&["T-peer"], Vec::new());
        other_chain.snapshot_hash = f.snapshot("T-other");
        assert!(f.store_closure(other_chain, &accept).is_err());
        let tip = f.event("T-owner", 2, 3);
        let mut good = f.draft(&["T-peer"], Vec::new());
        good.terminal_event_hash = Some(tip.event_hash.clone());
        good.terminal_thread_seq = 2;
        good.terminal_chain_seq = 3;
        f.store_closure(good.clone(), &accept).unwrap();
        let mut wrong_seq = good.clone();
        wrong_seq.terminal_chain_seq = 4;
        assert!(f.store_closure(wrong_seq, &accept).is_err());
        let mut absent = good;
        absent.terminal_event_hash = Some("8".repeat(64));
        assert!(f.store_closure(absent, &accept).is_err());
    }

    fn accept(_event: &ThreadEvent) -> Result<()> {
        Ok(())
    }

    /// Evidence: the owner's own tip event, bounded by the owner's tip, with a
    /// counterpart tip alongside.
    fn owner_evidence(f: &Fixture, counterpart: &str) -> EvidenceCoordinate {
        let owner_tip = f.event("T-owner", 2, 3);
        let counterpart_tip = f.event(counterpart, 2, 5);
        EvidenceCoordinate {
            counterpart_root: counterpart.to_string(),
            event_hash: owner_tip.event_hash.clone(),
            chain_seq: 3,
            owner_tip,
            counterpart_tip,
        }
    }

    #[test]
    fn manifest_and_pages_round_trip_with_coordinates_and_validator_calls() {
        let f = fixture();
        let evidence = owner_evidence(&f, "T-peer");
        let seen = Cell::new(0);
        let hash = f
            .store_closure(f.draft(&["T-peer"], vec![evidence.clone()]), &|event| {
                assert_eq!(event.event_type, "retained_fact");
                seen.set(seen.get() + 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen.get(), 1);
        let closure = read_retired_chain_closure(&f.cas, "T-owner", &hash).unwrap();
        assert_eq!(closure.manifest.chain_root_id, "T-owner");
        assert_eq!(
            closure.counterpart_roots,
            BTreeSet::from(["T-peer".to_string()])
        );
        assert_eq!(closure.retained_evidence, vec![evidence]);
        assert!(read_retired_chain_closure(&f.cas, "T-other", &hash).is_err());
        // The same draft stores to the same manifest, so a retry after a
        // crash re-qualifies to the identical hash.
        assert_eq!(
            f.store_closure(f.draft(&["T-peer"], closure.retained_evidence), &accept)
                .unwrap(),
            hash
        );
    }

    #[test]
    fn validator_refusal_and_bad_coordinates_refuse_the_closure() {
        let f = fixture();
        let good = owner_evidence(&f, "T-peer");
        let refused = f
            .store_closure(f.draft(&["T-peer"], vec![good.clone()]), &|_| {
                anyhow::bail!("meaning rejected")
            })
            .unwrap_err();
        assert!(
            format!("{refused:#}").contains("meaning rejected"),
            "{refused:#}"
        );

        let mut beyond = good.clone();
        beyond.chain_seq = 4;
        let mut wrong_chain = good.clone();
        wrong_chain.counterpart_tip = f.event("T-third", 2, 5);
        let mut absent = good.clone();
        absent.event_hash = "9".repeat(64);
        let mut stranger = good.clone();
        stranger.counterpart_root = "T-stranger".to_string();
        for (counterparts, bad) in [
            (vec!["T-peer"], beyond),
            (vec!["T-peer"], wrong_chain),
            (vec!["T-peer"], absent),
            (vec!["T-peer"], stranger),
        ] {
            assert!(
                f.store_closure(f.draft(&counterparts, vec![bad]), &accept)
                    .is_err()
            );
        }
        // A foreign event cannot stand as evidence for this pair.
        let foreign = f.event("T-third", 2, 3);
        let mut other = good.clone();
        other.event_hash = foreign.event_hash;
        assert!(
            f.store_closure(f.draft(&["T-peer"], vec![other]), &accept)
                .is_err()
        );
        // R1: refusals publish nothing a collection or a reader could trust.
        assert!(
            f.store
                .unreleased_retirement_manifest_roots()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            f.store.retired_chain_fence("T-owner").unwrap(),
            RetiredChainFence::Absent
        );
    }

    #[test]
    fn closure_pages_continue_and_keep_every_counterpart() {
        let f = fixture();
        let roots: Vec<String> = (0..300).map(|index| format!("T-p{index:04}")).collect();
        let refs: Vec<&str> = roots.iter().map(String::as_str).collect();
        let hash = f
            .store_closure(f.draft(&refs, Vec::new()), &accept)
            .unwrap();
        let closure = read_retired_chain_closure(&f.cas, "T-owner", &hash).unwrap();
        assert_eq!(closure.counterpart_roots.len(), 300);
        let first = closure.manifest.closure_page_head.clone().unwrap();
        let page = RetiredChainClosurePage::from_value(
            &f.cas
                .get_object_bounded(&first, MAX_RETIRED_CHAIN_CLOSURE_PAGE_BYTES)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            page.counterpart_roots.len(),
            MAX_RETIRED_CHAIN_CLOSURE_PAGE_ENTRIES
        );
        assert!(page.next_page_hash.is_some());
    }

    #[test]
    fn fence_manifest_release_lifecycle_waits_for_every_counterpart_and_obligation() {
        let f = fixture();
        f.set_head("T-owner");
        f.set_head("T-peer");
        f.set_head("T-holder");
        // The holder records an obligation against the still-live owner.
        {
            let holder = f.lock("T-holder");
            let owner = f.lock("T-owner");
            let mut appended = false;
            f.store
                .publish_evidence_obligation_then(
                    &f.guard,
                    &owner,
                    &holder,
                    "T-owner",
                    "T-holder",
                    || {
                        appended = true;
                        Ok(())
                    },
                )
                .unwrap();
            assert!(appended);
        }
        let hash = f
            .store_closure(f.draft(&["T-peer"], Vec::new()), &accept)
            .unwrap();
        let closure = read_retired_chain_closure(&f.cas, "T-owner", &hash).unwrap();
        assert_eq!(
            closure.counterpart_roots,
            BTreeSet::from(["T-holder".to_string(), "T-peer".to_string()])
        );
        // R3/R4: manifest stored, fence published, head still present.
        f.fence_owner(&hash);
        assert_eq!(
            f.store.unreleased_retirement_manifest_roots().unwrap(),
            BTreeSet::from([hash.clone()])
        );
        assert_eq!(
            f.store.unreleased_retired_chains().unwrap(),
            std::collections::BTreeMap::from([("T-owner".to_string(), hash.clone())])
        );
        assert!(
            f.release_owner()
                .unwrap_err()
                .to_string()
                .contains("still exists")
        );
        // R5: head unlinked; counterparts still live.
        f.unlink_head("T-owner");
        assert!(
            f.release_owner()
                .unwrap_err()
                .to_string()
                .contains("not fenced")
        );
        for id in ["T-peer", "T-holder"] {
            f.unlink_head(id);
            let lock = f.lock(id);
            f.store
                .publish_retired_chain_fence(&f.guard, &lock, id, &"cd".repeat(32))
                .unwrap();
        }
        // R6 onward: release is durable and the root drops out.
        f.release_owner().unwrap();
        assert!(
            f.store
                .unreleased_retirement_manifest_roots()
                .unwrap()
                .iter()
                .all(|root| root != &hash)
        );
        assert!(matches!(
            f.store.retired_chain_fence("T-owner").unwrap(),
            RetiredChainFence::Released { .. }
        ));
        // A second pass needs no manifest at all.
        std::fs::remove_dir_all(f.temp.path().join("objects")).unwrap();
        f.release_owner().unwrap();
    }

    #[test]
    fn release_without_a_fence_refuses() {
        let f = fixture();
        assert!(f.release_owner().is_err());
    }

    #[test]
    fn a_fenced_chain_can_never_prepare_a_new_head() {
        let f = fixture();
        f.set_head("T-owner");
        f.fence_owner(&"cd".repeat(32));
        let lock = f.lock("T-owner");
        let refused = f
            .store
            .prepare_set(&lock, "T-owner", None, &"ef".repeat(32))
            .unwrap_err();
        assert!(
            refused.to_string().contains("fenced as retired"),
            "{refused:#}"
        );
    }
}
