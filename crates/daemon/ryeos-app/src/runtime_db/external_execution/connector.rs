//! Durable controller-local connector ownership.
//!
//! The local connector is deliberately subordinate to an already released
//! occurrence channel. Preparing it does not prove provider contact. The one
//! successful `prepared -> connected` compare-and-swap is the only durable
//! authority to accept protocol I/O from a peer; a repeated claim is uncertain
//! and never authorizes a replacement connection.

use super::*;
use ryeos_state::external_execution::connector::EXTERNAL_CONNECTOR_PROTOCOL;
use ryeos_state::external_execution::journal::{load_binding, revoked};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalConnectorPhase {
    Prepared,
    Connected,
    Closed,
}

impl ExternalConnectorPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Connected => "connected",
            Self::Closed => "closed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "connected" => Ok(Self::Connected),
            "closed" => Ok(Self::Closed),
            _ => bail!("external connector retained an unknown phase"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalConnectorRecord {
    pub placement_thread_id: String,
    pub channel_binding_digest: String,
    pub execution_binding_hash: String,
    pub connector_protocol: String,
    pub connector_artifact_hash: String,
    pub connector_artifact_bytes: u64,
    pub capability_generation: String,
    pub capability_hash: String,
    pub phase: ExternalConnectorPhase,
    pub peer_process_identity: Option<lillux::ExactProcessIdentity>,
    pub peer_process_identity_digest: Option<String>,
    pub prepared_at_ms: i64,
    pub connected_at_ms: Option<i64>,
    pub closed_at_ms: Option<i64>,
    pub close_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExternalConnectorPreparation {
    Fresh(ExternalConnectorRecord),
    Prior(ExternalConnectorRecord),
}

impl ExternalConnectorRecord {
    fn validate(&self) -> Result<()> {
        validate_bounded_runtime_text(
            "external connector placement",
            &self.placement_thread_id,
            256,
        )?;
        for digest in [
            &self.channel_binding_digest,
            &self.execution_binding_hash,
            &self.connector_artifact_hash,
            &self.capability_generation,
            &self.capability_hash,
        ] {
            validate_sha256("external connector identity", digest)?;
        }
        ensure!(
            self.connector_protocol == EXTERNAL_CONNECTOR_PROTOCOL
                && (1..=1024 * 1024 * 1024).contains(&self.connector_artifact_bytes)
                && self.prepared_at_ms > 0,
            "external connector contract is outside its bounds"
        );
        match self.phase {
            ExternalConnectorPhase::Prepared => ensure!(
                self.peer_process_identity.is_none()
                    && self.peer_process_identity_digest.is_none()
                    && self.connected_at_ms.is_none()
                    && self.closed_at_ms.is_none()
                    && self.close_reason.is_none(),
                "prepared external connector retained terminal evidence"
            ),
            ExternalConnectorPhase::Connected => ensure!(
                self.peer_process_identity.is_some()
                    && self.peer_process_identity_digest.is_some()
                    && self.connected_at_ms.is_some()
                    && self.closed_at_ms.is_none()
                    && self.close_reason.is_none(),
                "connected external connector lacks exact peer evidence"
            ),
            ExternalConnectorPhase::Closed => ensure!(
                self.closed_at_ms.is_some() && self.close_reason.is_some(),
                "closed external connector lacks terminal evidence"
            ),
        }
        if let Some(identity) = &self.peer_process_identity {
            ensure!(
                identity.incarnation_digest().map_err(anyhow::Error::msg)?
                    == *self
                        .peer_process_identity_digest
                        .as_ref()
                        .context("external connector peer digest is absent")?,
                "external connector peer identity digest changed"
            );
        } else {
            ensure!(
                self.peer_process_identity_digest.is_none() && self.connected_at_ms.is_none(),
                "external connector retained a partial peer identity"
            );
        }
        if let Some(reason) = &self.close_reason {
            validate_bounded_runtime_text("external connector close reason", reason, 64)?;
            ensure!(
                matches!(
                    reason.as_str(),
                    "completed"
                        | "disconnected"
                        | "authentication_refused"
                        | "execution_revoked"
                        | "startup_abandoned"
                        | "controller_shutdown"
                        | "protocol_fault"
                ),
                "external connector close reason is unsupported"
            );
        }
        Ok(())
    }
}

fn read_connector(conn: &Connection, placement: &str) -> Result<Option<ExternalConnectorRecord>> {
    let row: Option<(
        String,
        String,
        String,
        String,
        i64,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        i64,
        Option<i64>,
        Option<i64>,
        Option<String>,
    )> = conn
        .query_row(
            "SELECT channel_binding_digest,execution_binding_hash,connector_protocol,
                    connector_artifact_hash,connector_artifact_bytes,capability_generation,
                    capability_hash,state,peer_process_identity_json,
                    peer_process_identity_digest,prepared_at_ms,connected_at_ms,
                    closed_at_ms,close_reason
               FROM external_execution_connector WHERE placement_thread_id=?1",
            [placement],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(
            channel_binding_digest,
            execution_binding_hash,
            connector_protocol,
            connector_artifact_hash,
            connector_artifact_bytes,
            capability_generation,
            capability_hash,
            phase,
            peer_process_identity_json,
            peer_process_identity_digest,
            prepared_at_ms,
            connected_at_ms,
            closed_at_ms,
            close_reason,
        )| {
            let peer_process_identity = peer_process_identity_json
                .map(|json| {
                    let identity: lillux::ExactProcessIdentity = serde_json::from_str(&json)
                        .context("decode external connector peer identity")?;
                    ensure!(
                        lillux::canonical_json(&serde_json::to_value(&identity)?)? == json,
                        "external connector peer identity is not canonical"
                    );
                    Ok(identity)
                })
                .transpose()?;
            let record = ExternalConnectorRecord {
                placement_thread_id: placement.to_owned(),
                channel_binding_digest,
                execution_binding_hash,
                connector_protocol,
                connector_artifact_hash,
                connector_artifact_bytes: u64::try_from(connector_artifact_bytes)?,
                capability_generation,
                capability_hash,
                phase: ExternalConnectorPhase::parse(&phase)?,
                peer_process_identity,
                peer_process_identity_digest,
                prepared_at_ms,
                connected_at_ms,
                closed_at_ms,
                close_reason,
            };
            record.validate()?;
            Ok(record)
        },
    )
    .transpose()
}

fn derive_capability_generation(
    placement: &str,
    channel: &ryeos_state::external_execution::ExecutionChannelBinding,
    contract: &crate::node_config::sections::external_execution::ExternalPlacementBackendContract,
) -> Result<String> {
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.external-connector-capability-generation.v1",
        "placement_thread_id": placement,
        "channel_binding_digest": channel.digest()?,
        "execution_binding_hash": channel.execution_binding_hash,
        "connector_protocol": contract.workload.structured_session()?.connector_protocol,
        "connector_artifact_hash": contract.workload.structured_session()?.connector_artifact_hash,
        "connector_artifact_bytes": contract.workload.structured_session()?.connector_artifact_bytes,
    }))
}

