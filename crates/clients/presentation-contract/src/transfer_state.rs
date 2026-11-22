//! Pure canonical fold of caller-verified transfer testimony.
//!
//! This state proves only cross-record consistency. The caller remains
//! responsible for CAS bytes, chain/session ownership, current admission and
//! append serialization before supplying testimony.
use crate::{
    transfer::{TransferEvidence, TransferIdentity, TransferPhase, TransferRecord},
    transfer_proof::{
        TransferTestimony, verify_aborted, verify_accepted, verify_activated, verify_committed,
        verify_orphan_cancelled, verify_prepared, verify_released, verify_retained,
        verify_settlement_observed,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferSourceState {
    /// A destination-only crash orphan has no source obligation yet.
    Unprepared,
    Frozen,
    Committed,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDestinationState {
    Unreserved,
    /// Inbound-only orphan cancelled without source preparation or reservation.
    OrphanCancelled,
    Reserved,
    /// Historical activation, not proof of current attachment or dispatch authority.
    Active,
    /// Historical committed data only, with no live destination publication.
    Retained,
    Released,
}

#[derive(Debug, Clone, PartialEq)]
struct OwnedTestimony {
    evidence: TransferEvidence,
    record: TransferRecord,
}

impl OwnedTestimony {
    fn borrowed(&self) -> TransferTestimony<'_> {
        TransferTestimony {
            evidence: &self.evidence,
            record: &self.record,
        }
    }

    fn same(&self, testimony: TransferTestimony<'_>) -> bool {
        self.evidence == *testimony.evidence && self.record == *testimony.record
    }
}

/// One operation's consistency projection. It owns no authority and imposes no
/// quota; callers retain and bound the underlying testimony with existing seat
/// owners before folding it here.
#[derive(Debug, Clone, PartialEq)]
pub struct TransferLifecycleState {
    transfer: TransferIdentity,
    inbound: OwnedTestimony,
    orphan_cancelled: Option<OwnedTestimony>,
    prepared: Option<OwnedTestimony>,
    accepted: Option<OwnedTestimony>,
    committed: Option<OwnedTestimony>,
    aborted: Option<OwnedTestimony>,
    activated: Option<OwnedTestimony>,
    retained: Option<OwnedTestimony>,
    released: Option<OwnedTestimony>,
    settlement_observed: Option<OwnedTestimony>,
}

impl TransferLifecycleState {
    pub fn begin(testimony: TransferTestimony<'_>) -> Result<Self, String> {
        testimony.record.validate_shape()?;
        if !matches!(&testimony.record.phase, TransferPhase::InboundCommitment) {
            return Err("transfer lifecycle must begin with destination inbound commitment".into());
        }
        validate_testimony_evidence(testimony.evidence)?;
        Ok(Self {
            transfer: testimony.record.transfer.clone(),
            inbound: owned(testimony),
            orphan_cancelled: None,
            prepared: None,
            accepted: None,
            committed: None,
            aborted: None,
            activated: None,
            retained: None,
            released: None,
            settlement_observed: None,
        })
    }

    /// Fold one caller-verified record. Exact retries are idempotent; any
    /// competing record for an occupied phase or event position refuses.
    pub fn fold(&mut self, testimony: TransferTestimony<'_>) -> Result<(), String> {
        testimony.record.validate_shape()?;
        validate_testimony_evidence(testimony.evidence)?;
        if testimony.record.transfer != self.transfer {
            return Err("transfer lifecycle identity changed".into());
        }
        match &testimony.record.phase {
            TransferPhase::InboundCommitment => {
                if self.inbound.same(testimony) {
                    return Ok(());
                }
                return Err("transfer inbound commitment conflicts with retained testimony".into());
            }
            TransferPhase::OrphanCancelled { .. } => {
                if retry(&self.orphan_cancelled, testimony)? {
                    return Ok(());
                }
                if self.prepared.is_some()
                    || self.accepted.is_some()
                    || self.committed.is_some()
                    || self.aborted.is_some()
                {
                    return Err(
                        "orphan cancellation followed source preparation or decision".into(),
                    );
                }
                self.ensure_unused_position(testimony)?;
                verify_orphan_cancelled(testimony.record, self.inbound.borrowed())?;
                self.orphan_cancelled = Some(owned(testimony));
            }
            TransferPhase::Prepared { .. } => {
                if retry(&self.prepared, testimony)? {
                    return Ok(());
                }
                if self.orphan_cancelled.is_some() {
                    return Err(
                        "source preparation followed destination orphan cancellation".into(),
                    );
                }
                self.ensure_unused_position(testimony)?;
                verify_prepared(testimony.record, self.inbound.borrowed())?;
                self.prepared = Some(owned(testimony));
            }
            TransferPhase::Accepted { .. } => {
                if retry(&self.accepted, testimony)? {
                    return Ok(());
                }
                if self.aborted.is_some() || self.committed.is_some() {
                    return Err("destination acceptance followed a terminal source decision".into());
                }
                let prepared = self
                    .prepared
                    .as_ref()
                    .ok_or("destination acceptance preceded source preparation")?;
                self.ensure_unused_position(testimony)?;
                verify_accepted(testimony.record, prepared.borrowed())?;
                self.accepted = Some(owned(testimony));
            }
            TransferPhase::Committed { .. } => {
                if retry(&self.committed, testimony)? {
                    return Ok(());
                }
                if self.aborted.is_some() {
                    return Err("source commit conflicts with retained abort".into());
                }
                let accepted = self
                    .accepted
                    .as_ref()
                    .ok_or("source commit preceded destination acceptance")?;
                let prepared = self
                    .prepared
                    .as_ref()
                    .ok_or("source commit preceded preparation")?;
                self.ensure_unused_position(testimony)?;
                verify_committed(testimony.record, prepared.borrowed(), accepted.borrowed())?;
                self.committed = Some(owned(testimony));
            }
            TransferPhase::Aborted { .. } => {
                if retry(&self.aborted, testimony)? {
                    return Ok(());
                }
                if self.committed.is_some() || self.activated.is_some() || self.retained.is_some() {
                    return Err("source abort conflicts with retained commit".into());
                }
                let prepared = self
                    .prepared
                    .as_ref()
                    .ok_or("source abort preceded preparation")?;
                self.ensure_unused_position(testimony)?;
                verify_aborted(testimony.record, prepared.borrowed())?;
                self.aborted = Some(owned(testimony));
            }
            TransferPhase::Activated { .. } => {
                if retry(&self.activated, testimony)? {
                    return Ok(());
                }
                if self.aborted.is_some() || self.released.is_some() {
                    return Err("destination activation conflicts with retained abort".into());
                }
                if self.retained.is_some() {
                    return Err(
                        "destination activation conflicts with retained-data settlement".into(),
                    );
                }
                let committed = self
                    .committed
                    .as_ref()
                    .ok_or("destination activation preceded source commit")?;
                self.ensure_unused_position(testimony)?;
                verify_activated(testimony.record, committed.borrowed())?;
                self.activated = Some(owned(testimony));
            }
            TransferPhase::Retained { .. } => {
                if retry(&self.retained, testimony)? {
                    return Ok(());
                }
                if self.aborted.is_some() || self.released.is_some() {
                    return Err("retained-data settlement conflicts with retained abort".into());
                }
                if self.activated.is_some() {
                    return Err(
                        "retained-data settlement cannot replace destination activation".into(),
                    );
                }
                let committed = self
                    .committed
                    .as_ref()
                    .ok_or("retained-data settlement preceded source commit")?;
                self.ensure_unused_position(testimony)?;
                verify_retained(testimony.record, committed.borrowed())?;
                self.retained = Some(owned(testimony));
            }
            TransferPhase::Released { .. } => {
                if retry(&self.released, testimony)? {
                    return Ok(());
                }
                if self.activated.is_some() || self.retained.is_some() || self.committed.is_some() {
                    return Err("destination release conflicts with retained activation".into());
                }
                let aborted = self
                    .aborted
                    .as_ref()
                    .ok_or("destination release preceded source abort")?;
                let accepted = self
                    .accepted
                    .as_ref()
                    .ok_or("destination release preceded destination acceptance")?;
                self.ensure_unused_position(testimony)?;
                verify_released(testimony.record, aborted.borrowed(), accepted.borrowed())?;
                self.released = Some(owned(testimony));
            }
            TransferPhase::SettlementObserved { .. } => {
                if retry(&self.settlement_observed, testimony)? {
                    return Ok(());
                }
                let terminal = self
                    .activated
                    .as_ref()
                    .or(self.retained.as_ref())
                    .or(self.released.as_ref())
                    .ok_or("source observation preceded destination terminal testimony")?;
                self.ensure_unused_position(testimony)?;
                verify_settlement_observed(testimony.record, terminal.borrowed())?;
                self.settlement_observed = Some(owned(testimony));
            }
        }
        Ok(())
    }

    pub fn transfer(&self) -> &TransferIdentity {
        &self.transfer
    }

    pub fn source_state(&self) -> TransferSourceState {
        if self.committed.is_some() {
            TransferSourceState::Committed
        } else if self.aborted.is_some() {
            TransferSourceState::Aborted
        } else {
            if self.prepared.is_some() {
                TransferSourceState::Frozen
            } else {
                TransferSourceState::Unprepared
            }
        }
    }

    pub fn destination_state(&self) -> TransferDestinationState {
        if self.orphan_cancelled.is_some() {
            TransferDestinationState::OrphanCancelled
        } else if self.released.is_some() {
            TransferDestinationState::Released
        } else if self.activated.is_some() {
            TransferDestinationState::Active
        } else if self.retained.is_some() {
            TransferDestinationState::Retained
        } else if self.accepted.is_some() {
            TransferDestinationState::Reserved
        } else {
            TransferDestinationState::Unreserved
        }
    }

    pub fn inbound(&self) -> TransferTestimony<'_> {
        self.inbound.borrowed()
    }
    pub fn orphan_cancelled(&self) -> Option<TransferTestimony<'_>> {
        self.orphan_cancelled.as_ref().map(OwnedTestimony::borrowed)
    }
    pub fn prepared(&self) -> Option<TransferTestimony<'_>> {
        self.prepared.as_ref().map(OwnedTestimony::borrowed)
    }
    pub fn accepted(&self) -> Option<TransferTestimony<'_>> {
        self.accepted.as_ref().map(OwnedTestimony::borrowed)
    }
    pub fn committed(&self) -> Option<TransferTestimony<'_>> {
        self.committed.as_ref().map(OwnedTestimony::borrowed)
    }
    pub fn aborted(&self) -> Option<TransferTestimony<'_>> {
        self.aborted.as_ref().map(OwnedTestimony::borrowed)
    }
    pub fn activated(&self) -> Option<TransferTestimony<'_>> {
        self.activated.as_ref().map(OwnedTestimony::borrowed)
    }
    pub fn retained(&self) -> Option<TransferTestimony<'_>> {
        self.retained.as_ref().map(OwnedTestimony::borrowed)
    }
    pub fn released(&self) -> Option<TransferTestimony<'_>> {
        self.released.as_ref().map(OwnedTestimony::borrowed)
    }
    /// Retained evidence, not permission to finalize or retire either seat.
    pub fn settlement_observed(&self) -> Option<TransferTestimony<'_>> {
        self.settlement_observed
            .as_ref()
            .map(OwnedTestimony::borrowed)
    }

    fn ensure_unused_position(&self, testimony: TransferTestimony<'_>) -> Result<(), String> {
        if [
            Some(&self.inbound),
            self.orphan_cancelled.as_ref(),
            self.prepared.as_ref(),
            self.accepted.as_ref(),
            self.committed.as_ref(),
            self.aborted.as_ref(),
            self.activated.as_ref(),
            self.retained.as_ref(),
            self.released.as_ref(),
            self.settlement_observed.as_ref(),
        ]
        .into_iter()
        .flatten()
        .any(|known| {
            known.evidence.event_hash == testimony.evidence.event_hash
                || (known.evidence.chain_seq == testimony.evidence.chain_seq
                    && known.record.event_owner() == testimony.record.event_owner())
        }) {
            return Err("transfer phases reuse one testimony event position".into());
        }
        Ok(())
    }
}

