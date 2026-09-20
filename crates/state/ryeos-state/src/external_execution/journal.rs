//! Durable occurrence-scoped transcript rules shared by node and supervisor.
//!
//! The caller owns the SQLite transaction and its commit. In particular commit
//! terminal revocation independently before a contiguous append can fail.
//! Claims are one-shot intent, not continuing authority to write to a process:
//! the dispatcher must serialize actual delivery against sticky revocation.
//! Reopening validates evidence only; it never recreates or releases a launcher.

use super::transcript::{ChannelPhase, FrameFrontier, urgent_control};
use super::{
    AuthenticatedExecutionFrame, ChannelDirection, ExecutionChannelBinding, ExecutionChannelBudget,
    ExecutionChannelPayload, ExecutionFrame, ExecutionFrameApplication, SignedExecutionFrame,
    TERMINAL_CONTROL_BYTES,
};
use anyhow::{Context as _, Result, bail};
use lillux::crypto::SigningKey;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Common tables and immutable transitions. Each owner additionally protects
/// deletion and stores its own retention/cleanup evidence; this is not a node
/// runtime schema, bootstrap admission, or a complete guest database.
pub const CHANNEL_SQL: &str = r#"
CREATE TABLE external_execution_channel (
    placement_thread_id TEXT PRIMARY KEY,
    binding_digest TEXT NOT NULL UNIQUE,
    binding_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN
        ('prepared','ready','running','quiescing','exported','stopping','stopped')),
    completion_request_digest TEXT,
    export_snapshot_hash TEXT,
    export_evidence_hash TEXT,
    CHECK ((export_snapshot_hash IS NULL AND export_evidence_hash IS NULL)
        OR (export_snapshot_hash IS NOT NULL AND export_evidence_hash IS NOT NULL
            AND completion_request_digest IS NOT NULL))
);

CREATE TABLE external_execution_frame (
    binding_digest TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('owner_to_supervisor','supervisor_to_owner')),
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    frame_digest TEXT NOT NULL,
    frame_json TEXT NOT NULL,
    frame_bytes INTEGER NOT NULL CHECK (frame_bytes > 0),
    acknowledged_peer_sequence INTEGER NOT NULL CHECK (acknowledged_peer_sequence >= 0),
    application TEXT NOT NULL CHECK (application IN ('pending','claimed','applied','revoked')),
    PRIMARY KEY(binding_digest,direction,sequence),
    UNIQUE(binding_digest,ordinal)
);

CREATE TABLE external_execution_revocation (
    binding_digest TEXT PRIMARY KEY,
    frame_digest TEXT NOT NULL,
    frame_json TEXT NOT NULL
);

CREATE TRIGGER external_execution_revocation_no_update
BEFORE UPDATE ON external_execution_revocation
BEGIN SELECT RAISE(ABORT, 'external revocation is sticky and immutable'); END;
CREATE TRIGGER external_execution_revocation_no_delete
BEFORE DELETE ON external_execution_revocation
BEGIN SELECT RAISE(ABORT, 'external revocation requires explicit cleanup retention handoff'); END;
CREATE TRIGGER external_execution_channel_no_rebinding
BEFORE UPDATE ON external_execution_channel
WHEN NEW.placement_thread_id != OLD.placement_thread_id
 OR NEW.binding_digest != OLD.binding_digest OR NEW.binding_json != OLD.binding_json
BEGIN SELECT RAISE(ABORT, 'external channel cannot change its occurrence or keys'); END;
CREATE TRIGGER external_execution_channel_initial_state
BEFORE INSERT ON external_execution_channel
WHEN NEW.state!='prepared' OR NEW.completion_request_digest IS NOT NULL
 OR NEW.export_snapshot_hash IS NOT NULL OR NEW.export_evidence_hash IS NOT NULL
BEGIN SELECT RAISE(ABORT, 'external channel must begin prepared without a projection'); END;
CREATE TRIGGER external_execution_frame_initial_application
BEFORE INSERT ON external_execution_frame WHEN NEW.application!='pending'
BEGIN SELECT RAISE(ABORT, 'external frame must begin pending'); END;
CREATE TRIGGER external_execution_frame_immutable
BEFORE UPDATE ON external_execution_frame
WHEN NEW.binding_digest != OLD.binding_digest OR NEW.direction != OLD.direction
 OR NEW.sequence != OLD.sequence OR NEW.frame_digest != OLD.frame_digest
 OR NEW.ordinal != OLD.ordinal
 OR NEW.frame_json != OLD.frame_json OR NEW.frame_bytes != OLD.frame_bytes
 OR NEW.acknowledged_peer_sequence != OLD.acknowledged_peer_sequence
 OR NOT ((OLD.application='pending' AND NEW.application='claimed')
     OR (OLD.application='pending' AND NEW.application='revoked'
         AND COALESCE(json_extract(OLD.frame_json,'$.frame.payload.kind') IN ('release','protocol_bytes','protocol_eof'),0))
     OR (OLD.application='claimed' AND NEW.application='applied'))
BEGIN SELECT RAISE(ABORT, 'external frame cannot be rewritten or reapplied'); END;
"#;

/// Protected owner-specific checks; none has a permissive default. Shared
/// authentication, ordering, revocation and deadline checks cannot be replaced.
/// Implementations query the supplied connection (the caller's transaction),
/// never reconstruct authority from project files or another database.
pub trait JournalOwner {
    /// Check exact retained authority, including duplicate/idempotent requests.
    /// This must allow historical evidence under a quarantined/expired owner.
    fn require_owner(&self, conn: &Connection, binding: &ExecutionChannelBinding) -> Result<()>;
    /// Additional owner gate for newly retained frames and pending claims.
    /// Not used to acknowledge an already claimed historical delivery.
    fn authorize_frame(
        &self,
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        payload: &ExecutionChannelPayload,
    ) -> Result<()>;
    /// Exact durable export retention, not a transport acknowledgement or a
    /// caller success flag. Node imports and supervisor exports have different
    /// retention authorities; each must implement its own check.
    fn require_export_retention(
        &self,
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        frame_digest: &str,
        candidate_snapshot_hash: &str,
        completion_request_digest: &str,
        writer_exclusion_evidence_hash: &str,
    ) -> Result<()>;

    /// Resolve an authenticated application retained outside the contiguous
    /// frame table. This exists only for sticky terminal revocation that may
    /// cross a missing predecessor; ordinary frames must return `None`.
    fn out_of_band_application_state(
        &self,
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        direction: ChannelDirection,
        sequence: u64,
        digest: &str,
    ) -> Result<Option<ExecutionFrameApplication>>;

    /// True only when this journal is the destination-side application owner,
    /// so `pending` proves bytes were never claimed locally. A source-side
    /// controller must return false: missing peer evidence is uncertainty, and
    /// a late exact Applied acknowledgement remains admissible after cancel.
    fn can_prove_pending_input_revoked(&self) -> bool;
}

/// Exact durable result of trying to acquire one retained application.
///
/// Only [`ApplicationClaim::New`] authorizes a caller to cross an execution
/// boundary. `AlreadyClaimed` is deliberately uncertainty, not idempotent
/// dispatch permission; `AlreadyApplied` and `Revoked` are retained history.
pub enum ApplicationClaim {
    New(AuthenticatedExecutionFrame),
    AlreadyClaimed,
    AlreadyApplied,
    Revoked,
}

/// One exact retained outbound frame eligible for transport replay. The bytes
/// are the original canonical signed wire representation; callers must never
/// reconstruct them from fields.
pub struct PendingExecutionFrame {
    direction: ChannelDirection,
    sequence: u64,
    digest: String,
    wire: Vec<u8>,
}

