//! Durable controller ownership for Render's occurrence-private file relay.
//!
//! A transfer receipt is transport evidence only. It cannot attach a channel,
//! acknowledge a signed frame, start another guest process, or qualify a Q
//! result. The operation that interprets bytes must still pass the existing
//! attach/exchange boundary before an owner-authenticated response is signed.

use super::*;
use ryeos_state::external_execution::transport::ExternalSupervisorBootstrap;

pub(super) const JOURNAL_SQL: &str = r#"
CREATE TABLE external_execution_relay_transfer (
    placement_thread_id TEXT NOT NULL REFERENCES external_execution_allocation(placement_thread_id),
    kind TEXT NOT NULL CHECK (kind IN ('attach','exchange')),
    sequence INTEGER NOT NULL CHECK (sequence BETWEEN 0 AND 65536),
    direction TEXT NOT NULL CHECK (direction IN ('guest_to_controller','controller_to_guest')),
    remote_path TEXT NOT NULL UNIQUE,
    intent_json TEXT NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved','reading','upload_possible','accepted','quarantined','terminal_closed')),
    read_attempts INTEGER NOT NULL CHECK (read_attempts BETWEEN 0 AND 128),
    observed_sha256 TEXT,
    observed_bytes INTEGER,
    canonical_bytes BLOB,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (placement_thread_id,kind,sequence,direction),
    CHECK (updated_at_ms >= created_at_ms),
    CHECK (length(CAST(intent_json AS BLOB)) BETWEEN 1 AND 16384),
    CHECK ((observed_sha256 IS NULL AND observed_bytes IS NULL)
        OR (phase IN ('accepted','quarantined','terminal_closed') AND observed_sha256 IS NOT NULL
            AND observed_bytes BETWEEN 1 AND 2097152)),
    CHECK (canonical_bytes IS NULL OR length(canonical_bytes) BETWEEN 1 AND 2097152)
);
CREATE TRIGGER external_execution_relay_transfer_transition_guard
BEFORE UPDATE ON external_execution_relay_transfer
WHEN NEW.placement_thread_id!=OLD.placement_thread_id
 OR NEW.kind!=OLD.kind OR NEW.sequence!=OLD.sequence
 OR NEW.direction!=OLD.direction OR NEW.remote_path!=OLD.remote_path
 OR NEW.intent_json!=OLD.intent_json OR NEW.created_at_ms!=OLD.created_at_ms
 OR NEW.updated_at_ms<OLD.updated_at_ms
 OR NEW.read_attempts<OLD.read_attempts OR NEW.read_attempts>OLD.read_attempts+1
 OR (OLD.canonical_bytes IS NOT NULL AND NEW.canonical_bytes IS NOT OLD.canonical_bytes)
 OR (OLD.observed_sha256 IS NOT NULL AND
     (NEW.observed_sha256 IS NOT OLD.observed_sha256 OR NEW.observed_bytes IS NOT OLD.observed_bytes))
 OR NOT ((OLD.phase='reserved' AND NEW.phase IN ('reading','upload_possible','quarantined','terminal_closed'))
      OR (OLD.phase='reading' AND NEW.phase IN ('reading','accepted','quarantined','terminal_closed'))
      OR (OLD.phase='upload_possible' AND NEW.phase IN ('upload_possible','accepted','quarantined','terminal_closed'))
      OR (OLD.phase='accepted' AND NEW.phase IN ('quarantined','terminal_closed'))
      OR (OLD.phase='quarantined' AND NEW.phase='terminal_closed'))
BEGIN SELECT RAISE(ABORT, 'external relay transfer is one-use'); END;
CREATE TRIGGER external_execution_relay_transfer_no_delete
BEFORE DELETE ON external_execution_relay_transfer
BEGIN SELECT RAISE(ABORT, 'external relay transfer is retained'); END;
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RelayDirection {
    GuestToController,
    ControllerToGuest,
}

impl RelayDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::GuestToController => "guest_to_controller",
            Self::ControllerToGuest => "controller_to_guest",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RelayKind {
    Attach,
    Exchange,
}

impl RelayKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Attach => "attach",
            Self::Exchange => "exchange",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelayTransferIntent {
    pub schema: u32,
    pub placement_thread_id: String,
    pub occurrence_id: String,
    pub allocation_request_digest: String,
    pub bootstrap_digest: String,
    pub mailbox_root: String,
    pub kind: RelayKind,
    pub sequence: u32,
    pub direction: RelayDirection,
    pub remote_path: String,
    pub maximum_file_bytes: u64,
    /// Inbound bytes cannot be known before reading the protected guest file.
    /// The first accepted canonical request pins these fields permanently.
    pub expected_sha256: Option<String>,
    pub expected_bytes: Option<u64>,
    pub deadline_ms: i64,
    /// Controller-owned byte ceiling for all retained files in this placement.
    pub retention_budget_bytes: u64,
}

