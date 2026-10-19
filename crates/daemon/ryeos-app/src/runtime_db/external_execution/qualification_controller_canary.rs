//! Original local-owner canary stages. Public JSON is retained testimony only;
//! this module cannot open a pathname or reconstruct original file custody.
use super::qualification_local_start::{
    QualificationLocalStartPhase, QualificationLocalStartRecord, now_after, read_local_start,
    require_current_start, require_live_credential_custody, require_local_processes_settled,
    transition,
};
use super::*;
use ryeos_state::external_execution::qualification_controller_canary::{
    QControllerCanaryBinding, QControllerCanaryContract, QControllerCanaryEvidence,
    validate_controller_path,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationControllerCanaryIntent {
    pub schema: u32,
    pub contract: QControllerCanaryContract,
    pub controller_path: String,
    pub program_digest: String,
    pub provider_declaration_id: String,
    pub configuration_adapter_artifact_hash: String,
    pub configuration_adapter_artifact_bytes: u64,
    pub guest_input_identity: String,
    pub template_request_digest: String,
    pub reserved_at_ms: i64,
}

pub(super) fn optional_json<T: Serialize>(value: Option<&T>) -> Result<Option<String>> {
    value
        .map(|value| lillux::canonical_json(&serde_json::to_value(value)?).map_err(Into::into))
        .transpose()
}

pub(super) fn validate_canary_shape(record: &QualificationLocalStartRecord) -> Result<()> {
    let Some(intent) = &record.controller_canary_intent else {
        ensure!(
            record.controller_canary_binding.is_none()
                && record.controller_canary_terminal.is_none(),
            "Q canary observation has no original creation intent"
        );
        return Ok(());
    };
    ensure!(
        intent.schema == 1
            && intent.reserved_at_ms >= record.created_at_ms
            && intent.reserved_at_ms <= record.updated_at_ms,
        "Q canary intent is not current or ordered"
    );
    validate_controller_path(&intent.controller_path)?;
    ensure!(
        !intent.provider_declaration_id.is_empty()
            && intent.provider_declaration_id.len() <= 128
            && intent
                .provider_declaration_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            && intent.configuration_adapter_artifact_bytes > 0,
        "Q canary intent has an invalid selected adapter identity"
    );
    ensure!(
        std::path::Path::new(&intent.controller_path)
            == std::path::Path::new(&record.state_root)
                .join("controller-canary")
                .join("canary"),
        "Q canary differs from the original state's fixed child"
    );
    for value in [
        &intent.program_digest,
        &intent.guest_input_identity,
        &intent.template_request_digest,
        &intent.configuration_adapter_artifact_hash,
    ] {
        validate_sha256("Q canary intent", value)?;
    }
    if let Some(binding) = &record.controller_canary_binding {
        binding.validate()?;
        ensure!(
            binding.controller_path == intent.controller_path
                && binding.template_request_digest == intent.template_request_digest
                && binding.provider_declaration_id == intent.provider_declaration_id
                && binding.configuration_adapter_artifact_hash
                    == intent.configuration_adapter_artifact_hash
                && binding.configuration_adapter_artifact_bytes
                    == intent.configuration_adapter_artifact_bytes,
            "Q canary binding changed its creation intent"
        );
    }
    if let Some(terminal) = &record.controller_canary_terminal {
        terminal.validate()?;
        ensure!(
            record.controller_canary_binding.as_ref() == Some(&terminal.binding)
                && record.worker_incarnation_digest.is_some(),
            "Q canary terminal observation changed its bound process"
        );
    }
    ensure!(
        record.worker_incarnation_digest.is_none() || record.controller_canary_binding.is_some(),
        "Q canary process has no prepared original binding"
    );
    Ok(())
}

pub(super) fn require_retained_canary(
    conn: &Connection,
    record: &QualificationLocalStartRecord,
) -> Result<()> {
    validate_canary_shape(record)?;
    let columns: (Option<String>, Option<String>, Option<String>) = conn.query_row(
        "SELECT controller_canary_intent_json,controller_canary_binding_json,controller_canary_terminal_json
         FROM external_qualification_local_start WHERE placement_thread_id=?1",
        [&record.placement_thread_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    ensure!(
        columns
            == (
                optional_json(record.controller_canary_intent.as_ref())?,
                optional_json(record.controller_canary_binding.as_ref())?,
                optional_json(record.controller_canary_terminal.as_ref())?
            ),
        "Q canary columns contradict its canonical local record"
    );
    let Some(intent) = &record.controller_canary_intent else {
        return Ok(());
    };
    let allocation =
        read(conn, &record.placement_thread_id)?.context("Q canary lost its allocation")?;
    let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
    else {
        bail!("Q canary has another allocation owner");
    };
    ensure!(
        intent.program_digest == owner.program.digest()?,
        "Q canary changed its original program"
    );
    let retained = read_retained_binding(conn, &allocation.reservation.binding_hash)?
        .context("Q canary lost retained binding")?;
    let contract = retained.backend_contract();
    let selected = contract.workload.session_artifacts()?;
    ensure!(
        intent.provider_declaration_id == selected.provider_declaration_id
            && intent.configuration_adapter_artifact_hash
                == selected.configuration_adapter_artifact_hash
            && intent.configuration_adapter_artifact_bytes
                == selected.configuration_adapter_artifact_bytes,
        "Q canary changed its retained selected adapter"
    );
    let activation = read_canonical_evidence::<ExternalSupervisorActivationIntent>(
        conn,
        "external_execution_supervisor_activation_intent",
        "intent_json",
        &record.placement_thread_id,
    )?
    .context("Q canary lost original guest input activation")?;
    activation.validate(
        &allocation.reservation,
        allocation
            .occurrence
            .as_ref()
            .context("Q canary lost original guest occurrence")?,
    )?;
    ensure!(
        activation.guest_input_identity == intent.guest_input_identity,
        "Q canary changed its original guest input exclusion witness"
    );
    if let Some(binding) = &record.controller_canary_binding {
        let command: Option<String> = conn.query_row(
            "SELECT request_digest FROM external_qualification_worker_command WHERE placement_thread_id=?1",
            [&record.placement_thread_id],|row|row.get(0)).optional()?;
        ensure!(
            command
                .as_ref()
                .is_none_or(|digest| digest == &binding.resolved_request_digest),
            "Q canary command changed its resolved request"
        );
    }
    if record.controller_canary_terminal.is_some() {
        require_local_processes_settled(conn, record)?;
    }
    Ok(())
}

pub(super) fn require_bound_canary(
    record: &QualificationLocalStartRecord,
    proof: &crate::state_store::VerifiedExternalQualificationOwner,
    actual_request_digest: Option<&str>,
) -> Result<()> {
    match proof.canary_request() {
        None => ensure!(
            record.controller_canary_intent.is_none(),
            "Q canary differs from absent signed selection"
        ),
        Some(request) => {
            let intent = record
                .controller_canary_intent
                .as_ref()
                .context("Q signed canary has no original intent")?;
            let binding = record
                .controller_canary_binding
                .as_ref()
                .context("Q signed canary has no original adapter binding")?;
            ensure!(
                Some(intent.contract) == request.controller_canary,
                "Q canary changed signed selection"
            );
            request.bound_controller_canary_body(Some(binding))?;
            ensure!(
                actual_request_digest
                    .is_none_or(|digest| digest == binding.resolved_request_digest),
                "Q contact changed original bound request"
            );
        }
    }
    Ok(())
}

impl RuntimeDb {
    pub(crate) fn reserve_qualification_controller_canary(
        &self,
        expected: &QualificationLocalStartRecord,
        guest_input_identity: &str,
        proof: &crate::state_store::VerifiedExternalQualificationOwner,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let current = read_local_start(&tx, &expected.placement_thread_id)?
                .context("Q canary lost original start")?;
            ensure!(
                &current == expected
                    && current.controller_canary_intent.is_none()
                    && current.directory_borrow_intent.is_none()
                    && current.worker_incarnation_digest.is_none()
                    && matches!(
                        current.phase,
                        QualificationLocalStartPhase::Reserved
                            | QualificationLocalStartPhase::WorkspaceReady
                    ),
                "Q canary creation was already reserved or directory borrowed"
            );
            require_current_start(&tx, &current)?;
            require_live_credential_custody(&tx, &current)?;
            let allocation =
                read(&tx, &current.placement_thread_id)?.context("Q canary lost allocation")?;
            require_new_owner(&tx, &allocation.reservation, None, Some(proof))?;
            let request = proof
                .canary_request()
                .context("Q canary has no exact signed selection")?;
            let ExternalAllocationOwner::QualificationCandidate(owner) = &allocation.reservation.owner
            else {
                bail!("Q canary has wrong owner")
            };
            let retained = read_retained_binding(&tx, &allocation.reservation.binding_hash)?
                .context("Q canary lost binding")?;
            let contract = retained.backend_contract();
            let selected = contract.workload.session_artifacts()?;
            let activation = read_canonical_evidence::<ExternalSupervisorActivationIntent>(
                &tx,
                "external_execution_supervisor_activation_intent",
                "intent_json",
                &current.placement_thread_id,
            )?
            .context("Q canary lost original activation")?;
            ensure!(
                activation.guest_input_identity == guest_input_identity,
                "Q canary exclusion witness changed original activation"
            );
            let now = now_after(&current)?;
            let mut next = current.clone();
            next.updated_at_ms = now;
            next.controller_canary_intent = Some(QualificationControllerCanaryIntent {
                schema: 1,
                contract: request
                    .controller_canary
                    .context("Q signed canary is absent")?,
                controller_path: std::path::Path::new(&current.state_root)
                    .join("controller-canary")
                    .join("canary")
                    .to_str()
                    .context("Q canary path not UTF-8")?
                    .into(),
                program_digest: owner.program.digest()?,
                guest_input_identity: guest_input_identity.into(),
                template_request_digest: ryeos_state::objects::canonical_value_digest(&request.body)?,
                reserved_at_ms: now,
                provider_declaration_id: selected.provider_declaration_id.into(),
                configuration_adapter_artifact_hash: selected
                    .configuration_adapter_artifact_hash
                    .into(),
                configuration_adapter_artifact_bytes: selected.configuration_adapter_artifact_bytes,
            });
            transition(&tx, &current, &next)?;
            tx.commit()?;
            Ok(next)
        })
    }

    pub(crate) fn bind_qualification_controller_canary(
        &self,
        expected: &QualificationLocalStartRecord,
        binding: &QControllerCanaryBinding,
        proof: &crate::state_store::VerifiedExternalQualificationOwner,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let current = read_local_start(&tx, &expected.placement_thread_id)?
                .context("Q canary bind lost start")?;
            ensure!(
                &current == expected
                    && current.controller_canary_intent.is_some()
                    && current.controller_canary_binding.is_none()
                    && current.directory_borrow_intent.is_none()
                    && current.worker_incarnation_digest.is_none()
                    && matches!(
                        current.phase,
                        QualificationLocalStartPhase::Reserved
                            | QualificationLocalStartPhase::WorkspaceReady
                    ),
                "Q canary cannot replace or adopt its original adapter binding"
            );
            require_current_start(&tx, &current)?;
            require_live_credential_custody(&tx, &current)?;
            let allocation =
                read(&tx, &current.placement_thread_id)?.context("Q canary bind lost allocation")?;
            require_new_owner(&tx, &allocation.reservation, None, Some(proof))?;
            proof
                .canary_request()
                .context("Q canary lost signed selection")?
                .bound_controller_canary_body(Some(binding))?;
            let mut next = current.clone();
            next.controller_canary_binding = Some(binding.clone());
            next.updated_at_ms = now_after(&current)?;
            validate_canary_shape(&next)?;
            require_bound_canary(&next, proof, None)?;
            transition(&tx, &current, &next)?;
            tx.commit()?;
            Ok(next)
        })
    }
}