impl PendingExecutionFrame {
    pub fn direction(&self) -> ChannelDirection {
        self.direction
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn wire(&self) -> &[u8] {
        &self.wire
    }
}

impl ApplicationClaim {
    /// Compatibility projection for owners whose established API reports only
    /// whether this call acquired a new claim. An execution dispatcher must
    /// instead consume the authenticated frame carried by `New`.
    pub fn is_new(&self) -> bool {
        matches!(self, Self::New(_))
    }
}

/// Stage sticky terminal revocation. Commit this transaction before attempting
/// a potentially failing contiguous append; failure must never reopen input.
pub fn record_revocation(
    tx: &Transaction<'_>,
    owner: &impl JournalOwner,
    placement: &str,
    wire: &[u8],
) -> Result<bool> {
    let binding = load_binding(tx, placement)?;
    let verified =
        SignedExecutionFrame::decode_and_verify(wire, &binding, lillux::time::timestamp_millis())?;
    if verified.frame().direction != ChannelDirection::OwnerToSupervisor
        || !matches!(verified.frame().payload, ExecutionChannelPayload::Cancel)
        || wire.len() as u64 > TERMINAL_CONTROL_BYTES
    {
        bail!("external fast path accepts only bounded owner revocation");
    }
    owner.require_owner(tx, &binding)?;
    let prior: Option<(String, String)> = tx
        .query_row(
            "SELECT frame_digest,frame_json
        FROM external_execution_revocation WHERE binding_digest=?1",
            [binding.digest()?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some(prior) = prior {
        if prior.0 != verified.digest() || prior.1 != verified.canonical() {
            bail!("external cancellation changed its exact retained frame");
        }
        return Ok(false);
    }
    owner.authorize_frame(tx, &binding, &verified.frame().payload)?;
    tx.execute(
        "INSERT INTO external_execution_revocation VALUES(?1,?2,?3)",
        params![binding.digest()?, verified.digest(), verified.canonical()],
    )?;
    if owner.can_prove_pending_input_revoked() {
        tx.execute(
            "UPDATE external_execution_frame SET application='revoked'
            WHERE binding_digest=?1 AND application='pending'
            AND json_extract(frame_json,'$.frame.payload.kind') IN ('release','protocol_bytes','protocol_eof')",
            [binding.digest()?],
        )?;
    }
    Ok(true)
}

/// Retain one authenticated frame. False is an exact duplicate, not permission
/// to deliver its bytes again. Cancellation requires prior sticky revocation.
pub fn append_frame(
    tx: &Transaction<'_>,
    owner: &impl JournalOwner,
    placement: &str,
    wire: &[u8],
) -> Result<bool> {
    let binding = load_binding(tx, placement)?;
    let now = lillux::time::timestamp_millis();
    let verified = SignedExecutionFrame::decode_and_verify(wire, &binding, now)?;
    let frame = verified.frame();
    if matches!(frame.payload, ExecutionChannelPayload::Cancel) {
        let retained: Option<String> = tx
            .query_row(
                "SELECT frame_digest FROM external_execution_revocation WHERE binding_digest=?1",
                [&frame.binding_digest],
                |row| row.get(0),
            )
            .optional()?;
        if retained.as_deref() != Some(verified.digest()) {
            bail!("commit exact sticky revocation before appending cancellation");
        }
    }
    owner.require_owner(tx, &binding)?;
    let direction = frame.direction.as_str();
    let prior: Option<(String, String, i64, i64)> = tx
        .query_row(
            "SELECT frame_digest,frame_json,frame_bytes,acknowledged_peer_sequence
             FROM external_execution_frame
         WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
            params![
                frame.binding_digest,
                direction,
                i64::try_from(frame.sequence)?
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    if let Some(prior) = prior {
        if prior.0 != verified.digest()
            || prior.1 != verified.canonical()
            || prior.2 != i64::try_from(wire.len())?
            || prior.3 != i64::try_from(frame.acknowledged_peer_sequence)?
        {
            bail!("external frame sequence was reused with different bytes");
        }
        return Ok(false);
    }
    let last: Option<(i64, String, i64)> = tx
        .query_row(
            "SELECT sequence,frame_digest,acknowledged_peer_sequence FROM external_execution_frame
         WHERE binding_digest=?1 AND direction=?2 ORDER BY sequence DESC LIMIT 1",
            params![frame.binding_digest, direction],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (sequence, predecessor, last_ack) = match last {
        None => (0, None, 0),
        Some((sequence, digest, ack)) => (sequence, Some(digest), ack),
    };
    let peer_sequence: i64 = tx.query_row(
        "SELECT COALESCE(MAX(sequence),0) FROM external_execution_frame WHERE binding_digest=?1 AND direction=?2",
        params![frame.binding_digest, frame.direction.opposite().as_str()], |row|row.get(0))?;
    FrameFrontier::from_retained(
        u64::try_from(sequence)?,
        predecessor,
        u64::try_from(last_ack)?,
    )?
    .require_successor(frame, u64::try_from(peer_sequence)?)?;
    let mut budget = retained_budget(tx, &frame.binding_digest, direction)?;
    budget.retain(&binding, &frame.payload, wire.len() as u64)?;
    let (state, completion): (String, Option<String>) = tx.query_row(
        "SELECT state,completion_request_digest FROM external_execution_channel WHERE placement_thread_id=?1",
        [placement], |row|Ok((row.get(0)?,row.get(1)?)))?;
    owner.authorize_frame(tx, &binding, &frame.payload)?;
    let next = ChannelPhase::parse(&state)?.advance(
        completion.as_deref(),
        &frame.payload,
        now < binding.execution_deadline_ms,
    )?;
    let ordinal: i64 = tx.query_row(
        "SELECT COALESCE(MAX(ordinal),0)+1 FROM external_execution_frame WHERE binding_digest=?1",
        [&frame.binding_digest],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO external_execution_frame VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'pending')",
        params![
            frame.binding_digest,
            direction,
            i64::try_from(frame.sequence)?,
            ordinal,
            verified.digest(),
            verified.canonical(),
            i64::try_from(wire.len())?,
            i64::try_from(frame.acknowledged_peer_sequence)?
        ],
    )?;
    if let ExecutionChannelPayload::Acknowledge {
        peer_frame_sequence,
        peer_frame_digest,
        application,
    } = &frame.payload
    {
        reconcile_peer_application(
            tx,
            owner,
            &binding,
            &frame.binding_digest,
            frame.direction.opposite(),
            *peer_frame_sequence,
            peer_frame_digest,
            *application,
        )?;
    }
    tx.execute(
        "UPDATE external_execution_channel SET state=?2 WHERE placement_thread_id=?1",
        params![placement, next.as_str()],
    )?;
    if revoked(tx, &frame.binding_digest)? && owner.can_prove_pending_input_revoked() {
        tx.execute(
            "UPDATE external_execution_frame SET application='revoked'
            WHERE binding_digest=?1 AND application='pending'
            AND json_extract(frame_json,'$.frame.payload.kind') IN ('release','protocol_bytes','protocol_eof')",
            [&frame.binding_digest],
        )?;
    }
    match &frame.payload {
        ExecutionChannelPayload::Cancel | ExecutionChannelPayload::Stopped { .. }
            if owner.can_prove_pending_input_revoked() =>
        {
            // Pending input has provably not been applied. Claimed input
            // remains unknown; cancellation may overtake it but cannot
            // relabel it as uncontacted or completed.
            tx.execute("UPDATE external_execution_frame SET application='revoked'
                WHERE binding_digest=?1 AND application='pending'
                AND json_extract(frame_json,'$.frame.payload.kind') IN ('release','protocol_bytes','protocol_eof')",
                [&frame.binding_digest])?;
        }
        ExecutionChannelPayload::Quiesce {
            completion_request_digest,
        } => {
            tx.execute("UPDATE external_execution_channel SET completion_request_digest=?2 WHERE placement_thread_id=?1",
                params![placement,completion_request_digest])?;
        }
        ExecutionChannelPayload::ExportSealed {
            candidate_snapshot_hash,
            writer_exclusion_evidence_hash,
            ..
        } => {
            tx.execute("UPDATE external_execution_channel SET export_snapshot_hash=?2,export_evidence_hash=?3 WHERE placement_thread_id=?1",
                params![placement,candidate_snapshot_hash,writer_exclusion_evidence_hash])?;
        }
        _ => {}
    }
    Ok(true)
}

fn reconcile_peer_application(
    tx: &Transaction<'_>,
    owner: &impl JournalOwner,
    binding: &ExecutionChannelBinding,
    binding_digest: &str,
    direction: ChannelDirection,
    sequence: u64,
    digest: &str,
    reported: ExecutionFrameApplication,
) -> Result<()> {
    let retained: Option<(String, String, String)> = tx
        .query_row(
            "SELECT frame_digest,application,json_extract(frame_json,'$.frame.payload.kind')
         FROM external_execution_frame
         WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
            params![binding_digest, direction.as_str(), i64::try_from(sequence)?],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((stored_digest, mut state, payload_kind)) = retained else {
        let actual = owner
            .out_of_band_application_state(tx, binding, direction, sequence, digest)?
            .context("external acknowledgement target is not retained")?;
        if !application_state_proves(actual, reported) {
            bail!("external acknowledgement exceeds terminal application evidence");
        }
        return Ok(());
    };
    if stored_digest != digest {
        bail!("external application acknowledgement changed its peer frame");
    }
    match reported {
        ExecutionFrameApplication::Retained => {
            if state != "pending" {
                bail!("external application acknowledgement regressed from retained state");
            }
        }
        ExecutionFrameApplication::Claimed => match state.as_str() {
            "pending" => {
                tx.execute(
                    "UPDATE external_execution_frame SET application='claimed'
                     WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
                    params![binding_digest, direction.as_str(), i64::try_from(sequence)?],
                )?;
            }
            "claimed" => {}
            _ => bail!("external application acknowledgement regressed or reopened a frame"),
        },
        ExecutionFrameApplication::Applied => {
            if state == "pending" {
                tx.execute(
                    "UPDATE external_execution_frame SET application='claimed'
                     WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
                    params![binding_digest, direction.as_str(), i64::try_from(sequence)?],
                )?;
                state = "claimed".to_owned();
            }
            match state.as_str() {
                "claimed" => {
                    tx.execute(
                        "UPDATE external_execution_frame SET application='applied'
                         WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
                        params![binding_digest, direction.as_str(), i64::try_from(sequence)?],
                    )?;
                }
                "applied" => {}
                _ => bail!("external applied acknowledgement reopened a revoked frame"),
            }
        }
        ExecutionFrameApplication::Revoked => {
            if !matches!(
                payload_kind.as_str(),
                "release" | "protocol_bytes" | "protocol_eof"
            ) {
                bail!("external revocation acknowledgement named a non-input frame");
            }
            match state.as_str() {
                "pending" => {
                    tx.execute(
                        "UPDATE external_execution_frame SET application='revoked'
                         WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
                        params![binding_digest, direction.as_str(), i64::try_from(sequence)?],
                    )?;
                }
                "revoked" => {}
                _ => bail!("external revocation acknowledgement changed uncertain application"),
            }
        }
    }
    Ok(())
}

/// Author and retain one exact directional frame inside the caller's writer
/// transaction. A commit error is uncertain publication: recover the retained
/// frontier rather than guessing a successor.
pub fn author_frame(
    tx: &Transaction<'_>,
    owner: &impl JournalOwner,
    placement: &str,
    direction: ChannelDirection,
    signing_key: &SigningKey,
    payload: ExecutionChannelPayload,
) -> Result<AuthenticatedExecutionFrame> {
    let prepared = prepare_frame(tx, owner, placement, direction, signing_key, payload)?;
    if !append_frame(tx, owner, placement, prepared.canonical().as_bytes())? {
        bail!("fresh external frame unexpectedly duplicated");
    }
    Ok(prepared)
}

/// Prepare the exact next signed frame while holding the journal writer. This
/// is separated from append solely so terminal revocation can be committed in
/// an independent transaction before its gap-sensitive transcript append.
pub fn prepare_frame(
    tx: &Transaction<'_>,
    owner: &impl JournalOwner,
    placement: &str,
    direction: ChannelDirection,
    signing_key: &SigningKey,
    payload: ExecutionChannelPayload,
) -> Result<AuthenticatedExecutionFrame> {
    let binding = load_binding(tx, placement)?;
    owner.require_owner(tx, &binding)?;
    let binding_digest = binding.digest()?;
    let prior: Option<(i64, String)> = tx
        .query_row(
            "SELECT sequence,frame_digest FROM external_execution_frame
             WHERE binding_digest=?1 AND direction=?2
             ORDER BY sequence DESC LIMIT 1",
            params![binding_digest, direction.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (sequence, previous_frame_digest) = match prior {
        Some((sequence, digest)) => (
            u64::try_from(sequence)?
                .checked_add(1)
                .context("external frame sequence overflow")?,
            Some(digest),
        ),
        None => (1, None),
    };
    let acknowledged_peer_sequence: i64 = tx.query_row(
        "SELECT COALESCE(MAX(sequence),0) FROM external_execution_frame
         WHERE binding_digest=?1 AND direction=?2",
        params![binding_digest, direction.opposite().as_str()],
        |row| row.get(0),
    )?;
    let signed = SignedExecutionFrame::sign(
        ExecutionFrame {
            schema: 1,
            binding_digest,
            direction,
            sequence,
            previous_frame_digest,
            acknowledged_peer_sequence: u64::try_from(acknowledged_peer_sequence)?,
            payload,
        },
        &binding,
        signing_key,
    )?;
    let wire = lillux::canonical_json(&serde_json::to_value(signed)?)?.into_bytes();
    SignedExecutionFrame::decode_and_verify(&wire, &binding, lillux::time::timestamp_millis())
}

/// Recover or author an exact signed acknowledgement of the peer frame's
/// current durable application state. Node ingress suppresses ack-of-ack;
/// the outbound supervisor may emit one to advance its cumulative receive
/// frontier and then reuse that exact frame for polling.
pub fn ensure_application_acknowledgement(
    tx: &Transaction<'_>,
    owner: &impl JournalOwner,
    placement: &str,
    acknowledgement_direction: ChannelDirection,
    peer_sequence: u64,
    peer_digest: &str,
    signing_key: &SigningKey,
    acknowledge_acknowledgement: bool,
) -> Result<Option<AuthenticatedExecutionFrame>> {
    let binding = load_binding(tx, placement)?;
    let binding_digest = binding.digest()?;
    let peer_direction = acknowledgement_direction.opposite();
    let (stored_digest, application, payload_kind): (String, String, String) = tx.query_row(
        "SELECT frame_digest,application,json_extract(frame_json,'$.frame.payload.kind')
         FROM external_execution_frame
         WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
        params![
            binding_digest,
            peer_direction.as_str(),
            i64::try_from(peer_sequence)?
        ],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if stored_digest != peer_digest {
        bail!("external acknowledgement changed its peer frame");
    }
    if payload_kind == "acknowledge" && !acknowledge_acknowledgement {
        return Ok(None);
    }
    let application = match application.as_str() {
        "pending" => ExecutionFrameApplication::Retained,
        "claimed" => ExecutionFrameApplication::Claimed,
        "applied" => ExecutionFrameApplication::Applied,
        "revoked" => ExecutionFrameApplication::Revoked,
        _ => bail!("external peer frame has unknown application state"),
    };
    ensure_application_acknowledgement_for_state(
        tx,
        owner,
        placement,
        acknowledgement_direction,
        peer_sequence,
        peer_digest,
        application,
        signing_key,
    )
}

/// Recover or author exact application evidence when the target is retained in
/// an owner-specific terminal journal rather than the contiguous transcript.
/// The caller must authenticate the exact target and state before invoking it.
pub fn ensure_application_acknowledgement_for_state(
    tx: &Transaction<'_>,
    owner: &impl JournalOwner,
    placement: &str,
    acknowledgement_direction: ChannelDirection,
    peer_sequence: u64,
    peer_digest: &str,
    application: ExecutionFrameApplication,
    signing_key: &SigningKey,
) -> Result<Option<AuthenticatedExecutionFrame>> {
    let binding = load_binding(tx, placement)?;
    owner.require_owner(tx, &binding)?;
    super::hash(peer_digest)?;
    if peer_sequence == 0 {
        bail!("external acknowledgement target sequence is zero");
    }
    let binding_digest = binding.digest()?;
    let application_name = match application {
        ExecutionFrameApplication::Retained => "retained",
        ExecutionFrameApplication::Claimed => "claimed",
        ExecutionFrameApplication::Applied => "applied",
        ExecutionFrameApplication::Revoked => "revoked",
    };
    let existing: Option<String> = tx
        .query_row(
            "SELECT frame_json FROM external_execution_frame
             WHERE binding_digest=?1 AND direction=?2
             AND json_extract(frame_json,'$.frame.payload.kind')='acknowledge'
             AND json_extract(frame_json,'$.frame.payload.peer_frame_sequence')=?3
             AND json_extract(frame_json,'$.frame.payload.peer_frame_digest')=?4
             AND json_extract(frame_json,'$.frame.payload.application')=?5
             ORDER BY sequence ASC LIMIT 1",
            params![
                binding_digest,
                acknowledgement_direction.as_str(),
                i64::try_from(peer_sequence)?,
                peer_digest,
                application_name
            ],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(wire) = existing {
        return Ok(Some(SignedExecutionFrame::decode_and_verify(
            wire.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )?));
    }
    Ok(Some(author_frame(
        tx,
        owner,
        placement,
        acknowledgement_direction,
        signing_key,
        ExecutionChannelPayload::Acknowledge {
            peer_frame_sequence: peer_sequence,
            peer_frame_digest: peer_digest.to_owned(),
            application,
        },
    )?))
}

/// Recover the bounded exact outbound backlog from the peer's latest retained
/// acknowledgement. Claimed input is never replayed; a revoked gap blocks the
/// transcript instead of skipping ahead to terminal control.
pub fn pending_transport_frames(
    conn: &Connection,
    owner: &impl JournalOwner,
    placement: &str,
    direction: ChannelDirection,
    frame_limit: usize,
    byte_limit: usize,
) -> Result<Vec<PendingExecutionFrame>> {
    pending_transport_frames_after(
        conn,
        owner,
        placement,
        direction,
        0,
        frame_limit,
        byte_limit,
    )
}

/// Return pending frames after a process-local transport cursor while still
/// validating every retained predecessor from the peer's signed cumulative
/// frontier. The cursor grants no application or replay authority; callers may
/// advance it only across exact acknowledgement frames whose HTTP delivery they
/// observed, because the protocol intentionally suppresses ack-of-ack.
pub fn pending_transport_frames_after(
    conn: &Connection,
    owner: &impl JournalOwner,
    placement: &str,
    direction: ChannelDirection,
    after_sequence: u64,
    frame_limit: usize,
    byte_limit: usize,
) -> Result<Vec<PendingExecutionFrame>> {
    if frame_limit == 0 || frame_limit > 256 || byte_limit == 0 || byte_limit > 16 * 1024 * 1024 {
        bail!("external transport response bounds are invalid");
    }
    let binding = load_binding(conn, placement)?;
    owner.require_owner(conn, &binding)?;
    let binding_digest = binding.digest()?;
    let terminally_revoked = revoked(conn, &binding_digest)?;
    let acknowledged: i64 = conn.query_row(
        "SELECT COALESCE(MAX(acknowledged_peer_sequence),0)
         FROM external_execution_frame WHERE binding_digest=?1 AND direction=?2",
        params![binding_digest, direction.opposite().as_str()],
        |row| row.get(0),
    )?;
    let mut statement = conn.prepare(
        "SELECT sequence,frame_digest,frame_json,application
         FROM external_execution_frame
         WHERE binding_digest=?1 AND direction=?2 AND sequence>?3
         ORDER BY sequence ASC",
    )?;
    let mut rows = statement.query(params![binding_digest, direction.as_str(), acknowledged])?;
    let mut result = Vec::new();
    let mut bytes = 0_usize;
    let mut expected = u64::try_from(acknowledged)?
        .checked_add(1)
        .context("external transport frontier overflow")?;
    while let Some(row) = rows.next()? {
        let sequence = u64::try_from(row.get::<_, i64>(0)?)?;
        if sequence != expected {
            bail!("external transport backlog has a sequence gap");
        }
        expected = expected
            .checked_add(1)
            .context("external transport sequence overflow")?;
        let digest: String = row.get(1)?;
        let canonical: String = row.get(2)?;
        let application: String = row.get(3)?;
        if application != "pending" && terminally_revoked {
            // A separately retained signed cancellation may overtake this
            // uncertain or revoked predecessor. Never replay the predecessor;
            // the caller retrieves cancellation from the urgent lane below.
            break;
        }
        if application != "pending" {
            bail!("external reconnect cannot replay {application} frame {sequence}");
        }
        let verified = authenticate_retained_frame(
            &binding,
            direction,
            i64::try_from(sequence)?,
            &digest,
            canonical.as_bytes(),
            binding.issued_at_ms,
        )?;
        if terminally_revoked && matches!(verified.frame().payload, ExecutionChannelPayload::Cancel)
        {
            // Sticky cancellation is returned only through the urgent lane so
            // callers cannot accidentally serialize it behind ordinary input.
            break;
        }
        if terminally_revoked
            && matches!(
                verified.frame().payload,
                ExecutionChannelPayload::Release
                    | ExecutionChannelPayload::ProtocolBytes { .. }
                    | ExecutionChannelPayload::ProtocolEof
            )
        {
            // Cancellation fences this uncertain source-side input. Its exact
            // bytes are not replayed, while later signed Claimed/Applied proof
            // remains admissible as historical evidence.
            break;
        }
        if sequence <= after_sequence {
            continue;
        }
        let next_bytes = bytes
            .checked_add(canonical.len())
            .context("external transport response size overflow")?;
        if result.len() >= frame_limit || next_bytes > byte_limit {
            break;
        }
        bytes = next_bytes;
        result.push(PendingExecutionFrame {
            direction,
            sequence,
            digest,
            wire: canonical.into_bytes(),
        });
    }
    Ok(result)
}

/// Return the exact sticky cancellation independently of the contiguous
/// ordinary backlog. This is the sole lane allowed to cross a missing,
/// claimed, or revoked input predecessor; it never turns that input into
/// retryable work. Signed peer evidence suppresses the lane once cancellation
/// itself has been retained remotely.
pub fn pending_terminal_revocation_frame(
    conn: &Connection,
    owner: &impl JournalOwner,
    placement: &str,
    direction: ChannelDirection,
) -> Result<Option<PendingExecutionFrame>> {
    let binding = load_binding(conn, placement)?;
    owner.require_owner(conn, &binding)?;
    let binding_digest = binding.digest()?;
    let retained: Option<(String, String)> = conn
        .query_row(
            "SELECT frame_digest,frame_json FROM external_execution_revocation
             WHERE binding_digest=?1",
            [&binding_digest],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((digest, canonical)) = retained else {
        return Ok(None);
    };
    let verified = SignedExecutionFrame::decode_and_verify(
        canonical.as_bytes(),
        &binding,
        binding.issued_at_ms,
    )?;
    if verified.digest() != digest
        || verified.frame().direction != direction
        || !matches!(verified.frame().payload, ExecutionChannelPayload::Cancel)
    {
        bail!("external urgent revocation changed retained authority");
    }
    let sequence = verified.frame().sequence;
    let cumulative_ack: i64 = conn.query_row(
        "SELECT COALESCE(MAX(acknowledged_peer_sequence),0)
         FROM external_execution_frame WHERE binding_digest=?1 AND direction=?2",
        params![binding_digest, direction.opposite().as_str()],
        |row| row.get(0),
    )?;
    let application_ack: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_frame
         WHERE binding_digest=?1 AND direction=?2
           AND json_extract(frame_json,'$.frame.payload.kind')='acknowledge'
           AND json_extract(frame_json,'$.frame.payload.peer_frame_sequence')=?3
           AND json_extract(frame_json,'$.frame.payload.peer_frame_digest')=?4)",
        params![
            binding_digest,
            direction.opposite().as_str(),
            i64::try_from(sequence)?,
            digest
        ],
        |row| row.get(0),
    )?;
    if u64::try_from(cumulative_ack)? >= sequence || application_ack {
        return Ok(None);
    }
    Ok(Some(PendingExecutionFrame {
        direction,
        sequence,
        digest,
        wire: canonical.into_bytes(),
    }))
}

/// Complete an incoming acknowledgement's own no-effect application in the
/// same transaction that retained and reconciled its signed peer evidence.
/// Outbound acknowledgements must not use this helper: their application is
/// controlled only by later signed evidence from the remote peer.
pub fn apply_received_acknowledgement(
    tx: &Transaction<'_>,
    owner: &impl JournalOwner,
    placement: &str,
    direction: ChannelDirection,
    sequence: u64,
    digest: &str,
) -> Result<()> {
    let binding = load_binding(tx, placement)?;
    let row: (String, String) = tx.query_row(
        "SELECT frame_digest,frame_json FROM external_execution_frame
         WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
        params![
            binding.digest()?,
            direction.as_str(),
            i64::try_from(sequence)?
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if row.0 != digest {
        bail!("incoming acknowledgement changed its retained digest");
    }
    let verified = authenticate_retained_frame(
        &binding,
        direction,
        i64::try_from(sequence)?,
        digest,
        row.1.as_bytes(),
        binding.issued_at_ms,
    )?;
    if !matches!(
        verified.frame().payload,
        ExecutionChannelPayload::Acknowledge { .. }
    ) {
        bail!("no-effect application requires an acknowledgement frame");
    }
    match claim_application(tx, owner, placement, direction, sequence, digest)? {
        ApplicationClaim::New(_) => {
            finish_application(tx, owner, placement, direction, sequence, digest)
        }
        ApplicationClaim::AlreadyApplied => Ok(()),
        ApplicationClaim::AlreadyClaimed => {
            bail!("incoming acknowledgement retained an uncertain no-effect application")
        }
        ApplicationClaim::Revoked => bail!("incoming acknowledgement was unexpectedly revoked"),
    }
}

/// Claim exactly once. A crash after commit is uncertain delivery, never a
/// retry license. The actual dispatcher must share the revocation gate.
pub fn claim_application(
    tx: &Transaction<'_>,
    owner: &impl JournalOwner,
    placement: &str,
    direction: ChannelDirection,
    sequence: u64,
    digest: &str,
) -> Result<ApplicationClaim> {
    let binding = load_binding(tx, placement)?;
    let binding_digest = binding.digest()?;
    owner.require_owner(tx, &binding)?;
    let sequence = i64::try_from(sequence)?;
    let (stored_digest, application, wire): (String, String, String) = tx.query_row(
        "SELECT frame_digest,application,frame_json FROM external_execution_frame
         WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
        params![binding_digest, direction.as_str(), sequence],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if digest != stored_digest {
        bail!("external application claim changed its frame");
    }
    authenticate_retained_frame(
        &binding,
        direction,
        sequence,
        &stored_digest,
        wire.as_bytes(),
        binding.issued_at_ms,
    )?;
    match application.as_str() {
        "claimed" => return Ok(ApplicationClaim::AlreadyClaimed),
        "applied" => return Ok(ApplicationClaim::AlreadyApplied),
        "revoked" => return Ok(ApplicationClaim::Revoked),
        "pending" => {}
        _ => bail!("external frame retained an unknown application state"),
    }
    let verified = authenticate_retained_frame(
        &binding,
        direction,
        sequence,
        &stored_digest,
        wire.as_bytes(),
        lillux::time::timestamp_millis(),
    )?;
    owner.authorize_frame(tx, &binding, &verified.frame().payload)?;
    if revoked(tx, &binding_digest)? && !urgent_control(&verified.frame().payload) {
        bail!("external execution is durably revoked");
    }
    if lillux::time::timestamp_millis() >= binding.execution_deadline_ms
        && matches!(
            verified.frame().payload,
            ExecutionChannelPayload::Release
                | ExecutionChannelPayload::ProtocolBytes { .. }
                | ExecutionChannelPayload::ProtocolEof
        )
    {
        bail!("external execution deadline passed before application");
    }
    let state: String = tx.query_row(
        "SELECT state FROM external_execution_channel WHERE placement_thread_id=?1",
        [placement],
        |row| row.get(0),
    )?;
    if matches!(
        verified.frame().payload,
        ExecutionChannelPayload::Release
            | ExecutionChannelPayload::ProtocolBytes { .. }
            | ExecutionChannelPayload::ProtocolEof
    ) && !ChannelPhase::parse(&state)?.permits_pending_input()
    {
        bail!("external execution was revoked before pending application");
    }
    let pending_prior: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_frame WHERE binding_digest=?1
         AND direction=?2 AND sequence<?3 AND application NOT IN ('applied','revoked'))",
        params![binding_digest, direction.as_str(), sequence],
        |row| row.get(0),
    )?;
    if pending_prior && !urgent_control(&verified.frame().payload) {
        bail!("external frame has an unsettled predecessor application");
    }
    // Projection describes authored observations, not applied operations.
    // An export can be retained early, but cannot be imported before the
    // exact owner quiesce (and all its preceding input) has been applied.
    if matches!(
        verified.frame().payload,
        ExecutionChannelPayload::ExportObjectChunk { .. }
            | ExecutionChannelPayload::ExportSealed { .. }
    ) {
        let quiesced: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM external_execution_frame
            WHERE binding_digest=?1 AND direction='owner_to_supervisor'
            AND application='applied' AND json_extract(frame_json,'$.frame.payload.kind')='quiesce')",
            [&binding_digest], |row|row.get(0))?;
        if !quiesced {
            bail!("external export precedes applied owner quiescence");
        }
    }
    let changed = tx.execute(
        "UPDATE external_execution_frame SET application='claimed'
        WHERE binding_digest=?1 AND direction=?2 AND sequence=?3 AND application='pending'",
        params![binding_digest, direction.as_str(), sequence],
    )?;
    if changed != 1 {
        bail!("external application claim lost its serialized pending state");
    }
    Ok(ApplicationClaim::New(verified))
}

/// Acknowledge an exact claimed delivery, including after cancellation.
/// This is neither command completion nor external cleanup authority.
pub fn finish_application(
    tx: &Transaction<'_>,
    owner: &impl JournalOwner,
    placement: &str,
    direction: ChannelDirection,
    sequence: u64,
    digest: &str,
) -> Result<()> {
    let binding = load_binding(tx, placement)?;
    owner.require_owner(tx, &binding)?;
    let sequence = i64::try_from(sequence)?;
    let stored: (String, String, String) = tx.query_row(
        "SELECT frame_digest,application,frame_json FROM external_execution_frame
        WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
        params![binding.digest()?, direction.as_str(), sequence],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if stored.0 != digest || !matches!(stored.1.as_str(), "claimed" | "applied") {
        bail!("external frame completion has no exact claimed application");
    }
    // Historical delivery may finish after revocation or expiry. It cannot
    // create a new dispatch, and a sealed export still needs owner retention.
    let verified = authenticate_retained_frame(
        &binding,
        direction,
        sequence,
        &stored.0,
        stored.2.as_bytes(),
        binding.issued_at_ms,
    )?;
    if let ExecutionChannelPayload::ExportSealed {
        candidate_snapshot_hash,
        completion_request_digest,
        writer_exclusion_evidence_hash,
    } = &verified.frame().payload
    {
        owner.require_export_retention(
            tx,
            &binding,
            digest,
            candidate_snapshot_hash,
            completion_request_digest,
            writer_exclusion_evidence_hash,
        )?;
    }
    if stored.1 == "applied" {
        return Ok(());
    }
    tx.execute(
        "UPDATE external_execution_frame SET application='applied'
        WHERE binding_digest=?1 AND direction=?2 AND sequence=?3 AND application='claimed'",
        params![binding.digest()?, direction.as_str(), sequence],
    )?;
    Ok(())
}

fn authenticate_retained_frame(
    binding: &ExecutionChannelBinding,
    direction: ChannelDirection,
    sequence: i64,
    stored_digest: &str,
    wire: &[u8],
    validation_instant_ms: i64,
) -> Result<AuthenticatedExecutionFrame> {
    let verified = SignedExecutionFrame::decode_and_verify(wire, binding, validation_instant_ms)?;
    if verified.digest() != stored_digest
        || verified.frame().direction != direction
        || i64::try_from(verified.frame().sequence)? != sequence
    {
        bail!("retained external frame row contradicts its authenticated bytes");
    }
    Ok(verified)
}

pub fn load_binding(conn: &Connection, placement: &str) -> Result<ExecutionChannelBinding> {
    let (digest,raw): (String,String) = conn.query_row(
        "SELECT binding_digest,binding_json FROM external_execution_channel WHERE placement_thread_id=?1",
        [placement], |row|Ok((row.get(0)?,row.get(1)?)))?;
    if raw.len() > 8192 {
        bail!("external binding exceeds bound");
    }
    let binding: ExecutionChannelBinding = serde_json::from_str(&raw)?;
    if binding.placement_thread_id != placement
        || binding.digest()? != digest
        || lillux::canonical_json(&serde_json::to_value(&binding)?)? != raw
    {
        bail!("external channel row contradicts canonical binding");
    }
    Ok(binding)
}

pub fn revoked(conn: &Connection, digest: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_revocation WHERE binding_digest=?1)",
        [digest],
        |row| row.get(0),
    )?)
}

/// Validate retained signed transcripts after the runtime schema was accepted.
/// Historical authentication uses the binding's issue instant; its expiration
/// refuses new use, not retention/recovery of already recorded observations.
pub fn validate_channels(conn: &Connection, owner: &impl JournalOwner) -> Result<()> {
    let mut revocations = conn.prepare("SELECT r.binding_digest,r.frame_digest,r.frame_json,c.placement_thread_id
        FROM external_execution_revocation r LEFT JOIN external_execution_channel c ON c.binding_digest=r.binding_digest")?;
    let mut rows = revocations.query([])?;
    while let Some(row) = rows.next()? {
        let placement = row
            .get::<_, Option<String>>(3)?
            .context("external revocation has no channel owner")?;
        let binding = load_binding(conn, &placement)?;
        let wire: String = row.get(2)?;
        let verified = SignedExecutionFrame::decode_and_verify(
            wire.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )?;
        if row.get::<_, String>(0)? != binding.digest()?
            || row.get::<_, String>(1)? != verified.digest()
            || !matches!(verified.frame().payload, ExecutionChannelPayload::Cancel)
            || verified.frame().direction != ChannelDirection::OwnerToSupervisor
            || wire.len() as u64 > TERMINAL_CONTROL_BYTES
        {
            bail!("retained external revocation changed authority");
        }
    }
    let mut statement = conn.prepare(
        "SELECT placement_thread_id,state,completion_request_digest,
        export_snapshot_hash,export_evidence_hash FROM external_execution_channel",
    )?;
    let channels = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (placement, stored_state, stored_completion, stored_snapshot, stored_evidence) in channels {
        let binding = load_binding(conn, &placement)?;
        owner.require_owner(conn, &binding)?;
        let binding_digest = binding.digest()?;
        let sticky_revoked = revoked(conn, &binding_digest)?;
        let mut frames = conn.prepare(
            "SELECT ordinal,direction,sequence,frame_digest,frame_json,
            frame_bytes,acknowledged_peer_sequence,application FROM external_execution_frame
            WHERE binding_digest=?1 ORDER BY ordinal",
        )?;
        let mut rows = frames.query([&binding_digest])?;
        let mut ordinal = 0_i64;
        let mut frontiers = [FrameFrontier::default(), FrameFrontier::default()];
        let mut budgets = [
            ExecutionChannelBudget::default(),
            ExecutionChannelBudget::default(),
        ];
        let mut unsettled = [false; 2];
        // The controller may atomically claim a complete export prefix only
        // after its seal is retained. That batch is one idempotently
        // reconstructible CAS application, not several independently
        // replayable side effects. No other claimed-frame sequence may cross
        // an unsettled predecessor.
        let mut claimed_export_batch = [false; 2];
        let mut claimed_export_sealed = [false; 2];
        let mut state = ChannelPhase::Prepared;
        let mut completion: Option<String> = None;
        let mut snapshot: Option<String> = None;
        let mut evidence: Option<String> = None;
        let mut owner_quiesced_applied = false;
        let mut terminal_close_observed = sticky_revoked;
        let mut pending_input = false;
        let mut revoked_input = false;
        while let Some(row) = rows.next()? {
            ordinal += 1;
            let wire: String = row.get(4)?;
            let verified = SignedExecutionFrame::decode_and_verify(
                wire.as_bytes(),
                &binding,
                binding.issued_at_ms,
            )?;
            let frame = verified.frame();
            if matches!(frame.payload, ExecutionChannelPayload::Cancel) {
                let retained: Option<String> = conn
                    .query_row(
                        "SELECT frame_digest FROM external_execution_revocation
                    WHERE binding_digest=?1",
                        [&binding_digest],
                        |row| row.get(0),
                    )
                    .optional()?;
                if retained.as_deref() != Some(verified.digest()) {
                    bail!("external transcript cancellation lost its sticky revocation");
                }
            }
            let index = match frame.direction {
                ChannelDirection::OwnerToSupervisor => 0,
                ChannelDirection::SupervisorToOwner => 1,
            };
            let application: String = row.get(7)?;
            let is_export = matches!(
                frame.payload,
                ExecutionChannelPayload::ExportObjectChunk { .. }
                    | ExecutionChannelPayload::ExportSealed { .. }
            );
            let continues_claimed_export =
                claimed_export_batch[index] && application == "claimed" && is_export;
            if row.get::<_, i64>(0)? != ordinal
                || row.get::<_, String>(1)? != frame.direction.as_str()
                || row.get::<_, i64>(2)? != i64::try_from(frame.sequence)?
                || row.get::<_, String>(3)? != verified.digest()
                || row.get::<_, i64>(5)? != i64::try_from(wire.len())?
                || row.get::<_, i64>(6)? != i64::try_from(frame.acknowledged_peer_sequence)?
                || (unsettled[index]
                    && !matches!(application.as_str(), "pending" | "revoked")
                    && !urgent_control(&frame.payload)
                    && !continues_claimed_export)
            {
                bail!("retained external transcript ordering, application or identity mismatch");
            }
            frontiers[index].require_successor(frame, frontiers[1 - index].sequence())?;
            if !matches!(
                application.as_str(),
                "pending" | "claimed" | "applied" | "revoked"
            ) {
                bail!("retained external application has unknown state");
            }
            let is_input = matches!(
                frame.payload,
                ExecutionChannelPayload::Release
                    | ExecutionChannelPayload::ProtocolBytes { .. }
                    | ExecutionChannelPayload::ProtocolEof
            );
            if application == "revoked" {
                if !is_input {
                    bail!("external control observation was incorrectly revoked");
                }
                revoked_input = true;
            }
            pending_input |= application == "pending" && is_input;
            terminal_close_observed |= matches!(
                frame.payload,
                ExecutionChannelPayload::Cancel | ExecutionChannelPayload::Stopped { .. }
            );
            if matches!(
                frame.payload,
                ExecutionChannelPayload::ExportObjectChunk { .. }
                    | ExecutionChannelPayload::ExportSealed { .. }
            ) && matches!(application.as_str(), "claimed" | "applied")
                && !owner_quiesced_applied
            {
                bail!("retained external export precedes applied owner quiescence");
            }
            if application == "applied"
                && let ExecutionChannelPayload::ExportSealed {
                    candidate_snapshot_hash,
                    completion_request_digest,
                    writer_exclusion_evidence_hash,
                } = &frame.payload
            {
                owner.require_export_retention(
                    conn,
                    &binding,
                    verified.digest(),
                    candidate_snapshot_hash,
                    completion_request_digest,
                    writer_exclusion_evidence_hash,
                )?;
            }
            if application == "claimed" && is_export {
                claimed_export_batch[index] = true;
                claimed_export_sealed[index] |=
                    matches!(frame.payload, ExecutionChannelPayload::ExportSealed { .. });
            }
            if let ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence,
                peer_frame_digest,
                application: reported,
            } = &frame.payload
            {
                let peer: Option<(String, String)> = conn
                    .query_row(
                        "SELECT frame_digest,application FROM external_execution_frame
                     WHERE binding_digest=?1 AND direction=?2 AND sequence=?3",
                        params![
                            binding_digest,
                            frame.direction.opposite().as_str(),
                            i64::try_from(*peer_frame_sequence)?
                        ],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                if let Some((peer_digest, peer_application)) = peer {
                    if peer_digest != *peer_frame_digest
                        || !retained_application_proves(&peer_application, *reported)
                    {
                        bail!("retained external acknowledgement contradicts peer application");
                    }
                } else {
                    let actual = owner
                        .out_of_band_application_state(
                            conn,
                            &binding,
                            frame.direction.opposite(),
                            *peer_frame_sequence,
                            peer_frame_digest,
                        )?
                        .context("retained acknowledgement lost its terminal target")?;
                    if !application_state_proves(actual, *reported) {
                        bail!("retained acknowledgement exceeds terminal application evidence");
                    }
                }
            }
            owner_quiesced_applied |= application == "applied"
                && frame.direction == ChannelDirection::OwnerToSupervisor
                && matches!(frame.payload, ExecutionChannelPayload::Quiesce { .. });
            // Acknowledgements are durable transcript evidence, not executable
            // applications. Their cumulative peer frontier retires transport;
            // requiring an acknowledgement-of-acknowledgement would create an
            // infinite control loop.
            unsettled[index] |=
                !matches!(frame.payload, ExecutionChannelPayload::Acknowledge { .. })
                    && !matches!(application.as_str(), "applied" | "revoked");
            frontiers[index] = FrameFrontier::from_retained(
                frame.sequence,
                Some(verified.digest().to_owned()),
                frame.acknowledged_peer_sequence,
            )?;
            budgets[index].retain(&binding, &frame.payload, wire.len() as u64)?;
            state = state.advance(completion.as_deref(), &frame.payload, true)?;
            match &frame.payload {
                ExecutionChannelPayload::Quiesce {
                    completion_request_digest,
                } => completion = Some(completion_request_digest.clone()),
                ExecutionChannelPayload::ExportSealed {
                    candidate_snapshot_hash,
                    writer_exclusion_evidence_hash,
                    ..
                } => {
                    snapshot = Some(candidate_snapshot_hash.clone());
                    evidence = Some(writer_exclusion_evidence_hash.clone());
                }
                _ => {}
            }
        }
        if revoked_input && !terminal_close_observed {
            bail!("revoked external input has no authenticated close evidence");
        }
        if pending_input && terminal_close_observed && owner.can_prove_pending_input_revoked() {
            bail!("closed external execution retained pending input");
        }
        if claimed_export_batch
            .iter()
            .zip(claimed_export_sealed)
            .any(|(claimed, sealed)| *claimed && !sealed)
        {
            bail!("claimed external export prefix has no retained seal");
        }
        if state != ChannelPhase::parse(&stored_state)?
            || completion != stored_completion
            || snapshot != stored_snapshot
            || evidence != stored_evidence
        {
            bail!("external lifecycle projection contradicts signed transcript");
        }
    }
    let orphan: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM external_execution_frame f
        LEFT JOIN external_execution_channel c ON c.binding_digest=f.binding_digest WHERE c.binding_digest IS NULL)",
        [], |row|row.get(0))?;
    if orphan {
        bail!("external transcript has no channel owner");
    }
    Ok(())
}

fn retained_application_proves(retained: &str, reported: ExecutionFrameApplication) -> bool {
    let actual = match retained {
        "pending" => ExecutionFrameApplication::Retained,
        "claimed" => ExecutionFrameApplication::Claimed,
        "applied" => ExecutionFrameApplication::Applied,
        "revoked" => ExecutionFrameApplication::Revoked,
        _ => return false,
    };
    application_state_proves(actual, reported)
}

fn application_state_proves(
    actual: ExecutionFrameApplication,
    reported: ExecutionFrameApplication,
) -> bool {
    match reported {
        ExecutionFrameApplication::Retained => true,
        ExecutionFrameApplication::Claimed => matches!(
            actual,
            ExecutionFrameApplication::Claimed | ExecutionFrameApplication::Applied
        ),
        ExecutionFrameApplication::Applied => actual == ExecutionFrameApplication::Applied,
        ExecutionFrameApplication::Revoked => actual == ExecutionFrameApplication::Revoked,
    }
}

fn retained_budget(
    conn: &Connection,
    digest: &str,
    direction: &str,
) -> Result<ExecutionChannelBudget> {
    let mut budget = ExecutionChannelBudget::default();
    let mut statement = conn.prepare(
        "SELECT json_extract(frame_json,'$.frame.payload.kind'),
        COUNT(*),SUM(frame_bytes) FROM external_execution_frame
        WHERE binding_digest=?1 AND direction=?2 GROUP BY 1",
    )?;
    let mut rows = statement.query(params![digest, direction])?;
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let count = u64::try_from(row.get::<_, i64>(1)?)?;
        let bytes = u64::try_from(row.get::<_, i64>(2)?)?;
        if matches!(kind.as_str(), "cancel" | "stopped") {
            budget.terminal_frames += count;
            budget.terminal_bytes += bytes;
        } else {
            budget.ordinary_frames += count;
            budget.ordinary_bytes += bytes;
        }
    }
    Ok(budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_execution::{ExecutionFrame, ExternalStopReason};
    use anyhow::ensure;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use lillux::crypto::SigningKey;

    struct TestOwner;

    impl JournalOwner for TestOwner {
        fn require_owner(
            &self,
            conn: &Connection,
            binding: &ExecutionChannelBinding,
        ) -> Result<()> {
            let owned: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM test_owner WHERE binding_digest=?1)",
                [binding.digest()?],
                |row| row.get(0),
            )?;
            if !owned {
                bail!("test journal lost exact owner");
            }
            Ok(())
        }

        fn authorize_frame(
            &self,
            _conn: &Connection,
            _binding: &ExecutionChannelBinding,
            _payload: &ExecutionChannelPayload,
        ) -> Result<()> {
            Ok(())
        }

        fn require_export_retention(
            &self,
            conn: &Connection,
            binding: &ExecutionChannelBinding,
            frame_digest: &str,
            candidate_snapshot_hash: &str,
            completion_request_digest: &str,
            writer_exclusion_evidence_hash: &str,
        ) -> Result<()> {
            let retained: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM test_retention
                 WHERE binding_digest=?1 AND frame_digest=?2 AND snapshot_hash=?3
                   AND completion_digest=?4 AND evidence_hash=?5)",
                params![
                    binding.digest()?,
                    frame_digest,
                    candidate_snapshot_hash,
                    completion_request_digest,
                    writer_exclusion_evidence_hash
                ],
                |row| row.get(0),
            )?;
            if !retained {
                bail!("test export is not retained exactly");
            }
            Ok(())
        }

        fn out_of_band_application_state(
            &self,
            conn: &Connection,
            binding: &ExecutionChannelBinding,
            direction: ChannelDirection,
            sequence: u64,
            digest: &str,
        ) -> Result<Option<ExecutionFrameApplication>> {
            let wire: Option<String> = conn
                .query_row(
                    "SELECT frame_json FROM external_execution_revocation
                     WHERE binding_digest=?1 AND frame_digest=?2",
                    params![binding.digest()?, digest],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(wire) = wire else {
                return Ok(None);
            };
            let frame = SignedExecutionFrame::decode_and_verify(
                wire.as_bytes(),
                binding,
                binding.issued_at_ms,
            )?;
            ensure!(
                frame.frame().direction == direction
                    && frame.frame().sequence == sequence
                    && matches!(frame.frame().payload, ExecutionChannelPayload::Cancel),
                "test terminal application changed exact revocation"
            );
            Ok(Some(ExecutionFrameApplication::Retained))
        }

        fn can_prove_pending_input_revoked(&self) -> bool {
            true
        }
    }

    fn setup() -> (Connection, ExecutionChannelBinding, SigningKey, SigningKey) {
        let now = lillux::time::timestamp_millis();
        setup_with_window(now - 1_000, now + 60_000, now + 120_000)
    }

    fn setup_with_window(
        issued_at_ms: i64,
        execution_deadline_ms: i64,
        expires_at_ms: i64,
    ) -> (Connection, ExecutionChannelBinding, SigningKey, SigningKey) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(CHANNEL_SQL).unwrap();
        conn.execute_batch(
            "CREATE TABLE test_owner(binding_digest TEXT PRIMARY KEY);
             CREATE TABLE test_retention(
                binding_digest TEXT NOT NULL,
                frame_digest TEXT NOT NULL,
                snapshot_hash TEXT NOT NULL,
                completion_digest TEXT NOT NULL,
                evidence_hash TEXT NOT NULL,
                PRIMARY KEY(binding_digest,frame_digest));",
        )
        .unwrap();
        let owner = lillux::crypto::generate_signing_key();
        let supervisor = lillux::crypto::generate_signing_key();
        let binding = ExecutionChannelBinding {
            schema: 3,
            placement_thread_id: "T-shared-journal".into(),
            allocation_request_digest: "a".repeat(64),
            occurrence_id: "occurrence-one".into(),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            candidate_program_digest: "0".repeat(64),
            channel_nonce: "f".repeat(64),
            owner_public_key: STANDARD.encode(owner.verifying_key().as_bytes()),
            supervisor_public_key: STANDARD.encode(supervisor.verifying_key().as_bytes()),
            issued_at_ms,
            execution_deadline_ms,
            expires_at_ms,
            candidate_export_max_bytes: 512 * 1024,
            max_frames: 64,
            max_bytes: 1024 * 1024,
        };
        binding.validate().unwrap();
        let digest = binding.digest().unwrap();
        let canonical = lillux::canonical_json(&serde_json::to_value(&binding).unwrap()).unwrap();
        conn.execute(
            "INSERT INTO external_execution_channel VALUES(?1,?2,?3,'prepared',NULL,NULL,NULL)",
            params![binding.placement_thread_id, digest, canonical],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO test_owner VALUES(?1)",
            [binding.digest().unwrap()],
        )
        .unwrap();
        (conn, binding, owner, supervisor)
    }

    fn wire(
        binding: &ExecutionChannelBinding,
        key: &SigningKey,
        direction: ChannelDirection,
        sequence: u64,
        previous: Option<String>,
        acknowledged_peer_sequence: u64,
        payload: ExecutionChannelPayload,
    ) -> (Vec<u8>, String) {
        let signed = SignedExecutionFrame::sign(
            ExecutionFrame {
                schema: 1,
                binding_digest: binding.digest().unwrap(),
                direction,
                sequence,
                previous_frame_digest: previous,
                acknowledged_peer_sequence,
                payload,
            },
            binding,
            key,
        )
        .unwrap();
        let bytes = lillux::canonical_json(&serde_json::to_value(signed).unwrap())
            .unwrap()
            .into_bytes();
        let digest = lillux::sha256_hex(&bytes);
        (bytes, digest)
    }

    fn append(conn: &Connection, binding: &ExecutionChannelBinding, wire: &[u8]) -> Result<bool> {
        let tx = Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let result = append_frame(&tx, &TestOwner, &binding.placement_thread_id, wire)?;
        tx.commit()?;
        Ok(result)
    }

    fn claim(
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        direction: ChannelDirection,
        sequence: u64,
        digest: &str,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let result = claim_application(
            &tx,
            &TestOwner,
            &binding.placement_thread_id,
            direction,
            sequence,
            digest,
        )?;
        tx.commit()?;
        Ok(result.is_new())
    }

    fn claim_outcome(
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        direction: ChannelDirection,
        sequence: u64,
        digest: &str,
    ) -> Result<ApplicationClaim> {
        let tx = Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let result = claim_application(
            &tx,
            &TestOwner,
            &binding.placement_thread_id,
            direction,
            sequence,
            digest,
        )?;
        tx.commit()?;
        Ok(result)
    }

    fn finish(
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        direction: ChannelDirection,
        sequence: u64,
        digest: &str,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        finish_application(
            &tx,
            &TestOwner,
            &binding.placement_thread_id,
            direction,
            sequence,
            digest,
        )?;
        tx.commit()?;
        Ok(())
    }

    fn ready(
        conn: &Connection,
        binding: &ExecutionChannelBinding,
        supervisor: &SigningKey,
    ) -> String {
        let (wire, digest) = wire(
            binding,
            supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        );
        append(conn, binding, &wire).unwrap();
        claim(
            conn,
            binding,
            ChannelDirection::SupervisorToOwner,
            1,
            &digest,
        )
        .unwrap();
        finish(
            conn,
            binding,
            ChannelDirection::SupervisorToOwner,
            1,
            &digest,
        )
        .unwrap();
        digest
    }

    #[test]
    fn shared_journal_borrows_transaction_and_rechecks_duplicate_owner() {
        let (conn, binding, _owner, supervisor) = setup();
        let (ready_wire, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        );
        {
            let tx = Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            assert!(
                append_frame(&tx, &TestOwner, &binding.placement_thread_id, &ready_wire).unwrap()
            );
        }
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM external_execution_frame", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            0
        );
        assert!(append(&conn, &binding, &ready_wire).unwrap());
        conn.execute("DELETE FROM test_owner", []).unwrap();
        assert!(append(&conn, &binding, &ready_wire).is_err());
    }

