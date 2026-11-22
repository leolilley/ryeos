//! Original-owner creation and one captured provider-adapter invocation.
//! No caller-supplied resolved body or public observation is admission here.
use super::*;
use ryeos_external_execution::guest_inputs::ExternalGuestInputExclusionWitness;
use ryeos_external_execution_contract::qualification_request_binding::{
    MAX_QUALIFICATION_REQUEST_BINDING_BYTES, QUALIFICATION_REQUEST_BINDING_FD_ENV,
    QualificationRequestBindingInput, QualificationRequestBindingOutput,
};
use ryeos_state::external_execution::qualification_controller_canary::QControllerCanaryBinding;
use ryeos_state::objects::canonical_value_digest;

/// Constructed only below from original file custody and the exact selected
/// captured adapter. Deliberately no Deserialize, Clone, or public fields.
pub(crate) struct PreparedQualificationControllerCanary {
    record: QualificationLocalStartRecord,
    binding: QControllerCanaryBinding,
}
impl PreparedQualificationControllerCanary {
    pub(crate) fn record(&self) -> &QualificationLocalStartRecord {
        &self.record
    }
    pub(crate) fn binding(&self) -> &QControllerCanaryBinding {
        &self.binding
    }
}

impl QualificationLocalOwner<'_> {
    /// Caller retains the complete original guest source witness and its same
    /// materialization leases, while this borrowed owner holds the occurrence
    /// gate. No callback lock is held over filesystem or subprocess work.
    pub fn prepare_controller_canary(
        &self,
        witness: &ExternalGuestInputExclusionWitness,
    ) -> Result<Option<QControllerCanaryBinding>> {
        let invocation = self
            .root
            .purpose()
            .consumer_invocation
            .as_ref()
            .context("Q canary lost sealed invocation")?;
        let request = self
            .root
            .purpose()
            .policy_source
            .policy
            .producer_scenarios
            .get(&invocation.scenario_id)
            .context("Q canary lost signed scenario")?
            .consumer_worker_request
            .as_ref()
            .context("Q canary lost signed Worker request")?;
        request.validate()?;
        if request.controller_canary.is_none() {
            return Ok(None);
        }
        let current = self.start_record()?;
        let custody = self.state.qualification_workspace_custody.borrow_exact(
            &QualificationWorkspaceCustodyCoordinate::from_start(&current)?,
        )?;
        let home = self.credential_home()?;
        let admission = self.root.authenticate_invocation(self.state)?;
        // This durable uncertainty precedes every canary filesystem effect.
        let record = self
            .state
            .state_store
            .reserve_qualification_controller_canary(self.state, &current, witness, admission)?;
        let intent = record
            .controller_canary_intent
            .as_ref()
            .context("Q canary intent absent")?;
        let (path, initial_file) =
            custody.create_controller_canary(&home, witness.retained_descriptors())?;
        ensure!(
            path == intent.controller_path,
            "Q canary original helper changed reserved path"
        );
        let allocation = self
            .state
            .state_store
            .external_allocation(self.placement)?
            .context("Q canary lost allocation")?;
        let retained = self
            .state
            .state_store
            .retained_external_binding(&allocation.reservation.binding_hash)?
            .context("Q canary lost placement binding")?;
        let contract = retained.backend_contract();
        let adapter = self
            .state
            .external_provider_configurations
            .qualify(&contract)?;
        adapter
            .declaration()
            .require_qualification_request_binding()?;
        ensure!(
            adapter.declaration().id == intent.provider_declaration_id,
            "Q canary changed selected adapter declaration"
        );
        let channel = self
            .state
            .state_store
            .external_execution_channel(self.placement)?;
        let remaining = channel
            .execution_deadline_ms
            .checked_sub(lillux::time::timestamp_millis())
            .filter(|remaining| *remaining > 0)
            .context("Q canary binding exceeded original channel deadline")?;
        let deadline = self
            .root
            .cap_execution_deadline(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_millis(u64::try_from(remaining)?),
            ));
        ensure!(
            !deadline.has_elapsed(),
            "Q canary binding original execution clock expired"
        );
        let input = QualificationRequestBindingInput {
            schema: 1,
            template: request.body.clone(),
            controller_canary_path: path.clone(),
        };
        let output = invoke_adapter(&adapter, &input, deadline)?;
        let binding = QControllerCanaryBinding {
            schema: 1,
            controller_path: path,
            initial_file,
            provider_declaration_id: intent.provider_declaration_id.clone(),
            configuration_adapter_artifact_hash: intent.configuration_adapter_artifact_hash.clone(),
            configuration_adapter_artifact_bytes: intent.configuration_adapter_artifact_bytes,
            binding_input_digest: input.digest()?,
            binding_output_digest: output.digest()?,
            template_request_digest: canonical_value_digest(&request.body)?,
            resolved_request_digest: canonical_value_digest(&output.resolved_body)?,
            resolved_body: output.resolved_body,
        };
        request.bound_controller_canary_body(Some(&binding))?;
        ensure!(
            !deadline.has_elapsed(),
            "Q canary reply exceeded original execution clock"
        );
        let prepared = PreparedQualificationControllerCanary { record, binding };
        let admission = self.root.authenticate_invocation(self.state)?;
        let retained = self
            .state
            .state_store
            .bind_qualification_controller_canary(self.state, &prepared, admission)?;
        Ok(retained.controller_canary_binding)
    }
}

