//! Driving one chain's retirement to a fenced, released closure.
//!
//! Every step is durable and every cut between steps is resumable:
//!
//! 1. the closure manifest and its pages are stored in CAS (unrooted until
//!    fenced; a crash here leaves garbage for the next collection);
//! 2. the fence is published, which roots the manifest and forbids any later
//!    head Set (fence plus a live head is an interrupted retirement);
//! 3. the Remove is prepared and the head unlinked;
//! 4. the projection is deleted and the caller's runtime cleanup runs;
//! 5. the Remove is acknowledged.
//!
//! The pending Remove carries no manifest hash: the fence roots the manifest.
//! A fenced chain's Remove can never be cancelled, so a retry or startup
//! replay always drives the chain forward, never back.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;

use super::*;
use crate::objects::thread_event::hash_event;
use crate::objects::{ChainState, EvidenceCoordinate, PrefixTip};
use crate::recovery::{
    CasMutationGuard, EvidenceValidator, RetiredChainClosureDraft, RetiredChainFence,
    read_retired_chain_closure,
};

/// One retained event the caller wants kept as evidence. State derives the
/// sequence position and both chains' prefix tips from the live chains. A
/// counterpart root equal to the retired chain itself keeps an event of that
/// chain alone, with no other chain involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetirementEvidenceRef {
    pub counterpart_root: String,
    pub event_hash: String,
}

/// What the caller decides about one retirement. State decides nothing about
/// what the retained events mean: the validator does.
///
/// A chain that is already fenced keeps its stored manifest and ignores this
/// request's evidence and validator; the report's `stored_manifest == false`
/// is the signal that the validator was not consulted.
pub struct ChainRetirement<'a> {
    /// Counterpart chains to retain. Every evidence counterpart is added.
    pub counterpart_roots: BTreeSet<String>,
    pub evidence: Vec<RetirementEvidenceRef>,
    /// Chain locks of every evidence counterpart, acquired by the caller in
    /// the same global order as the owner lock.
    pub counterpart_locks: &'a [&'a crate::chain::ChainLock],
    pub validator: EvidenceValidator<'a>,
    /// Deletes the runtime rows and files of the removed chain. It runs after
    /// the head is gone and the projection is deleted, and again on every
    /// re-drive of an unacknowledged Remove, so it must be idempotent.
    pub runtime_cleanup: &'a dyn Fn() -> anyhow::Result<()>,
}

/// Result of driving one retirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainRetirementReport {
    /// The manifest the fence binds.
    pub manifest_hash: String,
    /// False when an earlier attempt had already fenced the chain, so this
    /// call only finished the remaining steps.
    pub stored_manifest: bool,
}

/// What a startup replay of interrupted retirements did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FencedRetirementReplay {
    pub fences_checked: usize,
    /// Chains whose fence found a live head and whose head was unlinked.
    pub heads_unlinked: usize,
}

fn load_event(cas: &lillux::CasStore, hash: &str) -> anyhow::Result<ThreadEvent> {
    let value = cas
        .get_object(hash)?
        .ok_or_else(|| anyhow::anyhow!("event {hash} is absent from CAS"))?;
    let event: ThreadEvent = serde_json::from_value(value).context("decode event")?;
    event.validate()?;
    anyhow::ensure!(
        hash_event(&event)? == hash,
        "event hash differs from content"
    );
    Ok(event)
}

pub(super) fn read_chain_state(
    cas: &lillux::CasStore,
    trust_store: &TrustStore,
    lock: &crate::chain::ChainLock,
    chain_root_id: &str,
) -> anyhow::Result<Option<(SignedRef, ChainState)>> {
    let Some(head) = lock.read_verified_head(trust_store)? else {
        return Ok(None);
    };
    let value = cas
        .get_object(&head.target_hash)?
        .ok_or_else(|| anyhow::anyhow!("chain state {} is absent from CAS", head.target_hash))?;
    let state: ChainState = serde_json::from_value(value).context("decode chain state")?;
    state.validate()?;
    anyhow::ensure!(
        state.chain_root_id == chain_root_id
            && lillux::sha256_hex(lillux::canonical_json(&state.to_value())?.as_bytes())
                == head.target_hash,
        "chain state does not match the signed head of {chain_root_id}"
    );
    Ok(Some((head, state)))
}

