//! Read-only evidence for one returned ordinary execution, not a second owner.
use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_app::runtime_db::external_execution::{
    ExternalAllocationOccurrence, ExternalAllocationOwner, ExternalAllocationReservation,
};
use ryeos_state::external_execution::{
    ChannelDirection, ExecutionChannelBinding, ExecutionChannelPayload,
    ExternalCommandOutputStream, ExternalCommandTerminationReason, ExternalTargetExit,
    SignedExecutionFrame,
};
use serde_json::Value;
use std::path::Path;

pub fn verify(
    state: &Path,
    thread: &Value,
    result: &Value,
    artifacts: &Value,
    snapshot: &str,
) -> Result<()> {
    let id = thread["thread_id"]
        .as_str()
        .context("returned ordinary thread")?;
    let capsule_hash = thread["admitted_launch_capsule_hash"]
        .as_str()
        .context("returned capsule")?;
    ensure!(thread["chain_root_id"] == id, "ordinary root changed chain");
    let capsule = ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(
        lillux::CasStore::new(state.join(".ai/state/objects"))
            .get_object(capsule_hash)?
            .context("retained capsule")?,
    )?;
    capsule.validate()?;
    ensure!(
        serde_json::to_value(&capsule.project_authority)? == thread["project_authority"],
        "returned project differs from capsule"
    );
    ensure!(
        matches!(&capsule.project_authority,
        ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
            base_snapshot_hash, snapshot_hash,
            realization: ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            environment: ryeos_state::objects::EnvironmentAuthority::None,
            workspace_outputs: None, ..
        } if base_snapshot_hash == snapshot && snapshot_hash == snapshot),
        "ordinary project authority changed"
    );
    ensure!(
        thread["result_project_snapshot_hash"].is_null(),
        "ordinary execution published a project generation"
    );
    ensure!(
        artifacts == &serde_json::json!([]) && thread["final_cost"].is_null(),
        "ordinary result added artifacts or cost"
    );
    let db = rusqlite::Connection::open_with_flags(
        state.join(".ai/state/runtime.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (phase, reservation, occurrence): (String, String, String) = db.query_row(
        "SELECT phase,reservation_json,occurrence_json FROM external_execution_allocation WHERE placement_thread_id=?1", [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
    ensure!(
        phase == "terminated",
        "external occurrence has no proved termination"
    );
    let reservation: ExternalAllocationReservation = serde_json::from_str(&reservation)?;
    reservation.validate()?;
    let occurrence: ExternalAllocationOccurrence = serde_json::from_str(&occurrence)?;
    ensure!(
        reservation.placement_thread_id == id
            && reservation.admitted_capsule_hash == capsule_hash
            && reservation.base_snapshot_hash == snapshot
            && occurrence.binding_hash == reservation.binding_hash
            && occurrence.request_digest == reservation.request_digest,
        "allocation identity mismatch"
    );
    let ExternalAllocationOwner::DirectThread {
        chain_root_id,
        program,
        ..
    } = &reservation.owner
    else {
        anyhow::bail!("ordinary allocation became a session");
    };
    ensure!(chain_root_id == id, "allocation chain mismatch");
    program.validate()?;
    let ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor { command, .. } =
        &capsule.execution_closure
    else {
        anyhow::bail!("ordinary capsule lost direct closure");
    };
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        execution_plan_hash,
        ..
    } = &capsule.artifact_identity
    else {
        anyhow::bail!("ordinary capsule lost direct artifact");
    };
    ensure!(
        program.command() == command
            && program.execution_plan_hash() == execution_plan_hash
            && program.execution_closure_digest()
                == ryeos_state::objects::canonical_value_digest(&serde_json::to_value(
                    &capsule.execution_closure
                )?)?
            && program.projection().endpoint_binding_digest == reservation.binding_hash,
        "retained program differs from born capsule"
    );
    let (digest, binding, phase): (String, String, String) = db.query_row(
        "SELECT binding_digest,binding_json,state FROM external_execution_channel WHERE placement_thread_id=?1", [id],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    let binding: ExecutionChannelBinding = serde_json::from_str(&binding)?;
    binding.validate()?;
    ensure!(
        binding.digest()? == digest
            && binding.placement_thread_id == id
            && binding.admitted_capsule_hash == capsule_hash
            && binding.base_snapshot_hash == snapshot
            && binding.occurrence_id == occurrence.occurrence_id
            && binding.allocation_request_digest == reservation.request_digest
            && binding.execution_binding_hash == reservation.binding_hash
            && binding.candidate_program_digest == program.digest()?
            && binding.candidate_export_max_bytes == 0
            && matches!(phase.as_str(), "stopping" | "stopped"),
        "channel does not join exact terminated program"
    );
    let mut statement = db.prepare("SELECT frame_json,frame_digest,application FROM external_execution_frame WHERE binding_digest=?1 ORDER BY ordinal")?;
    let rows = statement.query_map([&digest], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let (mut ready, mut release, mut terminal) = (0, 0, None);
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    for row in rows {
        let (wire, retained_digest, application) = row?;
        let signed = SignedExecutionFrame::decode_and_verify(
            wire.as_bytes(),
            &binding,
            binding.issued_at_ms,
        )?;
        ensure!(
            signed.digest() == retained_digest,
            "retained signed frame digest changed"
        );
        let direction = signed.frame().direction;
        match &signed.frame().payload {
            ExecutionChannelPayload::Ready {
                supervisor_runtime_hash,
                base_snapshot_hash,
            } => {
                ensure!(
                    supervisor_runtime_hash == &binding.supervisor_runtime_hash
                        && base_snapshot_hash == &binding.base_snapshot_hash,
                    "Ready changed admitted runtime or base"
                );
                ensure!(
                    direction == ChannelDirection::SupervisorToOwner && application == "applied",
                    "Ready not applied"
                );
                ready += 1;
            }
            ExecutionChannelPayload::Release => {
                ensure!(
                    direction == ChannelDirection::OwnerToSupervisor && application == "applied",
                    "Release not applied"
                );
                release += 1;
            }
            ExecutionChannelPayload::CommandOutput {
                stream,
                offset,
                bytes_base64,
            } => {
                ensure!(
                    direction == ChannelDirection::SupervisorToOwner && application == "applied",
                    "output not applied"
                );
                let destination = match stream {
                    ExternalCommandOutputStream::Stdout => &mut stdout,
                    ExternalCommandOutputStream::Stderr => &mut stderr,
                };
                ensure!(*offset == destination.len() as u64, "noncontiguous output");
                destination.extend(STANDARD.decode(bytes_base64)?);
            }
            ExecutionChannelPayload::CommandTerminated { observation } => {
                ensure!(
                    direction == ChannelDirection::SupervisorToOwner
                        && application == "applied"
                        && terminal.is_none(),
                    "terminal absent, duplicated or not applied"
                );
                observation.validate(binding.execution_mode)?;
                terminal = Some(observation.clone());
            }
            ExecutionChannelPayload::Acknowledge { .. }
            | ExecutionChannelPayload::Stopped { .. } => {}
            _ => anyhow::bail!("ordinary channel retained unexpected payload"),
        }
    }
    ensure!(
        ready == 1 && release == 1,
        "ordinary readiness/release is not unique"
    );
    let terminal = terminal.context("authenticated target terminal")?;
    ensure!(
        terminal.reason == ExternalCommandTerminationReason::TargetExited
            && terminal.target_exit == ExternalTargetExit::Code(0),
        "target did not exit successfully"
    );
    for (bytes, commitment) in [(&stdout, &terminal.stdout), (&stderr, &terminal.stderr)] {
        ensure!(
            !commitment.truncated
                && commitment.bytes == bytes.len() as u64
                && commitment.sha256 == lillux::sha256_hex(bytes),
            "terminal output commitment mismatch"
        );
    }
    let interpreted =
        ryeos_engine::dispatch::interpret_external_terminal(&terminal, &stdout, &stderr);
    ensure!(
        &serde_json::to_value(interpreted.result)? == result,
        "public result differs from authenticated target output"
    );
    let intent: String = db.query_row("SELECT intent_json FROM external_execution_termination_intent WHERE placement_thread_id=?1", [id], |row| row.get(0))?;
    let observation: String = db.query_row("SELECT observation_json FROM external_execution_terminal_observation WHERE placement_thread_id=?1", [id], |row| row.get(0))?;
    let intent: Value = serde_json::from_str(&intent)?;
    let observation: Value = serde_json::from_str(&observation)?;
    ensure!(
        intent["schema"] == 1
            && observation["schema"] == 1
            && intent["termination_request_digest"]
                .as_str()
                .is_some_and(lillux::valid_hash),
        "invalid termination identity"
    );
    for (key, expected) in [
        ("binding_hash", reservation.binding_hash.as_str()),
        ("request_digest", reservation.request_digest.as_str()),
        ("occurrence_id", occurrence.occurrence_id.as_str()),
    ] {
        ensure!(
            intent[key] == expected && observation[key] == expected,
            "termination identity mismatch: {key}"
        );
    }
    ensure!(
        observation["terminal_state"] == "terminated"
            && observation["termination_request_digest"] == intent["termination_request_digest"]
            && observation["provider_observation_digest"]
                .as_str()
                .is_some_and(lillux::valid_hash),
        "independent occurrence death evidence missing"
    );
    // These are current retained-state assertions, not a claim that no local
    // process ever ran. No state owner, launch, or cleanup is created here.
    let clean: bool = db.query_row("SELECT pid IS NULL AND pgid IS NULL AND process_identity IS NULL AND workspace_id IS NULL AND workspace_view_identity IS NULL AND workspace_borrower_launch_owner IS NULL AND stop_intent IS NULL FROM thread_runtime WHERE thread_id=?1", [id], |row| row.get(0))?;
    ensure!(
        clean,
        "ordinary external root retains controller process/workspace/stop authority"
    );
    for table in ["dedicated_session", "external_execution_connector"] {
        let absent: bool = db.query_row(
            &format!("SELECT NOT EXISTS(SELECT 1 FROM {table} WHERE placement_thread_id=?1)"),
            [id],
            |row| row.get(0),
        )?;
        ensure!(absent, "ordinary root retains unexpected {table}");
    }
    let clean: bool = db.query_row("SELECT NOT EXISTS(SELECT 1 FROM thread_launch_claim WHERE thread_id=?1) AND NOT EXISTS(SELECT 1 FROM external_execution_revocation WHERE binding_digest=?2)", rusqlite::params![id,digest], |row| row.get(0))?;
    ensure!(clean, "successful root retains launch claim or revocation");
    let no_export: bool = db.query_row("SELECT completion_request_digest IS NULL AND export_snapshot_hash IS NULL AND export_output_capture_hash IS NULL AND export_evidence_hash IS NULL FROM external_execution_channel WHERE placement_thread_id=?1", [id], |row| row.get(0))?;
    ensure!(
        no_export,
        "ordinary readonly channel acquired export authority"
    );
    Ok(())
}