fn require_live_released_channel(
    conn: &Connection,
    placement: &str,
    channel: &ryeos_state::external_execution::ExecutionChannelBinding,
) -> Result<()> {
    let channel_binding_digest = channel.digest()?;
    let channel_state: String = conn.query_row(
        "SELECT state FROM external_execution_channel WHERE placement_thread_id=?1",
        [placement],
        |row| row.get(0),
    )?;
    ensure!(
        channel_state == "running",
        "external connector requires a currently running released channel"
    );
    ensure!(
        !revoked(conn, &channel_binding_digest)?,
        "revoked external execution cannot admit a connector"
    );
    ensure!(
        lillux::time::timestamp_millis() < channel.execution_deadline_ms,
        "external execution deadline passed before connector admission"
    );
    let release_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM external_execution_frame
          WHERE binding_digest=?1 AND direction='owner_to_supervisor'
            AND json_extract(frame_json,'$.frame.payload.kind')='release')",
        [&channel_binding_digest],
        |row| row.get(0),
    )?;
    ensure!(
        release_exists,
        "external connector requires exact release evidence"
    );
    Ok(())
}

/// Connection occurs while the admitted provider handles a dispatched
/// command, after worker binding. It is not another pre-launch allocation.
fn require_connecting_session(conn: &Connection, placement: &str) -> Result<()> {
    let live: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM dedicated_session s
           JOIN worker_process p ON p.worker_instance_id=s.worker_instance_id
             AND p.boot_epoch=s.worker_boot_epoch
             AND p.placement_thread_id=s.placement_thread_id
             AND p.session_capsule_hash=s.admitted_capsule_hash
           JOIN execution_workspace w ON w.workspace_id=s.workspace_id
           JOIN credential_profile c ON c.profile_id=s.credential_profile_id
           WHERE s.placement_thread_id=?1 AND s.state='idle'
             AND s.send_boundary='contacted' AND s.scope_retirement IS NULL
             AND s.bounded_outcome_json IS NULL
             AND p.state='live' AND p.cleanup_state='owned'
             AND w.state='active' AND w.process_identity=p.process_identity
             AND c.credential_generation=s.credential_generation
             AND c.lock_owner=s.worker_instance_id
             AND c.state IN ('unauthenticated','enrolling','confirming','active')
             AND EXISTS(SELECT 1 FROM dedicated_session_command d
               WHERE d.placement_thread_id=s.placement_thread_id
                 AND d.worker_boot_epoch=s.worker_boot_epoch
                 AND d.state='dispatched'))",
        [placement],
        |row| row.get(0),
    )?;
    ensure!(
        live,
        "external connector requires its live contacting session"
    );
    Ok(())
}

