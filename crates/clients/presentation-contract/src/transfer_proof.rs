//! Pure cross-record consistency checks, NOT authorization or settlement.
//! The caller must verify retained CAS bytes against each supplied event hash,
//! verify its chain position, and decode exact outer event ownership first.
//! These functions perform no I/O, admission, reservation, edit or publication.
use crate::transfer::{TransferEvidence, TransferPhase, TransferRecord};

/// Borrowed caller-supplied testimony. This type does not certify CAS verification.
#[derive(Debug, Clone, Copy)]
pub struct TransferTestimony<'a> {
    pub evidence: &'a TransferEvidence,
    pub record: &'a TransferRecord,
}

fn matching(
    record: &TransferRecord,
    testimony: TransferTestimony<'_>,
    link: &TransferEvidence,
) -> Result<(), String> {
    record.validate_shape()?;
    testimony.record.validate_shape()?;
    if testimony.evidence.event_hash.len() != 64
        || !testimony
            .evidence
            .event_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("caller-supplied testimony hash is not canonical".into());
    }
    if record.transfer != testimony.record.transfer || link != testimony.evidence {
        return Err("transfer testimony identity or exact hash/chain position differs".into());
    }
    Ok(())
}

/// The source preparation must name the exact destination-owned visibility
/// event. This is consistency only; the caller authenticates both envelopes.
pub fn verify_prepared(
    record: &TransferRecord,
    inbound: TransferTestimony<'_>,
) -> Result<(), String> {
    let TransferPhase::Prepared { inbound: link, .. } = &record.phase else {
        return Err("expected source preparation testimony".into());
    };
    matching(record, inbound, link)?;
    if !matches!(&inbound.record.phase, TransferPhase::InboundCommitment) {
        return Err("preparation does not reference destination inbound commitment".into());
    }
    Ok(())
}

/// Only an exact destination inbound event can be cancelled. The caller must
/// establish the absence of a source preparation under its publication fence.
pub fn verify_orphan_cancelled(
    record: &TransferRecord,
    inbound: TransferTestimony<'_>,
) -> Result<(), String> {
    let TransferPhase::OrphanCancelled { inbound: link } = &record.phase else {
        return Err("expected destination orphan cancellation testimony".into());
    };
    matching(record, inbound, link)?;
    if !matches!(&inbound.record.phase, TransferPhase::InboundCommitment) {
        return Err("orphan cancellation does not reference destination inbound commitment".into());
    }
    Ok(())
}

pub fn verify_accepted(
    record: &TransferRecord,
    prepared: TransferTestimony<'_>,
) -> Result<(), String> {
    let TransferPhase::Accepted { preparation, .. } = &record.phase else {
        return Err("expected destination acceptance testimony".into());
    };
    matching(record, prepared, preparation)?;
    if !matches!(&prepared.record.phase, TransferPhase::Prepared { .. }) {
        return Err("acceptance does not reference source preparation".into());
    }
    Ok(())
}

pub fn verify_committed(
    record: &TransferRecord,
    prepared: TransferTestimony<'_>,
    accepted: TransferTestimony<'_>,
) -> Result<(), String> {
    let TransferPhase::Committed {
        preparation,
        acceptance,
        import_digest,
    } = &record.phase
    else {
        return Err("expected source commit testimony".into());
    };
    matching(record, prepared, preparation)?;
    matching(record, accepted, acceptance)?;
    verify_accepted(accepted.record, prepared)?;
    let TransferPhase::Accepted {
        import_digest: accepted_digest,
        ..
    } = &accepted.record.phase
    else {
        unreachable!("verified acceptance phase")
    };
    if import_digest != accepted_digest {
        return Err("commit changed the exact destination import commitment".into());
    }
    Ok(())
}

pub fn verify_activated(
    record: &TransferRecord,
    committed: TransferTestimony<'_>,
) -> Result<(), String> {
    let TransferPhase::Activated {
        commit,
        import_digest,
    } = &record.phase
    else {
        return Err("expected destination activation testimony".into());
    };
    matching(record, committed, commit)?;
    let TransferPhase::Committed {
        import_digest: committed_digest,
        ..
    } = &committed.record.phase
    else {
        return Err("activation does not reference the exact source commit".into());
    };
    if import_digest != committed_digest {
        return Err("activation changed the committed import commitment".into());
    }
    Ok(())
}

/// Checks the historical import commitment only. It neither proves the seat's
/// producer was fenced nor restores the committed data as live attachments.
pub fn verify_retained(
    record: &TransferRecord,
    committed: TransferTestimony<'_>,
) -> Result<(), String> {
    let TransferPhase::Retained {
        commit,
        import_digest,
    } = &record.phase
    else {
        return Err("expected destination retained-data testimony".into());
    };
    matching(record, committed, commit)?;
    let TransferPhase::Committed {
        import_digest: committed_digest,
        ..
    } = &committed.record.phase
    else {
        return Err("retained data does not reference the exact source commit".into());
    };
    if import_digest != committed_digest {
        return Err("retained data changed the committed import commitment".into());
    }
    Ok(())
}

/// Checks the exact source-to-destination terminal testimony link. The caller
/// must separately verify the terminal's ancestry and current canonical phase.
/// This is neither a second decision nor a head-retirement eligibility proof.
pub fn verify_settlement_observed(
    record: &TransferRecord,
    terminal: TransferTestimony<'_>,
) -> Result<(), String> {
    let TransferPhase::SettlementObserved { settlement } = &record.phase else {
        return Err("expected source settlement-observation testimony".into());
    };
    matching(record, terminal, settlement)?;
    if !matches!(
        &terminal.record.phase,
        TransferPhase::Activated { .. }
            | TransferPhase::Retained { .. }
            | TransferPhase::Released { .. }
    ) {
        return Err("observation does not reference exact destination terminal testimony".into());
    }
    Ok(())
}

/// Source abort references preparation only. This cannot prove absence of a
/// competing commit; the existing source-seat admission owner must settle that.
pub fn verify_aborted(
    record: &TransferRecord,
    prepared: TransferTestimony<'_>,
) -> Result<(), String> {
    let TransferPhase::Aborted { preparation } = &record.phase else {
        return Err("expected source abort testimony".into());
    };
    matching(record, prepared, preparation)?;
    if !matches!(&prepared.record.phase, TransferPhase::Prepared { .. }) {
        return Err("abort does not reference source preparation".into());
    }
    Ok(())
}

pub fn verify_released(
    record: &TransferRecord,
    aborted: TransferTestimony<'_>,
    accepted: TransferTestimony<'_>,
) -> Result<(), String> {
    let TransferPhase::Released {
        abort,
        acceptance,
        import_digest,
    } = &record.phase
    else {
        return Err("expected destination release testimony".into());
    };
    matching(record, aborted, abort)?;
    matching(record, accepted, acceptance)?;
    if !matches!(&aborted.record.phase, TransferPhase::Aborted { .. }) {
        return Err("release does not reference the exact source abort".into());
    }
    let TransferPhase::Accepted {
        import_digest: accepted_digest,
        ..
    } = &accepted.record.phase
    else {
        return Err("release does not reference the exact destination acceptance".into());
    };
    if import_digest != accepted_digest {
        return Err("release changed the accepted import commitment".into());
    }
    Ok(())
}