impl RelayTransferIntent {
    pub(crate) fn from_bootstrap(
        bootstrap: &ExternalSupervisorBootstrap,
        kind: RelayKind,
        sequence: u32,
        direction: RelayDirection,
        outbound: Option<&[u8]>,
    ) -> Result<Self> {
        bootstrap.validate()?;
        let relay = bootstrap.controller.relay()?;
        ensure!(
            (kind == RelayKind::Attach && sequence == 0)
                || (kind == RelayKind::Exchange && (1..=relay.maximum_frames).contains(&sequence)),
            "external relay transfer exceeds signed frame sequence"
        );
        ensure!(
            outbound.is_some() == (direction == RelayDirection::ControllerToGuest),
            "external relay transfer direction changed its expected bytes"
        );
        let name = match (kind, direction) {
            (RelayKind::Attach, RelayDirection::GuestToController) => "attach.request.json".into(),
            (RelayKind::Attach, RelayDirection::ControllerToGuest) => "attach.response.json".into(),
            (RelayKind::Exchange, RelayDirection::GuestToController) => {
                format!("exchange-{sequence:05}.request.json")
            }
            (RelayKind::Exchange, RelayDirection::ControllerToGuest) => {
                format!("exchange-{sequence:05}.response.json")
            }
        };
        let mailbox_root = bootstrap.relay_mailbox_root()?;
        let intent = Self {
            schema: 1,
            placement_thread_id: bootstrap.placement_thread_id.clone(),
            occurrence_id: bootstrap.occurrence_id.clone(),
            allocation_request_digest: bootstrap.allocation_request_digest.clone(),
            bootstrap_digest: bootstrap.digest()?,
            remote_path: format!("{mailbox_root}/{name}"),
            mailbox_root,
            maximum_file_bytes: relay.maximum_file_bytes,
            kind,
            sequence,
            direction,
            expected_sha256: outbound.map(lillux::sha256_hex),
            expected_bytes: outbound.map(|bytes| bytes.len() as u64),
            deadline_ms: if kind == RelayKind::Attach {
                bootstrap.attachment_deadline_ms
            } else {
                bootstrap
                    .attachment_deadline_ms
                    .checked_add(i64::from(bootstrap.execution_timeout_seconds) * 1_000)
                    .and_then(|deadline| {
                        deadline.checked_add(
                            i64::from(bootstrap.post_execution_timeout_seconds) * 1_000,
                        )
                    })
                    .context("external relay exchange deadline overflow")?
            },
            retention_budget_bytes: bootstrap.channel_max_bytes.saturating_mul(4),
        };
        intent.validate()?;
        Ok(intent)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1,
            "unsupported external relay transfer intent"
        );
        for digest in [&self.allocation_request_digest, &self.bootstrap_digest] {
            validate_sha256("external relay identity", digest)?;
        }
        ensure!(
            !self.placement_thread_id.is_empty()
                && self.placement_thread_id.len() <= 256
                && self.occurrence_id.starts_with("sbx-")
                && self.occurrence_id.len() <= 128
                && self
                    .occurrence_id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "external relay placement or occurrence is invalid"
        );
        ensure!(
            (self.kind == RelayKind::Attach && self.sequence == 0)
                || (self.kind == RelayKind::Exchange && (1..=65_536).contains(&self.sequence)),
            "external relay transfer sequence is invalid"
        );
        let name = match (self.kind, self.direction) {
            (RelayKind::Attach, RelayDirection::GuestToController) => "attach.request.json".into(),
            (RelayKind::Attach, RelayDirection::ControllerToGuest) => "attach.response.json".into(),
            (RelayKind::Exchange, RelayDirection::GuestToController) => {
                format!("exchange-{:05}.request.json", self.sequence)
            }
            (RelayKind::Exchange, RelayDirection::ControllerToGuest) => {
                format!("exchange-{:05}.response.json", self.sequence)
            }
        };
        ensure!(
            self.mailbox_root.starts_with('/')
                && !self.mailbox_root.starts_with("//")
                && self.mailbox_root.len() <= 2048
                && self.mailbox_root.split('/').skip(1).all(|s| !s.is_empty()
                    && s != "."
                    && s != ".."
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
                && self.remote_path == format!("{}/{name}", self.mailbox_root)
                && self.remote_path.len() <= 2048,
            "external relay transfer changed its exact mailbox path"
        );
        ensure!(
            self.expected_sha256.is_some() == self.expected_bytes.is_some()
                && self.expected_sha256.is_some()
                    == (self.direction == RelayDirection::ControllerToGuest),
            "external relay transfer changed its directional byte identity"
        );
        if let (Some(digest), Some(bytes)) = (&self.expected_sha256, self.expected_bytes) {
            validate_sha256("external relay upload", digest)?;
            ensure!(
                (1..=self.maximum_file_bytes).contains(&bytes),
                "external relay upload exceeds file bound"
            );
        }
        ensure!(
            self.deadline_ms > 0
                && (1..=2 * 1024 * 1024).contains(&self.maximum_file_bytes)
                && (4..=256 * 1024 * 1024).contains(&self.retention_budget_bytes),
            "external relay deadline or retained byte budget is invalid"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RelayTransferPhase {
    Reserved,
    Reading,
    UploadPossible,
    Accepted,
    Quarantined,
    TerminalClosed,
}

impl RelayTransferPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Reading => "reading",
            Self::UploadPossible => "upload_possible",
            Self::Accepted => "accepted",
            Self::Quarantined => "quarantined",
            Self::TerminalClosed => "terminal_closed",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "reserved" => Ok(Self::Reserved),
            "reading" => Ok(Self::Reading),
            "upload_possible" => Ok(Self::UploadPossible),
            "accepted" => Ok(Self::Accepted),
            "quarantined" => Ok(Self::Quarantined),
            "terminal_closed" => Ok(Self::TerminalClosed),
            _ => anyhow::bail!("external relay transfer phase is invalid"),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RelayTransferRecord {
    pub intent: RelayTransferIntent,
    pub phase: RelayTransferPhase,
    pub read_attempts: u32,
    pub observed_sha256: Option<String>,
    pub observed_bytes: Option<u64>,
    /// Exact guest request or owner-signed response, retained for replay.
    pub canonical_bytes: Option<Vec<u8>>,
}

/// Process-local authority returned only after the durable contact transition.
/// It is consumed by the exact retained adapter; callers cannot construct it
/// from an occurrence or a remote path.
pub(crate) struct ClaimedRelayFileContact {
    record: RelayTransferRecord,
    effect: ryeos_external_execution_contract::relay_file::RelayFileEffect,
}

impl ClaimedRelayFileContact {
    pub(crate) fn record(&self) -> &RelayTransferRecord {
        &self.record
    }

    pub(crate) fn effect(&self) -> ryeos_external_execution_contract::relay_file::RelayFileEffect {
        self.effect
    }
}

fn canonical(intent: &RelayTransferIntent) -> Result<String> {
    intent.validate()?;
    Ok(lillux::canonical_json(&serde_json::to_value(intent)?)?)
}

fn read_transfer(
    conn: &Connection,
    intent: &RelayTransferIntent,
) -> Result<Option<RelayTransferRecord>> {
    let row: Option<(String, String, String, i64, Option<String>, Option<i64>, Option<Vec<u8>>)> = conn.query_row(
        "SELECT remote_path,intent_json,phase,read_attempts,observed_sha256,observed_bytes,canonical_bytes
         FROM external_execution_relay_transfer
         WHERE placement_thread_id=?1 AND kind=?2 AND sequence=?3 AND direction=?4",
        params![intent.placement_thread_id, intent.kind.as_str(), intent.sequence, intent.direction.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
    ).optional()?;
    row.map(
        |(path, raw, phase, attempts, digest, bytes, canonical_bytes)| {
            let retained: RelayTransferIntent = serde_json::from_str(&raw)?;
            ensure!(
                canonical(&retained)? == raw && retained.remote_path == path && retained == *intent,
                "external relay transfer changed its retained intent"
            );
            let phase = RelayTransferPhase::parse(&phase)?;
            ensure!(
                (0..=128).contains(&attempts),
                "external relay read count is invalid"
            );
            let observed_bytes = bytes.map(u64::try_from).transpose()?;
            ensure!(
                digest.is_some() == observed_bytes.is_some(),
                "external relay observation is incomplete"
            );
            if let (Some(digest), Some(bytes)) = (&digest, observed_bytes) {
                validate_sha256("external relay observation", digest)?;
                ensure!(
                    (1..=intent.maximum_file_bytes).contains(&bytes),
                    "external relay observation exceeds bound"
                );
            }
            if let Some(bytes) = &canonical_bytes {
                ensure!(
                    !bytes.is_empty()
                        && bytes.len() as u64 <= intent.maximum_file_bytes
                        && lillux::canonical_json(&serde_json::from_slice::<serde_json::Value>(
                            bytes
                        )?)?
                        .as_bytes()
                            == bytes,
                    "external relay retained bytes are not bounded canonical JSON"
                );
                let expected = if intent.direction == RelayDirection::ControllerToGuest {
                    (intent.expected_sha256.as_deref(), intent.expected_bytes)
                } else {
                    (digest.as_deref(), observed_bytes)
                };
                let actual_digest = lillux::sha256_hex(bytes);
                ensure!(
                    expected == (Some(actual_digest.as_str()), Some(bytes.len() as u64)),
                    "external relay retained bytes changed their digest or length"
                );
            }
            ensure!(
                canonical_bytes.is_some()
                    == (intent.direction == RelayDirection::ControllerToGuest || digest.is_some()),
                "external relay retained byte custody changed"
            );
            ensure!(
                match (intent.direction, phase) {
                    (
                        RelayDirection::GuestToController,
                        RelayTransferPhase::Reading | RelayTransferPhase::Accepted,
                    ) => attempts > 0,
                    (RelayDirection::ControllerToGuest, RelayTransferPhase::Accepted) =>
                        attempts > 0,
                    (RelayDirection::GuestToController, RelayTransferPhase::UploadPossible)
                    | (RelayDirection::ControllerToGuest, RelayTransferPhase::Reading) => false,
                    (_, RelayTransferPhase::Reserved) => attempts == 0,
                    _ => true,
                },
                "external relay transfer phase contradicts its durable contact count"
            );
            Ok(RelayTransferRecord {
                intent: retained,
                phase,
                read_attempts: attempts as u32,
                observed_sha256: digest,
                observed_bytes,
                canonical_bytes,
            })
        },
    )
    .transpose()
}

fn require_live_q_allocation(conn: &Connection, intent: &RelayTransferIntent) -> Result<()> {
    let allocation = read(conn, &intent.placement_thread_id)?
        .context("external relay lost its original allocation")?;
    let occurrence = allocation
        .occurrence
        .as_ref()
        .context("external relay has no original occurrence")?;
    ensure!(
        allocation.phase == ExternalAllocationPhase::Bound
            && matches!(
                &allocation.reservation.owner,
                ExternalAllocationOwner::QualificationCandidate(_)
            )
            && allocation.reservation.request_digest == intent.allocation_request_digest
            && occurrence.occurrence_id == intent.occurrence_id,
        "external relay requires the exact live Q allocation"
    );
    channel::require_current_execution_owner(conn, &allocation.reservation)?;
    let activation: ExternalSupervisorActivationIntent = read_canonical_evidence(
        conn,
        "external_execution_supervisor_activation_intent",
        "intent_json",
        &intent.placement_thread_id,
    )?
    .context("external relay has no original supervisor-start intent")?;
    activation.validate(&allocation.reservation, occurrence)?;
    Ok(())
}

fn require_preceding_transfer(conn: &Connection, intent: &RelayTransferIntent) -> Result<()> {
    let predecessor = match (intent.kind, intent.direction, intent.sequence) {
        (RelayKind::Attach, RelayDirection::GuestToController, 0) => return Ok(()),
        (kind, RelayDirection::ControllerToGuest, sequence) => (
            kind.as_str(),
            sequence,
            RelayDirection::GuestToController.as_str(),
        ),
        (RelayKind::Exchange, RelayDirection::GuestToController, 1) => (
            RelayKind::Attach.as_str(),
            0,
            RelayDirection::ControllerToGuest.as_str(),
        ),
        (RelayKind::Exchange, RelayDirection::GuestToController, sequence) => (
            RelayKind::Exchange.as_str(),
            sequence - 1,
            RelayDirection::ControllerToGuest.as_str(),
        ),
        _ => anyhow::bail!("external relay sequence has no predecessor"),
    };
    let accepted: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_relay_transfer
         WHERE placement_thread_id=?1 AND kind=?2 AND sequence=?3 AND direction=?4 AND phase='accepted')",
        params![intent.placement_thread_id, predecessor.0, predecessor.1, predecessor.2],
        |row| row.get(0),
    )?;
    ensure!(
        accepted,
        "external relay transfer has no accepted predecessor"
    );
    Ok(())
}

impl RuntimeDb {
    /// Recover precommitted controller bytes without minting a replacement PUT.
    pub(crate) fn retained_external_relay_outbound(
        &self,
        bootstrap: &ExternalSupervisorBootstrap,
        kind: RelayKind,
        sequence: u32,
    ) -> Result<RelayTransferRecord> {
        bootstrap.validate()?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let bytes: Vec<u8> = tx
            .query_row(
                "SELECT canonical_bytes FROM external_execution_relay_transfer
             WHERE placement_thread_id=?1 AND kind=?2 AND sequence=?3
               AND direction='controller_to_guest'",
                params![bootstrap.placement_thread_id, kind.as_str(), sequence],
                |row| row.get(0),
            )
            .context("external relay has no retained controller response")?;
        let intent = RelayTransferIntent::from_bootstrap(
            bootstrap,
            kind,
            sequence,
            RelayDirection::ControllerToGuest,
            Some(&bytes),
        )?;
        require_live_q_allocation(&tx, &intent)?;
        require_placement_bootstrap(&tx, &intent)?;
        let record = read_transfer(&tx, &intent)?
            .context("external relay retained controller response disappeared")?;
        ensure!(
            record.canonical_bytes.as_deref() == Some(bytes.as_slice()),
            "external relay retained response changed its exact bytes"
        );
        tx.commit()?;
        Ok(record)
    }

    pub(crate) fn accepted_external_relay_request(
        &self,
        intent: &RelayTransferIntent,
    ) -> Result<RelayTransferRecord> {
        ensure!(
            intent.direction == RelayDirection::GuestToController,
            "external relay predecessor must be a guest request"
        );
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        require_live_q_allocation(&tx, intent)?;
        let record = read_transfer(&tx, intent)?
            .context("external relay has no exact predecessor request")?;
        ensure!(
            record.phase == RelayTransferPhase::Accepted
                && record.observed_sha256.is_some()
                && record.canonical_bytes.is_some(),
            "external relay predecessor request is not accepted"
        );
        tx.commit()?;
        Ok(record)
    }

    pub(crate) fn claim_external_relay_read_contact(
        &self,
        intent: &RelayTransferIntent,
    ) -> Result<ClaimedRelayFileContact> {
        let record = self.claim_external_relay_read(intent)?;
        Ok(ClaimedRelayFileContact {
            record,
            effect: ryeos_external_execution_contract::relay_file::RelayFileEffect::Download,
        })
    }

    pub(crate) fn claim_external_relay_upload_contact(
        &self,
        intent: &RelayTransferIntent,
    ) -> Result<ClaimedRelayFileContact> {
        let record = self.claim_external_relay_upload(intent)?;
        Ok(ClaimedRelayFileContact {
            record,
            effect: ryeos_external_execution_contract::relay_file::RelayFileEffect::UploadOnce,
        })
    }

    /// Reserve the one file slot before any Render token mint. Exact replay is
    /// read-only; a different digest cannot take the same sequence/path.
    pub(crate) fn reserve_external_relay_transfer(
        &self,
        bootstrap: &ExternalSupervisorBootstrap,
        intent: &RelayTransferIntent,
        outbound: Option<&[u8]>,
    ) -> Result<RelayTransferRecord> {
        bootstrap.validate()?;
        ensure!(
            bootstrap.controller.relay().is_ok()
                && bootstrap.placement_thread_id == intent.placement_thread_id
                && bootstrap.occurrence_id == intent.occurrence_id
                && bootstrap.allocation_request_digest == intent.allocation_request_digest
                && bootstrap.digest()? == intent.bootstrap_digest,
            "external relay transfer changed its sealed bootstrap"
        );
        let expected = RelayTransferIntent::from_bootstrap(
            bootstrap,
            intent.kind,
            intent.sequence,
            intent.direction,
            outbound,
        )?;
        ensure!(
            expected == *intent,
            "external relay slot differs from sealed bootstrap"
        );
        intent.validate()?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        require_live_q_allocation(&tx, intent)?;
        require_placement_bootstrap(&tx, intent)?;
        if let Some(prior) = read_transfer(&tx, intent)? {
            if intent.direction == RelayDirection::ControllerToGuest {
                ensure!(
                    prior.canonical_bytes.as_deref() == outbound,
                    "external relay exact replay changed retained response bytes"
                );
            }
            tx.commit()?;
            return Ok(prior);
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < intent.deadline_ms,
            "external relay deadline expired before reservation"
        );
        require_preceding_transfer(&tx, intent)?;
        if let Some(bytes) = outbound {
            require_retention_budget(&tx, intent, bytes.len())?;
        }
        tx.execute(
            "INSERT INTO external_execution_relay_transfer VALUES(?1,?2,?3,?4,?5,?6,'reserved',0,NULL,NULL,?7,?8,?8)",
            params![intent.placement_thread_id, intent.kind.as_str(), intent.sequence,
                intent.direction.as_str(), intent.remote_path, canonical(intent)?, outbound, now],
        )?;
        let record =
            read_transfer(&tx, intent)?.context("external relay reservation disappeared")?;
        tx.commit()?;
        Ok(record)
    }

    /// Retain each read allowance before minting a Render download token.
    /// Repeated reads are allowed only for this same fixed path and owner.
    pub(crate) fn claim_external_relay_read(
        &self,
        intent: &RelayTransferIntent,
    ) -> Result<RelayTransferRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        require_live_q_allocation(&tx, intent)?;
        let prior =
            read_transfer(&tx, intent)?.context("external relay has no durable transfer owner")?;
        ensure!(
            (intent.direction == RelayDirection::GuestToController
                && matches!(
                    prior.phase,
                    RelayTransferPhase::Reserved | RelayTransferPhase::Reading
                ))
                || (intent.direction == RelayDirection::ControllerToGuest
                    && prior.phase == RelayTransferPhase::UploadPossible),
            "external relay read is outside its transfer phase"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < intent.deadline_ms && prior.read_attempts < 128,
            "external relay read budget or deadline expired"
        );
        tx.execute(
            "UPDATE external_execution_relay_transfer SET phase=?5,read_attempts=read_attempts+1,updated_at_ms=?6
             WHERE placement_thread_id=?1 AND kind=?2 AND sequence=?3 AND direction=?4",
            params![intent.placement_thread_id, intent.kind.as_str(), intent.sequence,
                intent.direction.as_str(),
                if intent.direction == RelayDirection::GuestToController { "reading" } else { "upload_possible" }, now],
        )?;
        let current =
            read_transfer(&tx, intent)?.context("external relay read claim disappeared")?;
        tx.commit()?;
        Ok(current)
    }

    /// This is the only permission for one upload token mint and PUT. A lost
    /// response leaves UploadPossible; recovery may download and compare the
    /// same file, but must never claim another upload.
    pub(crate) fn claim_external_relay_upload(
        &self,
        intent: &RelayTransferIntent,
    ) -> Result<RelayTransferRecord> {
        ensure!(
            intent.direction == RelayDirection::ControllerToGuest,
            "external relay upload requires a controller response"
        );
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        require_live_q_allocation(&tx, intent)?;
        let prior =
            read_transfer(&tx, intent)?.context("external relay has no durable transfer owner")?;
        ensure!(
            prior.phase == RelayTransferPhase::Reserved,
            "external relay upload was already possible; use exact readback"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < intent.deadline_ms,
            "external relay upload deadline expired"
        );
        tx.execute(
            "UPDATE external_execution_relay_transfer SET phase='upload_possible',updated_at_ms=?5
             WHERE placement_thread_id=?1 AND kind=?2 AND sequence=?3 AND direction=?4",
            params![
                intent.placement_thread_id,
                intent.kind.as_str(),
                intent.sequence,
                intent.direction.as_str(),
                now
            ],
        )?;
        let current =
            read_transfer(&tx, intent)?.context("external relay upload claim disappeared")?;
        tx.commit()?;
        Ok(current)
    }

    /// Exact canonical readback is a transport receipt, never an ACK. For a
    /// guest request the first accepted bytes become immutable; for an owner
    /// response they must equal the precommitted upload bytes.
    pub(crate) fn accept_external_relay_readback(
        &self,
        intent: &RelayTransferIntent,
        bytes: &[u8],
    ) -> Result<RelayTransferRecord> {
        intent.validate()?;
        ensure!(
            !bytes.is_empty() && bytes.len() as u64 <= intent.maximum_file_bytes,
            "external relay readback exceeds file bound"
        );
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        ensure!(
            lillux::canonical_json(&value)?.as_bytes() == bytes,
            "external relay readback is not canonical JSON"
        );
        let digest = lillux::sha256_hex(bytes);
        let len = u64::try_from(bytes.len())?;
        if intent.direction == RelayDirection::ControllerToGuest
            && (intent.expected_sha256.as_deref() != Some(&digest)
                || intent.expected_bytes != Some(len))
        {
            self.quarantine_external_relay_transfer(intent)?;
            anyhow::bail!("external relay upload readback differs from precommitted response");
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        require_live_q_allocation(&tx, intent)?;
        let prior =
            read_transfer(&tx, intent)?.context("external relay has no durable transfer owner")?;
        if prior.phase == RelayTransferPhase::Accepted {
            if prior.observed_sha256.as_deref() != Some(&digest)
                || prior.observed_bytes != Some(len)
            {
                quarantine_in_transaction(&tx, intent)?;
                tx.commit()?;
                anyhow::bail!("external relay accepted file changed its exact bytes");
            }
            tx.commit()?;
            return Ok(prior);
        }
        ensure!(
            prior.read_attempts > 0
                && ((intent.direction == RelayDirection::GuestToController
                    && prior.phase == RelayTransferPhase::Reading)
                    || (intent.direction == RelayDirection::ControllerToGuest
                        && prior.phase == RelayTransferPhase::UploadPossible)),
            "external relay readback has no prior download claim"
        );
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        ensure!(
            now < intent.deadline_ms,
            "external relay readback arrived after deadline"
        );
        if intent.direction == RelayDirection::GuestToController {
            require_retention_budget(&tx, intent, bytes.len())?;
        }
        tx.execute(
            "UPDATE external_execution_relay_transfer
             SET phase='accepted',observed_sha256=?5,observed_bytes=?6,
                 canonical_bytes=COALESCE(canonical_bytes,?7),updated_at_ms=?8
             WHERE placement_thread_id=?1 AND kind=?2 AND sequence=?3 AND direction=?4",
            params![
                intent.placement_thread_id,
                intent.kind.as_str(),
                intent.sequence,
                intent.direction.as_str(),
                digest,
                i64::try_from(len)?,
                bytes,
                now
            ],
        )?;
        let current = read_transfer(&tx, intent)?.context("external relay readback disappeared")?;
        tx.commit()?;
        Ok(current)
    }

    /// The original provider occurrence has settled. This closes further
    /// transfer contact without treating any transfer receipt as terminal
    /// evidence. An incomplete transfer remains visibly incomplete forever.
    pub(crate) fn close_external_relay_after_terminal(
        &self,
        intent: &RelayTransferIntent,
    ) -> Result<RelayTransferRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let allocation = read(&tx, &intent.placement_thread_id)?
            .context("external relay terminal closure lost allocation")?;
        ensure!(
            allocation.phase == ExternalAllocationPhase::Terminated
                && allocation
                    .occurrence
                    .as_ref()
                    .map(|o| o.occurrence_id.as_str())
                    == Some(intent.occurrence_id.as_str()),
            "external relay closure requires exact provider terminal settlement"
        );
        let prior =
            read_transfer(&tx, intent)?.context("external relay terminal closure lost transfer")?;
        if prior.phase == RelayTransferPhase::TerminalClosed {
            tx.commit()?;
            return Ok(prior);
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        tx.execute(
            "UPDATE external_execution_relay_transfer SET phase='terminal_closed',updated_at_ms=?5
             WHERE placement_thread_id=?1 AND kind=?2 AND sequence=?3 AND direction=?4",
            params![
                intent.placement_thread_id,
                intent.kind.as_str(),
                intent.sequence,
                intent.direction.as_str(),
                now
            ],
        )?;
        let closed =
            read_transfer(&tx, intent)?.context("external relay terminal closure disappeared")?;
        tx.commit()?;
        Ok(closed)
    }

    /// A complete but contradictory file, or any other unreconcilable
    /// provider ambiguity, fences the original occurrence. Only exact
    /// provider terminal settlement can later close this transfer.
    pub(crate) fn quarantine_external_relay_transfer(
        &self,
        intent: &RelayTransferIntent,
    ) -> Result<RelayTransferRecord> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let prior =
            read_transfer(&tx, intent)?.context("external relay quarantine lost transfer")?;
        if prior.phase != RelayTransferPhase::Quarantined {
            quarantine_in_transaction(&tx, intent)?;
        }
        let current =
            read_transfer(&tx, intent)?.context("external relay quarantine disappeared")?;
        tx.commit()?;
        Ok(current)
    }
}

fn quarantine_in_transaction(conn: &Connection, intent: &RelayTransferIntent) -> Result<()> {
    let allocation = read(conn, &intent.placement_thread_id)?
        .context("external relay quarantine lost allocation")?;
    ensure!(
        matches!(
            allocation.phase,
            ExternalAllocationPhase::Bound | ExternalAllocationPhase::Quarantined
        ) && allocation
            .occurrence
            .as_ref()
            .map(|o| o.occurrence_id.as_str())
            == Some(intent.occurrence_id.as_str()),
        "external relay quarantine has no unsettled original occurrence"
    );
    let now = i64::try_from(lillux::time::timestamp_millis())?;
    conn.execute(
        "UPDATE external_execution_relay_transfer SET phase='quarantined',updated_at_ms=?5
         WHERE placement_thread_id=?1 AND kind=?2 AND sequence=?3 AND direction=?4
           AND phase IN ('reserved','reading','upload_possible','accepted')",
        params![
            intent.placement_thread_id,
            intent.kind.as_str(),
            intent.sequence,
            intent.direction.as_str(),
            now
        ],
    )?;
    conn.execute(
        "UPDATE external_execution_allocation SET phase='quarantined',updated_at_ms=?2
         WHERE placement_thread_id=?1 AND phase='bound'",
        params![intent.placement_thread_id, now],
    )?;
    Ok(())
}

fn require_placement_bootstrap(conn: &Connection, intent: &RelayTransferIntent) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT intent_json FROM external_execution_relay_transfer WHERE placement_thread_id=?1",
    )?;
    for row in statement.query_map([&intent.placement_thread_id], |row| row.get::<_, String>(0))? {
        let prior: RelayTransferIntent = serde_json::from_str(&row?)?;
        ensure!(
            prior.bootstrap_digest == intent.bootstrap_digest
                && prior.mailbox_root == intent.mailbox_root
                && prior.maximum_file_bytes == intent.maximum_file_bytes
                && prior.retention_budget_bytes == intent.retention_budget_bytes,
            "external relay placement changed its sealed mailbox or byte budget"
        );
    }
    Ok(())
}