/// The chain's newest event as a prefix tip, or none for an eventless chain.
pub(super) fn chain_tip(
    cas: &lillux::CasStore,
    state: &ChainState,
) -> anyhow::Result<Option<PrefixTip>> {
    let Some(hash) = &state.last_event_hash else {
        return Ok(None);
    };
    let event = load_event(cas, hash)?;
    anyhow::ensure!(
        event.chain_root_id == state.chain_root_id && event.chain_seq == state.last_chain_seq,
        "chain tip event disagrees with its chain state"
    );
    Ok(Some(PrefixTip {
        event_hash: hash.clone(),
        thread_seq: event.thread_seq,
        chain_seq: event.chain_seq,
    }))
}

impl StateDb {
    /// Drive a chain's retirement through the fence to a released head.
    ///
    /// The caller holds the shared CAS guard and the owner's chain lock (and
    /// the counterpart locks it names), and has completed its own terminality
    /// and runtime-pin recheck. Calling this again after any interruption
    /// finishes the remaining steps; once the Remove is acknowledged it is a
    /// no-op. An already-fenced chain keeps its stored manifest: the request's
    /// evidence is not consulted again.
    pub fn retire_chain(
        &self,
        chain_root_id: &str,
        owner_lock: &crate::chain::ChainLock,
        guard: &CasMutationGuard,
        request: ChainRetirement<'_>,
    ) -> anyhow::Result<ChainRetirementReport> {
        self.ensure_cas_mutation_guard(guard)?;
        ensure_state_chain_lock(&self.refs_root, owner_lock, chain_root_id)?;
        let cas = self.pinned_cas()?;
        let (manifest_hash, stored_manifest) =
            match self.recovery.retired_chain_fence(chain_root_id)? {
                RetiredChainFence::Absent => {
                    let draft =
                        self.derive_retirement_draft(&cas, chain_root_id, owner_lock, &request)?;
                    let manifest_hash = self.recovery.store_retired_chain_manifest(
                        &cas,
                        guard,
                        owner_lock,
                        draft,
                        request.validator,
                    )?;
                    self.recovery.publish_retired_chain_fence(
                        guard,
                        owner_lock,
                        chain_root_id,
                        &manifest_hash,
                    )?;
                    (manifest_hash, true)
                }
                RetiredChainFence::Retired { manifest_hash } => (manifest_hash, false),
                RetiredChainFence::Released { manifest_hash } => {
                    anyhow::ensure!(
                        owner_lock
                            .read_verified_head(self.trust_store.as_ref())?
                            .is_none(),
                        "chain {chain_root_id} is released but still has a head"
                    );
                    (manifest_hash, false)
                }
            };
        self.unlink_fenced_head_locked(&cas, chain_root_id, owner_lock, &manifest_hash)?;
        match self.recovery.read_pending(chain_root_id)? {
            None => {}
            Some(pending) if pending.operation == HeadOperation::Remove => {
                self.delete_chain_projection(chain_root_id, owner_lock)?;
                (request.runtime_cleanup)()?;
                self.acknowledge_chain_removal_cleanup(chain_root_id, owner_lock)?;
            }
            Some(_) => anyhow::bail!(
                "fenced chain {chain_root_id} has a pending Set; it can never publish a head again"
            ),
        }
        Ok(ChainRetirementReport {
            manifest_hash,
            stored_manifest,
        })
    }

    /// Finish the head half of every interrupted retirement: each fence whose
    /// chain still has a head gets its Remove prepared, its head unlinked and
    /// its projection deleted. Runtime cleanup and the acknowledgement stay
    /// with the pending Remove, whose owner holds the runtime. A head that
    /// does not match its fenced closure refuses the replay.
    pub fn replay_fenced_chain_retirements(
        &self,
        guard: &CasMutationGuard,
    ) -> anyhow::Result<FencedRetirementReplay> {
        self.ensure_cas_mutation_guard(guard)?;
        let cas = self.pinned_cas()?;
        let mut report = FencedRetirementReplay::default();
        for (owner, manifest_hash) in self.recovery.unreleased_retired_chains()? {
            report.fences_checked += 1;
            let lock = self
                .acquire_existing_chain_lock(&owner)
                .with_context(|| format!("lock fenced chain {owner}"))?;
            if lock
                .read_verified_head(self.trust_store.as_ref())?
                .is_none()
            {
                continue;
            }
            self.unlink_fenced_head_locked(&cas, &owner, &lock, &manifest_hash)?;
            self.delete_chain_projection(&owner, &lock)?;
            report.heads_unlinked += 1;
        }
        Ok(report)
    }