    #[test]
    fn application_ack_target_is_independent_of_cumulative_receive_frontier() {
        let (conn, binding, owner, supervisor) = setup();
        let ready_digest = ready(&conn, &binding, &supervisor);
        let (release_wire, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        append(&conn, &binding, &release_wire).unwrap();
        claim(
            &conn,
            &binding,
            ChannelDirection::OwnerToSupervisor,
            1,
            &release_digest,
        )
        .unwrap();
        let (cancel_wire, _cancel_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest.clone()),
            1,
            ExecutionChannelPayload::Cancel,
        );
        {
            let tx = Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            record_revocation(&tx, &TestOwner, &binding.placement_thread_id, &cancel_wire).unwrap();
            append_frame(&tx, &TestOwner, &binding.placement_thread_id, &cancel_wire).unwrap();
            finish_application(
                &tx,
                &TestOwner,
                &binding.placement_thread_id,
                ChannelDirection::OwnerToSupervisor,
                1,
                &release_digest,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let tx =
            Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        let acknowledgement = ensure_application_acknowledgement(
            &tx,
            &TestOwner,
            &binding.placement_thread_id,
            ChannelDirection::SupervisorToOwner,
            1,
            &release_digest,
            &supervisor,
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(acknowledgement.frame().acknowledged_peer_sequence, 2);
        assert_eq!(
            acknowledgement.frame().previous_frame_digest.as_deref(),
            Some(ready_digest.as_str())
        );
        assert!(matches!(
            &acknowledgement.frame().payload,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 1,
                peer_frame_digest,
                application: ExecutionFrameApplication::Applied,
            } if peer_frame_digest == &release_digest
        ));
        tx.commit().unwrap();
        validate_channels(&conn, &TestOwner).unwrap();
    }

    #[test]
    fn sticky_cancellation_uses_urgent_lane_across_revoked_input_gap() {
        let (conn, binding, owner, supervisor) = setup();
        let ready_digest = ready(&conn, &binding, &supervisor);
        let (release_wire, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        append(&conn, &binding, &release_wire).unwrap();
        let (cancel_wire, cancel_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest),
            1,
            ExecutionChannelPayload::Cancel,
        );
        {
            let tx = Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            record_revocation(&tx, &TestOwner, &binding.placement_thread_id, &cancel_wire).unwrap();
            append_frame(&tx, &TestOwner, &binding.placement_thread_id, &cancel_wire).unwrap();
            tx.commit().unwrap();
        }
        assert!(
            pending_transport_frames(
                &conn,
                &TestOwner,
                &binding.placement_thread_id,
                ChannelDirection::OwnerToSupervisor,
                8,
                64 * 1024,
            )
            .unwrap()
            .is_empty()
        );
        let urgent = pending_terminal_revocation_frame(
            &conn,
            &TestOwner,
            &binding.placement_thread_id,
            ChannelDirection::OwnerToSupervisor,
        )
        .unwrap()
        .unwrap();
        assert_eq!(urgent.sequence(), 2);
        assert_eq!(urgent.digest(), cancel_digest);
        assert_eq!(urgent.wire(), cancel_wire);

        let (cancel_ack, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            0,
            ExecutionChannelPayload::Acknowledge {
                peer_frame_sequence: 2,
                peer_frame_digest: cancel_digest,
                application: ExecutionFrameApplication::Retained,
            },
        );
        append(&conn, &binding, &cancel_ack).unwrap();
        assert!(
            pending_terminal_revocation_frame(
                &conn,
                &TestOwner,
                &binding.placement_thread_id,
                ChannelDirection::OwnerToSupervisor,
            )
            .unwrap()
            .is_none()
        );
        validate_channels(&conn, &TestOwner).unwrap();
    }

    #[test]
    fn selected_row_must_match_authenticated_direction() {
        let (conn, binding, _owner, supervisor) = setup();
        let (ready_wire, ready_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        );
        conn.execute(
            "INSERT INTO external_execution_frame VALUES(?1,'owner_to_supervisor',1,1,?2,?3,?4,0,'pending')",
            params![binding.digest().unwrap(), ready_digest, String::from_utf8(ready_wire).unwrap(), 1],
        )
        .unwrap();
        assert!(
            claim(
                &conn,
                &binding,
                ChannelDirection::OwnerToSupervisor,
                1,
                &ready_digest
            )
            .is_err()
        );
        conn.execute(
            "UPDATE external_execution_frame SET application='claimed'",
            [],
        )
        .unwrap();
        assert!(
            finish(
                &conn,
                &binding,
                ChannelDirection::OwnerToSupervisor,
                1,
                &ready_digest
            )
            .is_err()
        );
    }

    #[test]
    fn selected_row_must_match_authenticated_digest() {
        let (conn, binding, _owner, supervisor) = setup();
        let (ready_wire, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        );
        let corrupt_digest = "0".repeat(64);
        let wire_bytes = i64::try_from(ready_wire.len()).unwrap();
        conn.execute(
            "INSERT INTO external_execution_frame VALUES(?1,'supervisor_to_owner',1,1,?2,?3,?4,0,'pending')",
            params![binding.digest().unwrap(), corrupt_digest, String::from_utf8(ready_wire).unwrap(), wire_bytes],
        )
        .unwrap();
        assert!(
            claim(
                &conn,
                &binding,
                ChannelDirection::SupervisorToOwner,
                1,
                &corrupt_digest
            )
            .is_err()
        );
        conn.execute(
            "UPDATE external_execution_frame SET application='claimed'",
            [],
        )
        .unwrap();
        assert!(
            finish(
                &conn,
                &binding,
                ChannelDirection::SupervisorToOwner,
                1,
                &corrupt_digest
            )
            .is_err()
        );
    }

    #[test]
    fn selected_row_must_match_authenticated_sequence() {
        let (conn, binding, _owner, supervisor) = setup();
        let (ready_wire, ready_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        );
        let wire_bytes = i64::try_from(ready_wire.len()).unwrap();
        conn.execute(
            "INSERT INTO external_execution_frame VALUES(?1,'supervisor_to_owner',2,1,?2,?3,?4,0,'pending')",
            params![binding.digest().unwrap(), ready_digest, String::from_utf8(ready_wire).unwrap(), wire_bytes],
        )
        .unwrap();
        assert!(
            claim(
                &conn,
                &binding,
                ChannelDirection::SupervisorToOwner,
                2,
                &ready_digest
            )
            .is_err()
        );
        conn.execute(
            "UPDATE external_execution_frame SET application='claimed'",
            [],
        )
        .unwrap();
        assert!(
            finish(
                &conn,
                &binding,
                ChannelDirection::SupervisorToOwner,
                2,
                &ready_digest
            )
            .is_err()
        );
    }

    #[test]
    fn new_input_claim_refuses_passed_execution_deadline_without_sleeping() {
        let now = lillux::time::timestamp_millis();
        let (conn, binding, owner, supervisor) =
            setup_with_window(now - 60_000, now - 1, now + 60_000);
        let (ready_wire, ready_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            1,
            None,
            0,
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
            },
        );
        let (release_wire, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        for (direction, digest, wire) in [
            ("supervisor_to_owner", &ready_digest, &ready_wire),
            ("owner_to_supervisor", &release_digest, &release_wire),
        ] {
            conn.execute(
                "INSERT INTO external_execution_frame VALUES(?1,?2,1,
                    (SELECT COALESCE(MAX(ordinal),0)+1 FROM external_execution_frame),
                    ?3,?4,?5,?6,'pending')",
                params![
                    binding.digest().unwrap(),
                    direction,
                    digest,
                    String::from_utf8(wire.clone()).unwrap(),
                    i64::try_from(wire.len()).unwrap(),
                    if direction == "owner_to_supervisor" {
                        1
                    } else {
                        0
                    }
                ],
            )
            .unwrap();
        }
        conn.execute(
            "UPDATE external_execution_frame SET application='claimed'
             WHERE direction='supervisor_to_owner'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE external_execution_frame SET application='applied'
             WHERE direction='supervisor_to_owner'",
            [],
        )
        .unwrap();
        conn.execute("UPDATE external_execution_channel SET state='running'", [])
            .unwrap();
        assert!(
            claim(
                &conn,
                &binding,
                ChannelDirection::OwnerToSupervisor,
                1,
                &release_digest
            )
            .is_err()
        );
    }

    #[test]
    fn sticky_revocation_commits_even_when_cancel_append_has_a_gap() {
        let (conn, binding, owner, supervisor) = setup();
        let ready_digest = ready(&conn, &binding, &supervisor);
        let (release, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        append(&conn, &binding, &release).unwrap();
        let (cancel, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            3,
            Some(release_digest.clone()),
            1,
            ExecutionChannelPayload::Cancel,
        );
        let tx =
            Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        assert!(record_revocation(&tx, &TestOwner, &binding.placement_thread_id, &cancel).unwrap());
        tx.commit().unwrap();
        let tx =
            Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        assert!(append_frame(&tx, &TestOwner, &binding.placement_thread_id, &cancel).is_err());
        drop(tx);
        assert!(revoked(&conn, &binding.digest().unwrap()).unwrap());
        let application: String = conn
            .query_row(
                "SELECT application FROM external_execution_frame WHERE frame_digest=?1",
                [release_digest],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(application, "revoked");
        assert!(!ready_digest.is_empty());
    }

    #[test]
    fn claimed_input_finishes_historically_after_cancellation() {
        let (conn, binding, owner, supervisor) = setup();
        ready(&conn, &binding, &supervisor);
        let (release, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        append(&conn, &binding, &release).unwrap();
        assert!(
            claim(
                &conn,
                &binding,
                ChannelDirection::OwnerToSupervisor,
                1,
                &release_digest
            )
            .unwrap()
        );
        assert!(matches!(
            claim_outcome(
                &conn,
                &binding,
                ChannelDirection::OwnerToSupervisor,
                1,
                &release_digest
            )
            .unwrap(),
            ApplicationClaim::AlreadyClaimed
        ));
        let (cancel, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest.clone()),
            1,
            ExecutionChannelPayload::Cancel,
        );
        let tx =
            Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        record_revocation(&tx, &TestOwner, &binding.placement_thread_id, &cancel).unwrap();
        tx.commit().unwrap();
        append(&conn, &binding, &cancel).unwrap();
        finish(
            &conn,
            &binding,
            ChannelDirection::OwnerToSupervisor,
            1,
            &release_digest,
        )
        .unwrap();
        assert!(matches!(
            claim_outcome(
                &conn,
                &binding,
                ChannelDirection::OwnerToSupervisor,
                1,
                &release_digest
            )
            .unwrap(),
            ApplicationClaim::AlreadyApplied
        ));
    }

    #[test]
    fn sealed_export_requires_exact_retention_at_finish_and_reopen() {
        let (conn, binding, owner, supervisor) = setup();
        let ready_digest = ready(&conn, &binding, &supervisor);
        let (release, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        append(&conn, &binding, &release).unwrap();
        claim(
            &conn,
            &binding,
            ChannelDirection::OwnerToSupervisor,
            1,
            &release_digest,
        )
        .unwrap();
        finish(
            &conn,
            &binding,
            ChannelDirection::OwnerToSupervisor,
            1,
            &release_digest,
        )
        .unwrap();
        let completion = "1".repeat(64);
        let snapshot = "2".repeat(64);
        let evidence = "3".repeat(64);
        let (quiesce, quiesce_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest),
            1,
            ExecutionChannelPayload::Quiesce {
                completion_request_digest: completion.clone(),
            },
        );
        append(&conn, &binding, &quiesce).unwrap();
        claim(
            &conn,
            &binding,
            ChannelDirection::OwnerToSupervisor,
            2,
            &quiesce_digest,
        )
        .unwrap();
        finish(
            &conn,
            &binding,
            ChannelDirection::OwnerToSupervisor,
            2,
            &quiesce_digest,
        )
        .unwrap();
        let (sealed, sealed_digest) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            2,
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash: snapshot.clone(),
                completion_request_digest: completion.clone(),
                writer_exclusion_evidence_hash: evidence.clone(),
            },
        );
        append(&conn, &binding, &sealed).unwrap();
        claim(
            &conn,
            &binding,
            ChannelDirection::SupervisorToOwner,
            2,
            &sealed_digest,
        )
        .unwrap();
        assert!(
            finish(
                &conn,
                &binding,
                ChannelDirection::SupervisorToOwner,
                2,
                &sealed_digest
            )
            .is_err()
        );
        conn.execute(
            "INSERT INTO test_retention VALUES(?1,?2,?3,?4,?5)",
            params![
                binding.digest().unwrap(),
                sealed_digest,
                snapshot,
                completion,
                evidence
            ],
        )
        .unwrap();
        finish(
            &conn,
            &binding,
            ChannelDirection::SupervisorToOwner,
            2,
            &sealed_digest,
        )
        .unwrap();
        validate_channels(&conn, &TestOwner).unwrap();
        conn.execute(
            "UPDATE test_retention SET evidence_hash=?1",
            ["4".repeat(64)],
        )
        .unwrap();
        assert!(validate_channels(&conn, &TestOwner).is_err());
        assert!(
            finish(
                &conn,
                &binding,
                ChannelDirection::SupervisorToOwner,
                2,
                &sealed_digest
            )
            .is_err()
        );
    }

    #[test]
    fn reopen_rejects_pending_input_after_revocation_and_export_before_applied_quiesce() {
        let (conn, binding, owner, supervisor) = setup();
        let ready_digest = ready(&conn, &binding, &supervisor);
        let (release, release_digest) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        append(&conn, &binding, &release).unwrap();
        let completion = "5".repeat(64);
        let (quiesce, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            2,
            Some(release_digest.clone()),
            1,
            ExecutionChannelPayload::Quiesce {
                completion_request_digest: completion.clone(),
            },
        );
        append(&conn, &binding, &quiesce).unwrap();
        let (sealed, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            2,
            ExecutionChannelPayload::ExportSealed {
                candidate_snapshot_hash: "6".repeat(64),
                completion_request_digest: completion,
                writer_exclusion_evidence_hash: "7".repeat(64),
            },
        );
        append(&conn, &binding, &sealed).unwrap();
        conn.execute(
            "UPDATE external_execution_frame SET application='claimed'
             WHERE direction='supervisor_to_owner' AND sequence=2",
            [],
        )
        .unwrap();
        assert!(validate_channels(&conn, &TestOwner).is_err());

        conn.execute(
            "UPDATE external_execution_frame SET application='pending'
             WHERE direction='supervisor_to_owner' AND sequence=2",
            [],
        )
        .unwrap_err();
        let (cancel, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            3,
            Some(release_digest),
            1,
            ExecutionChannelPayload::Cancel,
        );
        let tx =
            Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate).unwrap();
        record_revocation(&tx, &TestOwner, &binding.placement_thread_id, &cancel).unwrap();
        tx.commit().unwrap();
        conn.execute("DROP TRIGGER external_execution_frame_immutable", [])
            .unwrap();
        conn.execute(
            "UPDATE external_execution_frame SET application='pending'
             WHERE (direction='owner_to_supervisor' AND sequence=1)
                OR (direction='supervisor_to_owner' AND sequence=2)",
            [],
        )
        .unwrap();
        assert!(validate_channels(&conn, &TestOwner).is_err());
    }