/// Terminal data must already have been read from the original live custody by
/// the StateStore wrapper while the original occurrence cleanup gate is held.
impl RuntimeDb {
    pub(crate) fn record_qualification_controller_canary_terminal(
        &self,
        expected: &QualificationLocalStartRecord,
        evidence: &QControllerCanaryEvidence,
    ) -> Result<QualificationLocalStartRecord> {
        self.conn.with_sql(|conn| {
            evidence.validate()?;
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let current = read_local_start(&tx, &expected.placement_thread_id)?
                .context("Q terminal canary lost local start")?;
            ensure!(
                &current == expected
                    && current.daemon_generation_id == daemon_generation_id()
                    && current.controller_canary_terminal.is_none()
                    && current.controller_canary_binding.as_ref() == Some(&evidence.binding)
                    && matches!(
                        current.phase,
                        QualificationLocalStartPhase::WorkerAttached
                            | QualificationLocalStartPhase::CleanupUnproved
                    )
                    && current
                        .credential_custody
                        .as_ref()
                        .is_some_and(|custody| custody.released_at_ms.is_none()),
                "Q terminal canary changed original custody before credential release"
            );
            require_local_processes_settled(&tx, &current)?;
            let allocation = read(&tx, &current.placement_thread_id)?
                .context("Q terminal canary lost allocation")?;
            let process = qualification_process::require_terminal_qualification_worker_process(
                &tx,
                &allocation.reservation,
            )?;
            ensure!(
                current.worker_incarnation_digest.as_deref()
                    == Some(
                        process
                            .process_identity
                            .owner_incarnation_digest()?
                            .as_str()
                    ),
                "Q terminal canary changed original dead process"
            );
            let mut next = current.clone();
            next.controller_canary_terminal = Some(evidence.clone());
            next.updated_at_ms = now_after(&current)?;
            transition(&tx, &current, &next)?;
            tx.commit()?;
            Ok(next)
        })
    }
}