fn expected_connector(
    conn: &Connection,
    placement: &str,
    capability_hash: Option<&str>,
) -> Result<ExternalConnectorRecord> {
    let allocation = read(conn, placement)?.context("external connector allocation is absent")?;
    ensure!(
        allocation.phase == ExternalAllocationPhase::Bound,
        "external connector requires a live bound occurrence"
    );
    require_session_owner(conn, &allocation.reservation)?;
    require_contactable_session(conn, placement)?;
    let channel = load_binding(conn, placement)?;
    let channel_binding_digest = channel.digest()?;
    ensure!(
        channel.execution_binding_hash == allocation.reservation.binding_hash,
        "external connector channel changed its placement binding"
    );
    require_live_released_channel(conn, placement, &channel)?;
    let retained = read_retained_binding(conn, &allocation.reservation.binding_hash)?
        .context("external connector lost its retained placement binding")?;
    let contract = retained.backend_contract();
    ensure!(
        contract.workload.structured_session()?.connector_protocol == EXTERNAL_CONNECTOR_PROTOCOL,
        "external connector protocol is unsupported"
    );
    let generation = derive_capability_generation(placement, &channel, &contract)?;
    let record = ExternalConnectorRecord {
        placement_thread_id: placement.to_owned(),
        channel_binding_digest,
        execution_binding_hash: channel.execution_binding_hash,
        connector_protocol: contract
            .workload
            .structured_session()?
            .connector_protocol
            .clone(),
        connector_artifact_hash: contract
            .workload
            .structured_session()?
            .connector_artifact_hash
            .clone(),
        connector_artifact_bytes: contract
            .workload
            .structured_session()?
            .connector_artifact_bytes,
        capability_generation: generation,
        capability_hash: capability_hash.unwrap_or(&"0".repeat(64)).to_owned(),
        phase: ExternalConnectorPhase::Prepared,
        peer_process_identity: None,
        peer_process_identity_digest: None,
        prepared_at_ms: 1,
        connected_at_ms: None,
        closed_at_ms: None,
        close_reason: None,
    };
    record.validate()?;
    Ok(record)
}