fn retry(
    retained: &Option<OwnedTestimony>,
    testimony: TransferTestimony<'_>,
) -> Result<bool, String> {
    match retained {
        Some(retained) if retained.same(testimony) => Ok(true),
        Some(_) => Err("transfer phase conflicts with retained testimony".into()),
        None => Ok(false),
    }
}

fn owned(testimony: TransferTestimony<'_>) -> OwnedTestimony {
    OwnedTestimony {
        evidence: testimony.evidence.clone(),
        record: testimony.record.clone(),
    }
}

fn validate_testimony_evidence(evidence: &TransferEvidence) -> Result<(), String> {
    if evidence.event_hash.len() != 64
        || !evidence
            .event_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("transfer testimony event hash is not canonical".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload_digest::seat_payload_digest;
    use serde_json::json;

    fn testimony_chain() -> Vec<OwnedTestimony> {
        let export = json!({"mode":"copy","views":["view:test/a"]});
        let seat = |presentation: &str, side: &str| crate::transfer::TransferSeat {
            presentation_id: presentation.into(),
            thread_id: format!("thread-{side}"),
            chain_root_id: format!("chain-{side}"),
            producer_incarnation: "ab".repeat(32),
            revision: 3,
        };
        let transfer = TransferIdentity {
            operation_id: "33333333-3333-4333-8333-333333333333".into(),
            source: seat("11111111-1111-4111-8111-111111111111", "source"),
            destination: seat("22222222-2222-4222-8222-222222222222", "destination"),
            export_digest: seat_payload_digest(&export).unwrap(),
        };
        let evidence = |marker: &str, chain_seq| TransferEvidence {
            event_hash: marker.repeat(32),
            chain_seq,
        };
        let inbound_evidence = evidence("00", 1);
        let prepared_evidence = evidence("aa", 1);
        let accepted_evidence = evidence("bb", 2);
        let committed_evidence = evidence("cc", 3);
        let reservation = json!({"mounts":["destination:1"]});
        let import_digest = seat_payload_digest(&reservation).unwrap();
        let prepared = TransferRecord {
            schema: crate::transfer::TRANSFER_SCHEMA,
            transfer: transfer.clone(),
            phase: TransferPhase::Prepared {
                inbound: inbound_evidence,
                freeze: None,
                export,
            },
        };
        let accepted = TransferRecord {
            schema: crate::transfer::TRANSFER_SCHEMA,
            transfer: transfer.clone(),
            phase: TransferPhase::Accepted {
                preparation: prepared_evidence.clone(),
                reservation,
                import_digest: import_digest.clone(),
            },
        };
        let committed = TransferRecord {
            schema: crate::transfer::TRANSFER_SCHEMA,
            transfer: transfer.clone(),
            phase: TransferPhase::Committed {
                preparation: prepared_evidence.clone(),
                acceptance: accepted_evidence.clone(),
                import_digest: import_digest.clone(),
            },
        };
        let activated = TransferRecord {
            schema: crate::transfer::TRANSFER_SCHEMA,
            transfer,
            phase: TransferPhase::Activated {
                commit: committed_evidence.clone(),
                import_digest,
            },
        };
        vec![
            OwnedTestimony {
                evidence: prepared_evidence,
                record: prepared,
            },
            OwnedTestimony {
                evidence: accepted_evidence,
                record: accepted,
            },
            OwnedTestimony {
                evidence: committed_evidence,
                record: committed,
            },
            OwnedTestimony {
                evidence: evidence("ee", 4),
                record: activated,
            },
        ]
    }

    fn inbound_testimony(chain: &[OwnedTestimony]) -> OwnedTestimony {
        let TransferPhase::Prepared { inbound, .. } = &chain[0].record.phase else {
            unreachable!()
        };
        OwnedTestimony {
            evidence: inbound.clone(),
            record: TransferRecord {
                schema: crate::transfer::TRANSFER_SCHEMA,
                transfer: chain[0].record.transfer.clone(),
                phase: TransferPhase::InboundCommitment,
            },
        }
    }

    fn prepared_state(chain: &[OwnedTestimony]) -> TransferLifecycleState {
        let inbound = inbound_testimony(chain);
        let mut state = TransferLifecycleState::begin(inbound.borrowed()).unwrap();
        state.fold(chain[0].borrowed()).unwrap();
        state
    }

    #[test]
    fn destination_only_orphan_is_inert_and_acceptance_requires_exact_preparation() {
        let chain = testimony_chain();
        let inbound = inbound_testimony(&chain);
        assert!(TransferLifecycleState::begin(chain[0].borrowed()).is_err());
        let mut state = TransferLifecycleState::begin(inbound.borrowed()).unwrap();
        assert_eq!(state.source_state(), TransferSourceState::Unprepared);
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::Unreserved
        );
        assert!(state.prepared().is_none());
        assert!(state.fold(chain[1].borrowed()).is_err());
        assert!(state.fold(chain[2].borrowed()).is_err());
        assert_eq!(state.source_state(), TransferSourceState::Unprepared);
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::Unreserved
        );

        let mut wrong = chain[0].clone();
        if let TransferPhase::Prepared { inbound: link, .. } = &mut wrong.record.phase {
            link.chain_seq += 1;
        }
        assert!(state.fold(wrong.borrowed()).is_err());
        if let TransferPhase::Prepared { inbound: link, .. } = &mut wrong.record.phase {
            link.chain_seq -= 1;
            link.event_hash = "12".repeat(32);
        }
        assert!(state.fold(wrong.borrowed()).is_err());
        assert!(state.prepared().is_none());

        state.fold(chain[0].borrowed()).unwrap();
        assert_eq!(state.source_state(), TransferSourceState::Frozen);
        state.fold(chain[1].borrowed()).unwrap();
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::Reserved
        );
    }

    #[test]
    fn exact_orphan_cancellation_is_inert_terminal_destination_testimony() {
        let chain = testimony_chain();
        let inbound = inbound_testimony(&chain);
        let cancellation = OwnedTestimony {
            evidence: TransferEvidence {
                event_hash: "01".repeat(32),
                chain_seq: inbound.evidence.chain_seq + 1,
            },
            record: TransferRecord {
                schema: crate::transfer::TRANSFER_SCHEMA,
                transfer: chain[0].record.transfer.clone(),
                phase: TransferPhase::OrphanCancelled {
                    inbound: inbound.evidence.clone(),
                },
            },
        };
        let mut state = TransferLifecycleState::begin(inbound.borrowed()).unwrap();
        let mut wrong = cancellation.clone();
        if let TransferPhase::OrphanCancelled { inbound } = &mut wrong.record.phase {
            inbound.chain_seq += 1;
        }
        assert!(state.fold(wrong.borrowed()).is_err());
        state.fold(cancellation.borrowed()).unwrap();
        state.fold(cancellation.borrowed()).unwrap();
        assert_eq!(state.source_state(), TransferSourceState::Unprepared);
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::OrphanCancelled
        );
        assert!(state.prepared().is_none());
        assert_eq!(
            state.orphan_cancelled().unwrap().evidence,
            &cancellation.evidence
        );
        assert!(state.fold(chain[0].borrowed()).is_err());
        assert!(state.fold(chain[1].borrowed()).is_err());

        let mut prepared = prepared_state(&chain);
        assert!(prepared.fold(cancellation.borrowed()).is_err());
    }

    fn retained_testimony(activated: &OwnedTestimony) -> OwnedTestimony {
        let TransferPhase::Activated {
            commit,
            import_digest,
        } = &activated.record.phase
        else {
            unreachable!()
        };
        let mut retained = activated.clone();
        retained.evidence.event_hash = "dd".repeat(32);
        retained.record.phase = TransferPhase::Retained {
            commit: commit.clone(),
            import_digest: import_digest.clone(),
        };
        retained
    }

    fn abort_testimony(chain: &[OwnedTestimony]) -> OwnedTestimony {
        OwnedTestimony {
            evidence: TransferEvidence {
                event_hash: "ff".repeat(32),
                chain_seq: 3,
            },
            record: TransferRecord {
                schema: crate::transfer::TRANSFER_SCHEMA,
                transfer: chain[0].record.transfer.clone(),
                phase: TransferPhase::Aborted {
                    preparation: chain[0].evidence.clone(),
                },
            },
        }
    }

    fn release_testimony(chain: &[OwnedTestimony], aborted: &OwnedTestimony) -> OwnedTestimony {
        let TransferPhase::Accepted { import_digest, .. } = &chain[1].record.phase else {
            unreachable!()
        };
        OwnedTestimony {
            evidence: TransferEvidence {
                event_hash: "34".repeat(32),
                chain_seq: 4,
            },
            record: TransferRecord {
                schema: crate::transfer::TRANSFER_SCHEMA,
                transfer: chain[0].record.transfer.clone(),
                phase: TransferPhase::Released {
                    abort: aborted.evidence.clone(),
                    acceptance: chain[1].evidence.clone(),
                    import_digest: import_digest.clone(),
                },
            },
        }
    }

    fn observed_testimony(terminal: &OwnedTestimony) -> OwnedTestimony {
        OwnedTestimony {
            evidence: TransferEvidence {
                event_hash: "99".repeat(32),
                chain_seq: 5,
            },
            record: TransferRecord {
                schema: crate::transfer::TRANSFER_SCHEMA,
                transfer: terminal.record.transfer.clone(),
                phase: TransferPhase::SettlementObserved {
                    settlement: terminal.evidence.clone(),
                },
            },
        }
    }

    #[test]
    fn exact_retries_fold_to_one_frozen_reserved_then_active_lifecycle() {
        let chain = testimony_chain();
        let mut state = prepared_state(&chain);
        assert_eq!(state.source_state(), TransferSourceState::Frozen);
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::Unreserved
        );
        state.fold(chain[0].borrowed()).unwrap();
        state.fold(chain[1].borrowed()).unwrap();
        state.fold(chain[1].borrowed()).unwrap();
        assert_eq!(state.source_state(), TransferSourceState::Frozen);
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::Reserved
        );
        state.fold(chain[2].borrowed()).unwrap();
        assert_eq!(state.source_state(), TransferSourceState::Committed);
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::Reserved
        );
        state.fold(chain[3].borrowed()).unwrap();
        assert_eq!(state.destination_state(), TransferDestinationState::Active);
        assert!(state.prepared().unwrap().record == &chain[0].record);
        assert!(state.accepted().unwrap().evidence == &chain[1].evidence);
    }

    #[test]
    fn terminal_conflicts_order_gaps_and_reused_positions_fail_closed() {
        let chain = testimony_chain();
        let mut state = prepared_state(&chain);
        assert!(state.fold(chain[2].borrowed()).is_err());
        assert!(state.fold(chain[3].borrowed()).is_err());
        state.fold(chain[1].borrowed()).unwrap();
        let mut reserved_without_abort = state.clone();

        let abort = OwnedTestimony {
            evidence: TransferEvidence {
                event_hash: "ff".repeat(32),
                chain_seq: 3,
            },
            record: TransferRecord {
                schema: crate::transfer::TRANSFER_SCHEMA,
                transfer: chain[0].record.transfer.clone(),
                phase: TransferPhase::Aborted {
                    preparation: chain[0].evidence.clone(),
                },
            },
        };
        state.fold(abort.borrowed()).unwrap();
        assert_eq!(state.source_state(), TransferSourceState::Aborted);
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::Reserved
        );
        assert!(state.fold(chain[2].borrowed()).is_err());
        assert!(state.fold(chain[3].borrowed()).is_err());

        let mut conflicting = abort.clone();
        conflicting.evidence.event_hash = "12".repeat(32);
        assert!(state.fold(conflicting.borrowed()).is_err());

        let import_digest = match &chain[1].record.phase {
            TransferPhase::Accepted { import_digest, .. } => import_digest.clone(),
            _ => unreachable!(),
        };
        let released = OwnedTestimony {
            evidence: TransferEvidence {
                event_hash: "34".repeat(32),
                chain_seq: 4,
            },
            record: TransferRecord {
                schema: crate::transfer::TRANSFER_SCHEMA,
                transfer: chain[0].record.transfer.clone(),
                phase: TransferPhase::Released {
                    abort: abort.evidence.clone(),
                    acceptance: chain[1].evidence.clone(),
                    import_digest,
                },
            },
        };
        assert!(reserved_without_abort.fold(released.borrowed()).is_err());
        state.fold(released.borrowed()).unwrap();
        state.fold(released.borrowed()).unwrap();
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::Released
        );
        assert!(state.activated().is_none());

        let mut reused = testimony_chain();
        reused[1].evidence = reused[0].evidence.clone();
        let mut fresh = prepared_state(&reused);
        assert!(fresh.fold(reused[1].borrowed()).is_err());

        let chain = testimony_chain();
        let mut same_source_position = prepared_state(&chain);
        same_source_position.fold(chain[1].borrowed()).unwrap();
        let mut commit = chain[2].clone();
        commit.evidence.chain_seq = chain[0].evidence.chain_seq;
        commit.evidence.event_hash = "12".repeat(32);
        assert!(same_source_position.fold(commit.borrowed()).is_err());
    }

    #[test]
    fn producer_replacement_does_not_thaw_retained_source_state() {
        let chain = testimony_chain();
        let mut state = prepared_state(&chain);
        state.fold(chain[1].borrowed()).unwrap();
        let rebuilt = state.clone();
        assert_eq!(rebuilt.source_state(), TransferSourceState::Frozen);
        assert_eq!(
            rebuilt.destination_state(),
            TransferDestinationState::Reserved
        );

        let mut replacement_identity = chain[1].clone();
        replacement_identity
            .record
            .transfer
            .source
            .producer_incarnation = "ef".repeat(32);
        assert!(state.fold(replacement_identity.borrowed()).is_err());
        assert_eq!(state.source_state(), TransferSourceState::Frozen);
    }

    #[test]
    fn retained_data_settles_a_commit_without_activation_or_a_new_source_decision() {
        let chain = testimony_chain();
        let retained = retained_testimony(&chain[3]);
        let mut state = prepared_state(&chain);
        assert!(state.fold(retained.borrowed()).is_err());
        state.fold(chain[1].borrowed()).unwrap();
        assert!(state.fold(retained.borrowed()).is_err());
        state.fold(chain[2].borrowed()).unwrap();
        state.fold(retained.borrowed()).unwrap();
        state.fold(retained.borrowed()).unwrap();
        assert_eq!(state.source_state(), TransferSourceState::Committed);
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::Retained
        );
        assert_eq!(state.retained().unwrap().evidence, &retained.evidence);
        assert!(state.activated().is_none());
        assert!(state.settlement_observed().is_none());

        let before_conflicts = state.clone();
        assert!(state.fold(chain[3].borrowed()).is_err());
        let aborted = abort_testimony(&chain);
        assert!(state.fold(aborted.borrowed()).is_err());
        assert!(
            state
                .fold(release_testimony(&chain, &aborted).borrowed())
                .is_err()
        );
        assert_eq!(state, before_conflicts);

        let observed = observed_testimony(&retained);
        state.fold(observed.borrowed()).unwrap();
        // An exact retry reports the same historical record, not new authority.
        for testimony in [&chain[0], &chain[1], &chain[2], &retained, &observed] {
            state.fold(testimony.borrowed()).unwrap();
        }
        assert_eq!(state.source_state(), TransferSourceState::Committed);
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::Retained
        );
    }

    #[test]
    fn historical_activation_cannot_be_relabelled_as_retained_data() {
        let chain = testimony_chain();
        let mut state = prepared_state(&chain);
        for testimony in &chain[1..] {
            state.fold(testimony.borrowed()).unwrap();
        }
        let activated = state.clone();
        assert!(
            state
                .fold(retained_testimony(&chain[3]).borrowed())
                .is_err()
        );
        assert_eq!(state, activated);
        state
            .fold(observed_testimony(&chain[3]).borrowed())
            .unwrap();
        assert_eq!(state.destination_state(), TransferDestinationState::Active);
        assert!(state.retained().is_none());
        assert!(state.settlement_observed().is_some());
    }

    #[test]
    fn observation_requires_the_exact_already_folded_terminal_for_every_outcome() {
        let chain = testimony_chain();
        let aborted = abort_testimony(&chain);
        let retained = retained_testimony(&chain[3]);
        let released = release_testimony(&chain, &aborted);
        for terminal in [&chain[3], &retained, &released] {
            let mut state = prepared_state(&chain);
            state.fold(chain[1].borrowed()).unwrap();
            if matches!(&terminal.record.phase, TransferPhase::Released { .. }) {
                state.fold(aborted.borrowed()).unwrap();
            } else {
                state.fold(chain[2].borrowed()).unwrap();
            }
            let observed = observed_testimony(terminal);
            let before_terminal = state.clone();
            assert!(state.fold(observed.borrowed()).is_err());
            assert_eq!(state, before_terminal);
            state.fold(terminal.borrowed()).unwrap();
            let before_observation = state.clone();
            for change_hash in [false, true] {
                let mut wrong_link = observed.clone();
                let TransferPhase::SettlementObserved { settlement } = &mut wrong_link.record.phase
                else {
                    unreachable!()
                };
                if change_hash {
                    settlement.event_hash = "12".repeat(32);
                } else {
                    settlement.chain_seq += 1;
                }
                assert!(state.fold(wrong_link.borrowed()).is_err());
                assert_eq!(state, before_observation);
            }
            state.fold(observed.borrowed()).unwrap();
            state.fold(observed.borrowed()).unwrap();
            assert_eq!(state.source_state(), before_observation.source_state());
            assert_eq!(
                state.destination_state(),
                before_observation.destination_state()
            );
            assert_eq!(
                state.settlement_observed().unwrap().evidence,
                &observed.evidence
            );
            let complete = state.clone();
            let mut competing = observed.clone();
            competing.evidence.event_hash = "12".repeat(32);
            competing.evidence.chain_seq += 1;
            assert!(state.fold(competing.borrowed()).is_err());
            assert_eq!(state, complete);
        }
    }

    #[test]
    fn abort_without_acceptance_invents_neither_release_nor_observation() {
        let chain = testimony_chain();
        let aborted = abort_testimony(&chain);
        let mut state = prepared_state(&chain);
        state.fold(aborted.borrowed()).unwrap();
        let before = state.clone();
        assert!(state.fold(chain[1].borrowed()).is_err());
        assert!(
            state
                .fold(release_testimony(&chain, &aborted).borrowed())
                .is_err()
        );
        assert!(state.fold(observed_testimony(&aborted).borrowed()).is_err());
        assert_eq!(state, before);
        assert_eq!(state.source_state(), TransferSourceState::Aborted);
        assert_eq!(
            state.destination_state(),
            TransferDestinationState::Unreserved
        );
        assert!(state.settlement_observed().is_none());
        // Proving authoritative absence of a reservation for retirement remains
        // the lifecycle owner's job; this caller-supplied fold cannot prove it.
    }

    #[test]
    fn settlement_records_cannot_reuse_hashes_or_same_owner_positions() {
        let chain = testimony_chain();
        let retained = retained_testimony(&chain[3]);
        let mut state = prepared_state(&chain);
        state.fold(chain[1].borrowed()).unwrap();
        state.fold(chain[2].borrowed()).unwrap();
        for reuse_hash in [false, true] {
            let mut collision = retained.clone();
            if reuse_hash {
                collision.evidence.event_hash = chain[0].evidence.event_hash.clone();
            } else {
                collision.evidence.chain_seq = chain[1].evidence.chain_seq;
            }
            assert!(state.fold(collision.borrowed()).is_err());
        }
        state.fold(retained.borrowed()).unwrap();
        let observed = observed_testimony(&retained);
        for reuse_hash in [false, true] {
            let mut collision = observed.clone();
            if reuse_hash {
                collision.evidence.event_hash = retained.evidence.event_hash.clone();
            } else {
                collision.evidence.chain_seq = chain[2].evidence.chain_seq;
            }
            assert!(state.fold(collision.borrowed()).is_err());
        }
        state.fold(observed.borrowed()).unwrap();
    }
}