fn require_retention_budget(
    conn: &Connection,
    intent: &RelayTransferIntent,
    additional: usize,
) -> Result<()> {
    let used: i64 = conn.query_row(
        "SELECT COALESCE(SUM(length(canonical_bytes)),0) FROM external_execution_relay_transfer
         WHERE placement_thread_id=?1",
        [&intent.placement_thread_id],
        |row| row.get(0),
    )?;
    ensure!(
        used >= 0
            && u64::try_from(used)?.saturating_add(u64::try_from(additional)?)
                <= intent.retention_budget_bytes,
        "external relay retained files exceed signed channel budget"
    );
    Ok(())
}

pub(super) fn validate_transfers(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT placement_thread_id,kind,sequence,direction,intent_json,phase,read_attempts,observed_sha256,observed_bytes,canonical_bytes
         FROM external_execution_relay_transfer"
    )?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u32>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, u32>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, Option<Vec<u8>>>(9)?,
        ))
    })? {
        let (
            placement,
            kind,
            sequence,
            direction,
            raw,
            phase,
            attempts,
            digest,
            bytes,
            canonical_bytes,
        ) = row?;
        let intent: RelayTransferIntent = serde_json::from_str(&raw)?;
        ensure!(
            intent.placement_thread_id == placement
                && intent.kind.as_str() == kind
                && intent.sequence == sequence
                && intent.direction.as_str() == direction
                && canonical(&intent)? == raw,
            "external relay journal changed its exact transfer identity"
        );
        let phase = RelayTransferPhase::parse(&phase)?;
        ensure!(
            attempts <= 128
                && digest.is_some() == bytes.is_some()
                && ((phase == RelayTransferPhase::Accepted) == digest.is_some()
                    || matches!(
                        phase,
                        RelayTransferPhase::Quarantined | RelayTransferPhase::TerminalClosed
                    )),
            "external relay journal has an impossible phase or receipt"
        );
        let current =
            read_transfer(conn, &intent)?.context("external relay journal lost transfer")?;
        ensure!(
            current.canonical_bytes == canonical_bytes,
            "external relay journal changed exact retained bytes"
        );
        let allocation =
            read(conn, &placement)?.context("external relay journal lost allocation")?;
        ensure!(
            allocation.reservation.request_digest == intent.allocation_request_digest
                && allocation
                    .occurrence
                    .as_ref()
                    .map(|o| o.occurrence_id.as_str())
                    == Some(intent.occurrence_id.as_str())
                && matches!(
                    &allocation.reservation.owner,
                    ExternalAllocationOwner::QualificationCandidate(_)
                ),
            "external relay journal changed its Q allocation owner"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_response_bytes_cannot_be_replaced_after_a_possible_upload() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE external_execution_allocation (placement_thread_id TEXT PRIMARY KEY);",
        )
        .unwrap();
        conn.execute_batch(JOURNAL_SQL).unwrap();
        conn.execute(
            "INSERT INTO external_execution_allocation VALUES('placement')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO external_execution_relay_transfer VALUES(
                'placement','attach',0,'controller_to_guest','/relay/attach.response.json',
                '{}','reserved',0,NULL,NULL,?1,1,1)",
            [b"{}".as_slice()],
        )
        .unwrap();
        conn.execute(
            "UPDATE external_execution_relay_transfer
             SET phase='upload_possible',updated_at_ms=2 WHERE placement_thread_id='placement'",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "UPDATE external_execution_relay_transfer SET canonical_bytes=?1
                 WHERE placement_thread_id='placement'",
                [b"{\"different\":true}".as_slice()],
            )
            .is_err()
        );
        assert!(
            conn.execute(
                "UPDATE external_execution_relay_transfer SET phase='reserved'
                 WHERE placement_thread_id='placement'",
                [],
            )
            .is_err()
        );
    }
}