pub(super) fn validate_connectors(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT placement_thread_id FROM external_execution_connector ORDER BY placement_thread_id",
    )?;
    let placements = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for placement in placements {
        let record = read_connector(conn, &placement)?
            .context("external connector disappeared during validation")?;
        let allocation =
            read(conn, &placement)?.context("external connector lost its allocation")?;
        let channel = load_binding(conn, &placement)?;
        let retained = read_retained_binding(conn, &allocation.reservation.binding_hash)?
            .context("external connector lost its placement binding")?;
        let contract = retained.backend_contract();
        ensure!(
            record.channel_binding_digest == channel.digest()?
                && record.execution_binding_hash == channel.execution_binding_hash
                && record.execution_binding_hash == allocation.reservation.binding_hash
                && record.connector_protocol
                    == contract.workload.structured_session()?.connector_protocol
                && record.connector_artifact_hash
                    == contract
                        .workload
                        .structured_session()?
                        .connector_artifact_hash
                && record.connector_artifact_bytes
                    == contract
                        .workload
                        .structured_session()?
                        .connector_artifact_bytes,
            "external connector changed its retained execution authority"
        );
        ensure!(
            record.capability_generation
                == derive_capability_generation(&placement, &channel, &contract)?,
            "external connector changed its capability generation"
        );
        if allocation.phase.is_settled() {
            ensure!(
                record.phase == ExternalConnectorPhase::Closed,
                "settled external allocation retains a live connector"
            );
        }
    }
    Ok(())
}

impl RuntimeDb {
    pub(crate) fn external_connector_capability_generation(
        &self,
        placement: &str,
    ) -> Result<String> {
        Ok(expected_connector(&self.conn, placement, None)?.capability_generation)
    }

    pub(crate) fn external_connector(
        &self,
        placement: &str,
    ) -> Result<Option<ExternalConnectorRecord>> {
        read_connector(&self.conn, placement)
    }