    /// Prepare the Remove and unlink the head of a fenced chain that still has
    /// one. The head must be the one the fenced closure retired.
    fn unlink_fenced_head_locked(
        &self,
        cas: &lillux::CasStore,
        chain_root_id: &str,
        lock: &crate::chain::ChainLock,
        manifest_hash: &str,
    ) -> anyhow::Result<()> {
        let Some(head) = lock.read_verified_head(self.trust_store.as_ref())? else {
            return Ok(());
        };
        let closure = read_retired_chain_closure(cas, chain_root_id, manifest_hash)?;
        anyhow::ensure!(
            closure.manifest.retired_head_hash == head.target_hash,
            "fenced chain {chain_root_id} has a head its retired closure does not describe"
        );
        self.remove_chain_head_ref(chain_root_id, lock)?;
        Ok(())
    }

    /// Everything a closure records, derived from the live chains under their
    /// locks: no coordinate is trusted from the caller.
    pub(super) fn derive_retirement_draft(
        &self,
        cas: &lillux::CasStore,
        chain_root_id: &str,
        owner_lock: &crate::chain::ChainLock,
        request: &ChainRetirement<'_>,
    ) -> anyhow::Result<RetiredChainClosureDraft> {
        let (head, state) =
            read_chain_state(cas, self.trust_store.as_ref(), owner_lock, chain_root_id)?
                .ok_or_else(|| {
                    anyhow::anyhow!("chain {chain_root_id} has no live head to retire")
                })?;
        let root = state
            .threads
            .get(chain_root_id)
            .ok_or_else(|| anyhow::anyhow!("chain {chain_root_id} has no root thread"))?;
        let (terminal_event_hash, terminal_thread_seq, terminal_chain_seq) =
            match &root.last_event_hash {
                Some(hash) => {
                    let event = load_event(cas, hash)?;
                    (Some(hash.clone()), root.last_thread_seq, event.chain_seq)
                }
                None => (None, 0, 0),
            };
        let owner_tip = chain_tip(cas, &state)?;
        let mut counterpart_roots = request.counterpart_roots.clone();
        let mut counterpart_tips: BTreeMap<&str, PrefixTip> = BTreeMap::new();
        let mut retained_evidence = Vec::with_capacity(request.evidence.len());
        for reference in &request.evidence {
            let counterpart = reference.counterpart_root.as_str();
            let owner_tip = owner_tip
                .clone()
                .ok_or_else(|| anyhow::anyhow!("chain {chain_root_id} has no events"))?;
            let event = load_event(cas, &reference.event_hash)?;
            if counterpart == chain_root_id {
                anyhow::ensure!(
                    event.chain_root_id == chain_root_id,
                    "owner-only evidence is not an event of the retired chain"
                );
                retained_evidence.push(EvidenceCoordinate {
                    counterpart_root: counterpart.to_string(),
                    event_hash: reference.event_hash.clone(),
                    chain_seq: event.chain_seq,
                    counterpart_tip: owner_tip.clone(),
                    owner_tip,
                });
                continue;
            }
            if !counterpart_tips.contains_key(counterpart) {
                let lock = request
                    .counterpart_locks
                    .iter()
                    .find(|lock| lock.chain_root_id() == counterpart)
                    .ok_or_else(|| {
                        anyhow::anyhow!("evidence counterpart {counterpart} was not locked")
                    })?;
                ensure_state_chain_lock(&self.refs_root, lock, counterpart)?;
                let (_, counterpart_state) =
                    read_chain_state(cas, self.trust_store.as_ref(), lock, counterpart)?
                        .ok_or_else(|| {
                            anyhow::anyhow!("evidence counterpart {counterpart} has no live head")
                        })?;
                let tip = chain_tip(cas, &counterpart_state)?.ok_or_else(|| {
                    anyhow::anyhow!("evidence counterpart {counterpart} has no events")
                })?;
                counterpart_tips.insert(counterpart, tip);
            }
            let counterpart_tip = counterpart_tips[counterpart].clone();
            counterpart_roots.insert(counterpart.to_string());
            retained_evidence.push(EvidenceCoordinate {
                counterpart_root: counterpart.to_string(),
                event_hash: reference.event_hash.clone(),
                chain_seq: event.chain_seq,
                owner_tip,
                counterpart_tip,
            });
        }
        Ok(RetiredChainClosureDraft {
            chain_root_id: chain_root_id.to_string(),
            retired_head_hash: head.target_hash,
            snapshot_hash: root.snapshot_hash.clone(),
            terminal_event_hash,
            terminal_thread_seq,
            terminal_chain_seq,
            counterpart_roots,
            retained_evidence,
        })
    }
}