fn invoke_adapter(
    adapter: &crate::external_placement::InstalledExternalProviderConfiguration,
    input: &QualificationRequestBindingInput,
    deadline: lillux::time::MonotonicDeadline,
) -> Result<QualificationRequestBindingOutput> {
    ensure!(
        !deadline.has_elapsed(),
        "Q binder has no original remaining execution time"
    );
    let request = lillux::sealed_memfd(c"ryeos-q-request-binding", &input.canonical_bytes()?)
        .map_err(anyhow::Error::msg)?;
    let mut process = lillux::SubprocessRequest {
        cmd: String::new(),
        argv0: Some("ryeos-provider-configuration-adapter".into()),
        args: vec!["bind-qualification-request".into()],
        cwd: Some("/".into()),
        envs: vec![(
            QUALIFICATION_REQUEST_BINDING_FD_ENV.into(),
            request
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?
                .to_string(),
        )],
        stdin_data: None,
        timeout: deadline.remaining().as_secs_f64(),
        limits: Some(lillux::SubprocessLimits {
            max_open_files: Some(64),
            max_stdout_bytes: Some(MAX_QUALIFICATION_REQUEST_BINDING_BYTES as u64),
            max_stderr_bytes: Some(4096),
            ..Default::default()
        }),
        inherited_fds: vec![request],
        inherited_fd_mappings: Vec::new(),
        supervised_status: None,
    };
    adapter
        .executable()
        .bind_as_subprocess_executable_at_source(&mut process)
        .map_err(anyhow::Error::msg)?;
    let running = lillux::spawn_until(process, deadline)
        .map_err(|_| anyhow::anyhow!("Q selected provider binder could not start"))?;
    // Read original bounded bytes, not SubprocessResult's lossy UTF-8 display.
    // Lillux retains the sole wait/deadline/reap owner while its existing raw
    // capture observer runs; no detached task or second process owner exists.
    let (result, bytes) = running.wait_with_stdout(|reader| -> Result<Vec<u8>> {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        reader
            .take((MAX_QUALIFICATION_REQUEST_BINDING_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_QUALIFICATION_REQUEST_BINDING_BYTES,
            "Q selected provider binder raw output exceeded wire bound"
        );
        Ok(bytes)
    });
    ensure!(
        result.success
            && !result.timed_out
            && result.output_limit_exceeded.is_none()
            && result.launcher_refusal.is_none()
            && !deadline.has_elapsed(),
        "Q selected provider binder failed, overflowed, or exceeded its original deadline"
    );
    let bytes = bytes
        .map_err(|_| anyhow::anyhow!("Q selected provider binder raw output capture failed"))?;
    let output: QualificationRequestBindingOutput =
        ryeos_external_execution_contract::from_json_slice_strict(
            &bytes,
            MAX_QUALIFICATION_REQUEST_BINDING_BYTES,
        )?;
    output.validate_for(input)?;
    ensure!(
        output.canonical_bytes()? == bytes,
        "Q provider binder output is not exact canonical wire"
    );
    Ok(output)
}