    pub(crate) fn prepare_external_connector(
        &self,
        placement: &str,
        capability_generation: &str,
        capability_hash: &str,
    ) -> Result<ExternalConnectorPreparation> {
        validate_sha256(
            "external connector capability generation",
            capability_generation,
        )?;
        validate_sha256("external connector capability", capability_hash)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut expected = expected_connector(&tx, placement, Some(capability_hash))?;
        ensure!(
            expected.capability_generation == capability_generation,
            "external connector capability generation changed"
        );
        if let Some(prior) = read_connector(&tx, placement)? {
            ensure!(
                prior.placement_thread_id == expected.placement_thread_id
                    && prior.channel_binding_digest == expected.channel_binding_digest
                    && prior.execution_binding_hash == expected.execution_binding_hash
                    && prior.connector_protocol == expected.connector_protocol
                    && prior.connector_artifact_hash == expected.connector_artifact_hash
                    && prior.connector_artifact_bytes == expected.connector_artifact_bytes
                    && prior.capability_generation == expected.capability_generation
                    && prior.capability_hash == expected.capability_hash,
                "external connector preparation replay changed authority"
            );
            tx.commit()?;
            return Ok(ExternalConnectorPreparation::Prior(prior));
        }
        expected.prepared_at_ms = i64::try_from(lillux::time::timestamp_millis())?;
        tx.execute(
            "INSERT INTO external_execution_connector VALUES(
                ?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL,NULL,?10,NULL,NULL,NULL)",
            params![
                expected.placement_thread_id,
                expected.channel_binding_digest,
                expected.execution_binding_hash,
                expected.connector_protocol,
                expected.connector_artifact_hash,
                i64::try_from(expected.connector_artifact_bytes)?,
                expected.capability_generation,
                expected.capability_hash,
                expected.phase.as_str(),
                expected.prepared_at_ms,
            ],
        )?;
        tx.commit()?;
        Ok(ExternalConnectorPreparation::Fresh(expected))
    }

    /// Returns `true` only to the transaction that acquired the one allowed
    /// connection. `false` is durable uncertainty and grants no reconnect.
    pub(crate) fn connect_external_connector(
        &self,
        placement: &str,
        capability_generation: &str,
        capability_hash: &str,
        peer: &lillux::ExactProcessIdentity,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record =
            read_connector(&tx, placement)?.context("external connector is not prepared")?;
        ensure!(
            record.capability_generation == capability_generation
                && record.capability_hash == capability_hash,
            "external connector capability changed"
        );
        let peer_digest = peer.incarnation_digest().map_err(anyhow::Error::msg)?;
        let peer_json = lillux::canonical_json(&serde_json::to_value(peer)?)?;
        match record.phase {
            ExternalConnectorPhase::Prepared => {
                let allocation =
                    read(&tx, placement)?.context("external connector allocation disappeared")?;
                ensure!(
                    allocation.phase == ExternalAllocationPhase::Bound,
                    "external connector cannot attach after cleanup begins"
                );
                require_session_owner(&tx, &allocation.reservation)?;
                require_connecting_session(&tx, placement)?;
                let channel = load_binding(&tx, placement)?;
                require_live_released_channel(&tx, placement, &channel)?;
                let retained = read_retained_binding(&tx, &allocation.reservation.binding_hash)?
                    .context("external connector lost its placement binding")?;
                ensure!(
                    record.capability_generation
                        == derive_capability_generation(
                            placement,
                            &channel,
                            &retained.backend_contract()
                        )?,
                    "external connector capability generation changed"
                );
                let now = i64::try_from(lillux::time::timestamp_millis())?;
                ensure!(
                    tx.execute(
                        "UPDATE external_execution_connector
                            SET state='connected',peer_process_identity_json=?2,
                                peer_process_identity_digest=?3,connected_at_ms=?4
                          WHERE placement_thread_id=?1 AND state='prepared'",
                        params![placement, peer_json, peer_digest, now],
                    )? == 1,
                    "external connector connection claim lost its compare-and-swap"
                );
                tx.commit()?;
                Ok(true)
            }
            ExternalConnectorPhase::Connected => {
                ensure!(
                    record.peer_process_identity.as_ref() == Some(peer)
                        && record.peer_process_identity_digest.as_deref() == Some(&peer_digest),
                    "external connector reconnect changed its authenticated peer"
                );
                tx.commit()?;
                Ok(false)
            }
            ExternalConnectorPhase::Closed => bail!("closed external connector cannot reconnect"),
        }
    }

    pub(crate) fn close_external_connector(
        &self,
        placement: &str,
        reason: &str,
    ) -> Result<ExternalConnectorRecord> {
        validate_bounded_runtime_text("external connector close reason", reason, 64)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let record = read_connector(&tx, placement)?.context("external connector is absent")?;
        if record.phase == ExternalConnectorPhase::Closed {
            ensure!(
                record.close_reason.as_deref() == Some(reason),
                "external connector close replay changed its reason"
            );
            tx.commit()?;
            return Ok(record);
        }
        let now = i64::try_from(lillux::time::timestamp_millis())?;
        tx.execute(
            "UPDATE external_execution_connector SET state='closed',closed_at_ms=?2,close_reason=?3
              WHERE placement_thread_id=?1 AND state IN ('prepared','connected')",
            params![placement, now, reason],
        )?;
        let closed =
            read_connector(&tx, placement)?.context("closed external connector disappeared")?;
        closed.validate()?;
        tx.commit()?;
        Ok(closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn released(db: &RuntimeDb, suffix: &str) -> (String, String) {
        let (binding, _) = released_authority(db, suffix);
        let digest = binding.digest().unwrap();
        (binding.placement_thread_id, digest)
    }

    fn released_authority(
        db: &RuntimeDb,
        suffix: &str,
    ) -> (
        ryeos_state::external_execution::ExecutionChannelBinding,
        lillux::crypto::SigningKey,
    ) {
        let (_, _, _, binding, owner, supervisor) = super::super::channel::tests::pending_channel(
            db,
            suffix,
            &format!("occurrence-{suffix}"),
            100,
            1024 * 1024,
        );
        let startup_deadline =
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(60));
        db.register_external_execution_channel(&binding).unwrap();
        super::super::channel::tests::ready(db, &binding, &supervisor);
        db.admit_external_ready_and_author_release(
            &binding.placement_thread_id,
            &owner,
            startup_deadline,
        )
        .unwrap()
        .unwrap();
        (binding, owner)
    }

    fn peer(pid: u32, ticks: u64) -> lillux::ExactProcessIdentity {
        lillux::ExactProcessIdentity {
            boot_id: "test-boot".into(),
            target_pid: pid,
            target_start_time_ticks: ticks,
            group_leader_pid: pid,
            group_leader_start_time_ticks: ticks,
        }
    }

    fn contact_provider(db: &RuntimeDb, placement: &str) {
        let session = db.dedicated_session(placement).unwrap().unwrap();
        let worker = session.worker_instance_id.unwrap();
        let epoch = session.worker_boot_epoch.unwrap();
        db.attach_worker_process(&WorkerProcessRecord {
            worker_instance_id: worker.clone(),
            boot_identity_hash: "b".repeat(64),
            session_capsule_hash: session.admitted_capsule_hash,
            boot_epoch: epoch,
            lifecycle_generation: 1,
            process_identity: ExecutionProcessIdentity {
                schema_version: PROCESS_IDENTITY_SCHEMA_VERSION,
                process_scope: None,
                boot_id: "test-boot".into(),
                target_pid: 5000,
                target_start_time_ticks: 10,
                group_leader_pid: 5000,
                group_leader_start_time_ticks: 10,
                resource_selections: Vec::new(),
                resource_settlement_authority: None,
                resource_operations: Vec::new(),
                resource_allocation_limit: None,
                resource_occupancy_start: None,
                resource_occupancy_limit: None,
                resource_cleanup_allowance_ms: None,
            },
            control_channel_identity: "fd:connector-test".into(),
            state: WorkerProcessState::Attached,
            daemon_generation_id: "connector-test-daemon".into(),
            placement_thread_id: placement.into(),
            cleanup_state: "owned".into(),
            created_at_ms: 2,
            updated_at_ms: 2,
        })
        .unwrap();
        db.complete_worker_binding(&worker, placement, epoch)
            .unwrap();
        let command = db
            .reserve_dedicated_session_command(NewDedicatedSessionCommand {
                placement_thread_id: placement,
                idempotency_key: "connector-start",
                worker_boot_epoch: epoch,
                command_kind: "route",
                request_digest: &"d".repeat(64),
                payload: &serde_json::json!({}),
            })
            .unwrap();
        db.mark_dedicated_command_contacted(placement, command.command_sequence, epoch)
            .unwrap();
    }

    #[test]
    fn connector_preparation_requires_release_and_replays_exactly() {
        let unreleased_root = tempfile::tempdir().unwrap();
        let unreleased_db =
            RuntimeDb::open(&unreleased_root.path().join("runtime.sqlite3")).unwrap();
        let (_, _, _, binding, _, _) = super::super::channel::tests::pending_channel(
            &unreleased_db,
            "unreleased-connector",
            "occurrence-unreleased",
            100,
            1024 * 1024,
        );
        unreleased_db
            .register_external_execution_channel(&binding)
            .unwrap();
        assert!(
            unreleased_db
                .external_connector_capability_generation(&binding.placement_thread_id)
                .is_err()
        );

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (placement, channel_digest) = released(&db, "prepared-connector");
        let generation = db
            .external_connector_capability_generation(&placement)
            .unwrap();
        let prepared = db
            .prepare_external_connector(&placement, &generation, &"a".repeat(64))
            .unwrap();
        let ExternalConnectorPreparation::Fresh(prepared) = prepared else {
            panic!("first connector preparation was not fresh")
        };
        assert_eq!(prepared.phase, ExternalConnectorPhase::Prepared);
        assert_eq!(prepared.channel_binding_digest, channel_digest);
        let replay = db
            .prepare_external_connector(&placement, &generation, &"a".repeat(64))
            .unwrap();
        assert_eq!(
            replay,
            ExternalConnectorPreparation::Prior(prepared.clone())
        );
        assert!(
            db.prepare_external_connector(&placement, &generation, &"b".repeat(64))
                .is_err()
        );
        assert!(
            db.prepare_external_connector(&placement, &"c".repeat(64), &"a".repeat(64))
                .is_err()
        );
        drop(db);

        let reopened = RuntimeDb::open(&path).unwrap();
        assert_eq!(
            reopened.external_connector(&placement).unwrap(),
            Some(prepared)
        );
    }

    #[test]
    fn connector_connection_is_one_use_and_disconnect_is_terminal() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (placement, _) = released(&db, "connected-connector");
        let generation = db
            .external_connector_capability_generation(&placement)
            .unwrap();
        let capability_hash = "a".repeat(64);
        db.prepare_external_connector(&placement, &generation, &capability_hash)
            .unwrap();
        let first_peer = peer(3210, 99);
        // Preparing the connector does not authorize a pre-launch connection.
        assert!(
            db.connect_external_connector(&placement, &generation, &capability_hash, &first_peer)
                .is_err()
        );
        contact_provider(&db, &placement);
        assert!(
            db.connect_external_connector(&placement, &generation, &capability_hash, &first_peer,)
                .unwrap()
        );
        assert!(
            !db.connect_external_connector(&placement, &generation, &capability_hash, &first_peer,)
                .unwrap()
        );
        drop(db);
        let db = RuntimeDb::open(&path).unwrap();
        assert!(
            !db.connect_external_connector(&placement, &generation, &capability_hash, &first_peer,)
                .unwrap()
        );
        assert!(
            db.connect_external_connector(
                &placement,
                &generation,
                &capability_hash,
                &peer(3211, 100),
            )
            .is_err()
        );
        let closed = db
            .close_external_connector(&placement, "disconnected")
            .unwrap();
        assert_eq!(closed.phase, ExternalConnectorPhase::Closed);
        assert_eq!(
            db.close_external_connector(&placement, "disconnected")
                .unwrap(),
            closed
        );
        assert!(
            db.close_external_connector(&placement, "completed")
                .is_err()
        );
        assert!(
            db.connect_external_connector(&placement, &generation, &capability_hash, &first_peer,)
                .is_err()
        );
        drop(db);
        let reopened = RuntimeDb::open(&path).unwrap();
        assert_eq!(
            reopened.external_connector(&placement).unwrap(),
            Some(closed)
        );
    }

    #[test]
    fn connection_refuses_lost_worker_workspace_and_command_authority() {
        for mutation in [
            "UPDATE worker_process SET cleanup_state='unproved'",
            "UPDATE worker_process SET state='draining'",
            "UPDATE worker_process SET boot_epoch=2",
            "UPDATE execution_workspace SET process_identity=NULL",
            "UPDATE dedicated_session SET state='recovering'",
            "UPDATE dedicated_session SET state='outcome_unknown', send_boundary='outcome_unknown'",
            "UPDATE dedicated_session SET send_boundary='settled'",
            "UPDATE dedicated_session_command SET state='completed'",
            "UPDATE credential_profile SET lock_owner=NULL",
        ] {
            let root = tempfile::tempdir().unwrap();
            let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
            let (placement, _) = released(&db, "connection-authority");
            let generation = db
                .external_connector_capability_generation(&placement)
                .unwrap();
            let capability_hash = "a".repeat(64);
            db.prepare_external_connector(&placement, &generation, &capability_hash)
                .unwrap();
            contact_provider(&db, &placement);
            if mutation == "UPDATE credential_profile SET lock_owner=NULL" {
                // This authority loss is prohibited even before connection:
                // active external cleanup retains the credential lock.
                let error = db.conn.execute(mutation, []).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("external execution cleanup retains credential ownership")
                );
                continue;
            }
            db.conn.execute(mutation, []).unwrap();
            assert!(
                db.connect_external_connector(
                    &placement,
                    &generation,
                    &capability_hash,
                    &peer(4000, 101)
                )
                .is_err(),
                "{mutation}"
            );
            assert_eq!(
                db.external_connector(&placement).unwrap().unwrap().phase,
                ExternalConnectorPhase::Prepared
            );
        }
    }

    #[test]
    fn stale_released_channel_cannot_consume_the_connection_claim() {
        for state in ["quiescing", "stopping"] {
            let root = tempfile::tempdir().unwrap();
            let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
            let (placement, _) = released(&db, &format!("stale-{state}"));
            let generation = db
                .external_connector_capability_generation(&placement)
                .unwrap();
            let capability_hash = "a".repeat(64);
            db.prepare_external_connector(&placement, &generation, &capability_hash)
                .unwrap();
            db.conn
                .execute(
                    "UPDATE external_execution_channel SET state=?2 WHERE placement_thread_id=?1",
                    params![placement, state],
                )
                .unwrap();
            contact_provider(&db, &placement);
            assert!(
                db.connect_external_connector(
                    &placement,
                    &generation,
                    &capability_hash,
                    &peer(4000, 101),
                )
                .is_err()
            );
            assert_eq!(
                db.external_connector(&placement).unwrap().unwrap().phase,
                ExternalConnectorPhase::Prepared
            );
        }

        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, owner) = released_authority(&db, "revoked-connector");
        let placement = binding.placement_thread_id;
        let generation = db
            .external_connector_capability_generation(&placement)
            .unwrap();
        let capability_hash = "a".repeat(64);
        db.prepare_external_connector(&placement, &generation, &capability_hash)
            .unwrap();
        db.author_external_owner_revocation(&placement, &owner)
            .unwrap();
        contact_provider(&db, &placement);
        assert!(
            db.connect_external_connector(
                &placement,
                &generation,
                &capability_hash,
                &peer(4001, 102),
            )
            .is_err()
        );
        assert_eq!(
            db.external_connector(&placement).unwrap().unwrap().phase,
            ExternalConnectorPhase::Prepared
        );

        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (binding, _) = released_authority(&db, "expired-connector");
        let mut expired = binding.clone();
        expired.issued_at_ms = 1;
        expired.execution_deadline_ms = 2;
        expired.expires_at_ms = 3;
        assert!(
            require_live_released_channel(&db.conn, &binding.placement_thread_id, &expired)
                .is_err()
        );

        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (placement, _) = released(&db, "orphaned-connector");
        let generation = db
            .external_connector_capability_generation(&placement)
            .unwrap();
        let capability_hash = "a".repeat(64);
        db.prepare_external_connector(&placement, &generation, &capability_hash)
            .unwrap();
        contact_provider(&db, &placement);
        db.conn
            .execute("UPDATE execution_workspace SET state='orphaned'", [])
            .unwrap();
        assert!(
            db.connect_external_connector(
                &placement,
                &generation,
                &capability_hash,
                &peer(4002, 103),
            )
            .is_err()
        );
    }

    #[test]
    fn recovery_rederives_connector_capability_generation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("runtime.sqlite3");
        let db = RuntimeDb::open(&path).unwrap();
        let (placement, _) = released(&db, "generation-corruption");
        let generation = db
            .external_connector_capability_generation(&placement)
            .unwrap();
        db.prepare_external_connector(&placement, &generation, &"a".repeat(64))
            .unwrap();
        let trigger_sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger'
                   AND name='external_execution_connector_transition_guard'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        db.conn
            .execute_batch("DROP TRIGGER external_execution_connector_transition_guard")
            .unwrap();
        db.conn
            .execute(
                "UPDATE external_execution_connector SET capability_generation=?2
                  WHERE placement_thread_id=?1",
                params![placement, "b".repeat(64)],
            )
            .unwrap();
        db.conn.execute_batch(&trigger_sql).unwrap();
        assert!(
            validate_connectors(&db.conn)
                .unwrap_err()
                .to_string()
                .contains("capability generation")
        );
        drop(db);
        assert!(RuntimeDb::open(&path).is_err());
    }

    #[test]
    fn live_connector_blocks_capacity_settlement_and_rows_are_immutable() {
        let root = tempfile::tempdir().unwrap();
        let db = RuntimeDb::open(&root.path().join("runtime.sqlite3")).unwrap();
        let (placement, _) = released(&db, "settlement-connector");
        let generation = db
            .external_connector_capability_generation(&placement)
            .unwrap();
        db.prepare_external_connector(&placement, &generation, &"a".repeat(64))
            .unwrap();
        assert!(
            db.conn
                .execute(
                    "UPDATE external_execution_allocation SET phase='terminated',updated_at_ms=2
                      WHERE placement_thread_id=?1",
                    [&placement],
                )
                .is_err()
        );
        assert!(
            db.conn
                .execute(
                    "UPDATE external_execution_connector SET connector_artifact_hash=?2
                      WHERE placement_thread_id=?1",
                    params![placement, "b".repeat(64)],
                )
                .is_err()
        );
        assert!(
            db.conn
                .execute(
                    "DELETE FROM external_execution_connector WHERE placement_thread_id=?1",
                    [&placement],
                )
                .is_err()
        );
        assert!(
            db.close_external_connector(&placement, "unknown-reason")
                .is_err()
        );
        db.close_external_connector(&placement, "startup_abandoned")
            .unwrap();
    }
}