    #[test]
    fn reopen_requires_close_evidence_for_revoked_input() {
        let (conn, binding, owner, supervisor) = setup();
        ready(&conn, &binding, &supervisor);
        let (release, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        append(&conn, &binding, &release).unwrap();
        conn.execute("DROP TRIGGER external_execution_frame_immutable", [])
            .unwrap();
        conn.execute(
            "UPDATE external_execution_frame SET application='revoked'
             WHERE direction='owner_to_supervisor'",
            [],
        )
        .unwrap();
        assert!(validate_channels(&conn, &TestOwner).is_err());
    }

    #[test]
    fn reopen_rejects_pending_input_after_authenticated_stop() {
        let (conn, binding, owner, supervisor) = setup();
        let ready_digest = ready(&conn, &binding, &supervisor);
        let (release, _) = wire(
            &binding,
            &owner,
            ChannelDirection::OwnerToSupervisor,
            1,
            None,
            1,
            ExecutionChannelPayload::Release,
        );
        append(&conn, &binding, &release).unwrap();
        let (stopped, _) = wire(
            &binding,
            &supervisor,
            ChannelDirection::SupervisorToOwner,
            2,
            Some(ready_digest),
            1,
            ExecutionChannelPayload::Stopped {
                reason: ExternalStopReason::Cancelled,
            },
        );
        append(&conn, &binding, &stopped).unwrap();
        conn.execute("DROP TRIGGER external_execution_frame_immutable", [])
            .unwrap();
        conn.execute(
            "UPDATE external_execution_frame SET application='pending'
             WHERE direction='owner_to_supervisor'",
            [],
        )
        .unwrap();
        assert!(validate_channels(&conn, &TestOwner).is_err());
    }

    #[test]
    fn shared_reopen_refuses_projection_corruption_and_orphan_frames() {
        let (conn, binding, _owner, supervisor) = setup();
        ready(&conn, &binding, &supervisor);
        conn.execute("UPDATE external_execution_channel SET state='running'", [])
            .unwrap();
        assert!(validate_channels(&conn, &TestOwner).is_err());

        let (conn, binding, _owner, supervisor) = setup();
        ready(&conn, &binding, &supervisor);
        conn.execute(
            "DELETE FROM external_execution_channel WHERE placement_thread_id=?1",
            [&binding.placement_thread_id],
        )
        .unwrap();
        assert!(validate_channels(&conn, &TestOwner).is_err());
    }

    #[test]
    fn schema_refuses_advanced_initial_channel_and_application() {
        let (conn, binding, _owner, _supervisor) = setup();
        assert!(conn.execute(
            "INSERT INTO external_execution_channel VALUES('T-other','other','{}','ready',NULL,NULL,NULL)",
            [],
        ).is_err());
        assert!(conn.execute(
            "INSERT INTO external_execution_frame VALUES(?1,'owner_to_supervisor',1,1,'digest','{}',2,0,'claimed')",
            [binding.digest().unwrap()],
        ).is_err());
        assert!(conn.execute(
            "INSERT INTO external_execution_channel VALUES('T-projected','projected','{}','prepared',?1,NULL,NULL)",
            ["8".repeat(64)],
        ).is_err());
    }
}
