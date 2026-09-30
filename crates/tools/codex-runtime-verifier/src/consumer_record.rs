//! Canonical accepted input record inside a consumer-verifier archive.
//!
//! This is a product-owned input format, not a new Worker protocol. Parsing
//! checks data agreement; the controller must authenticate the accepted root,
//! retained signed source and archive before guest delivery.

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::{
    ConsumerRuntimeVerificationCoordinate, ConsumerRuntimeVerifierSelection,
    RemoteVerificationPurpose, RestoredVerifierAttemptIntent,
};
use ryeos_external_execution_contract::staging_package::GuestStagingEntry;
use ryeos_state::external_content::qualification_purpose::QualificationLaunchPurpose;
use ryeos_state::external_execution::admission::ExternalCandidateRequirement;
use serde::{Deserialize, Serialize};

use crate::QualificationExecutionEnvironment;

pub const CONSUMER_INPUT_RECORD_NAME: &str = "consumer-input.json";
pub const MAX_CONSUMER_INPUT_RECORD_BYTES: usize = 96 * 1024;
/// Dedicated command-environment startup input, never an admission credential.
pub const CONSUMER_NATIVE_CHALLENGE_ENV: &str = "RYEOS_CONSUMER_NATIVE_CHALLENGE_B64";

/// Codex-specific finite scenario parameters, not another runtime recipe.
/// Executable, guest recipe and input identities come from the accepted record.
/// Scripted-provider success does not qualify arbitrary live model settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerVerifierConfiguration {
    pub schema: String,
    pub expected_command_output: String,
    pub responses_origin: String,
    pub external_candidate_qualification_context:
        ryeos_state::external_execution::admission::ExternalCandidateQualificationUse,
}

impl ConsumerVerifierConfiguration {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == "ryeos.codex.consumer_verifier.v1",
            "unsupported consumer verifier configuration"
        );
        self.external_candidate_qualification_context.validate()?;
        crate::scripted_provider::validate_configuration(
            &self.expected_command_output,
            &self.responses_origin,
        )
    }
}

/// Product-edge canonical record plus exact foreign-target verifier payload.
/// This is byte custody and semantic agreement, not root admission, provider
/// permission or a controller-local executable authorization.
pub struct ConsumerDeliveryRecords {
    pub entries: Vec<GuestStagingEntry>,
    pub descriptors: std::collections::BTreeMap<String, lillux::InheritedDescriptorAuthority>,
}

/// Exact opened production inputs after private archive extraction. The outer
/// guest owner must retain the private generation, exclude writers and settle
/// native execution. This is descriptor custody, not a qualification result.
pub struct ImportedConsumerInputs {
    pub inputs: Vec<ryeos_external_execution_contract::GuestMountInput>,
    pub descriptors: std::collections::BTreeMap<u32, lillux::InheritedDescriptorAuthority>,
    _record: lillux::InheritedDescriptorAuthority,
    _verifier: lillux::InheritedDescriptorAuthority,
    root: lillux::PinnedDirectory,
    record_sha256: String,
}

impl ImportedConsumerInputs {
    /// Deliver the one-way finish signal after the scripted turn is complete,
    /// while Codex's command transport is still alive. This requests settlement;
    /// it is not an acknowledgment that writers are excluded or output is frozen.
    pub fn request_native_capture(
        &self,
        record: &ConsumerInputRecord,
        challenge: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeChallenge,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        self.require_record_challenge(record, challenge)?;
        ensure!(!deadline.has_elapsed(), "consumer finish deadline expired");
        let started = self
            .root
            .open_pinned_regular(std::ffi::OsStr::new("native-started"), false)?
            .context("consumer native execution has not started")?;
        ensure!(
            started.permission_mode()? == 0o400
                && started.read_stable_bounded(&started.observation()?, 64)?
                    == self.record_sha256.as_bytes(),
            "consumer native start marker differs from accepted input"
        );
        ensure!(
            !deadline.has_elapsed(),
            "consumer finish delivery deadline expired"
        );
        if self
            .root
            .atomic_create_pinned_regular(std::ffi::OsStr::new("finish"), b"capture", 0o400)?
            .is_none()
        {
            let existing = self
                .root
                .open_pinned_regular(std::ffi::OsStr::new("finish"), false)?
                .context("consumer finish marker disappeared")?;
            ensure!(
                existing.permission_mode()? == 0o400
                    && existing.read_stable_bounded(&existing.observation()?, 7)? == b"capture",
                "consumer finish request was changed"
            );
        }
        self.root.ensure_path_binding()?;
        ensure!(
            !deadline.has_elapsed(),
            "consumer finish acknowledgment exceeded deadline"
        );
        Ok(())
    }

    /// Point-read an exact native observation; None means no observation yet,
    /// not terminal failure. Caller uses its existing finite observation loop.
    /// Coordinate agreement is not semantic qualification or whole guest death.
    pub fn read_native_observation(
        &self,
        record: &ConsumerInputRecord,
        challenge: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeChallenge,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Option<Vec<u8>>> {
        self.require_record_challenge(record, challenge)?;
        ensure!(
            !deadline.has_elapsed(),
            "consumer observation deadline expired"
        );
        let Some(file) = self
            .root
            .open_pinned_regular(std::ffi::OsStr::new("guest-observation.json"), false)?
        else {
            self.root.ensure_path_binding()?;
            ensure!(
                !deadline.has_elapsed(),
                "consumer observation lookup exceeded deadline"
            );
            return Ok(None);
        };
        ensure!(
            file.permission_mode()? == 0o400,
            "consumer native observation mode changed"
        );
        let bytes = file.read_stable_bounded(&file.observation()?,
            ryeos_external_execution_contract::restored_runtime_measurement::MAX_CONSUMER_VERIFIER_EVIDENCE_BYTES)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        ensure!(
            lillux::canonical_json(&value)?.as_bytes() == bytes
                && value["schema"] == "ryeos.consumer-native-observation.v1"
                && value["operation_id"] == challenge.intent.operation_id
                && value["challenge_digest"] == challenge.intent.consumer_challenge_digest()?
                && value["input_record_sha256"] == self.record_sha256,
            "consumer native observation differs from exact attempt"
        );
        self.root.ensure_path_binding()?;
        ensure!(
            !deadline.has_elapsed(),
            "consumer observation read exceeded deadline"
        );
        Ok(Some(bytes))
    }

    pub(crate) fn require_record_challenge(
        &self,
        record: &ConsumerInputRecord,
        challenge: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeChallenge,
    ) -> Result<()> {
        self.root.ensure_path_binding()?;
        challenge.validate()?;
        record.validate_attempt(&challenge.intent)?;
        ensure!(
            record.selection == challenge.selection
                && lillux::sha256_hex(&record.canonical_bytes()?) == self.record_sha256,
            "consumer control request substituted imported authority"
        );
        Ok(())
    }

    /// Prepare the existing pinned app-server launch without spawning it.
    /// The enclosing verifier owns this fresh home, scripted peer, protocol,
    /// descendant settlement and output acceptance. Native guest execution
    /// remains the command environment, not a second Worker/session workflow.
    pub fn prepare_codex_app_server(
        &self,
        record: &ConsumerInputRecord,
        challenge: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeChallenge,
        fresh_home: &lillux::PinnedDirectory,
    ) -> Result<lillux::PinnedSubordinateProcessRequest> {
        challenge.validate()?;
        record.validate_attempt(&challenge.intent)?;
        ensure!(
            record.selection == challenge.selection,
            "app-server challenge substituted protected selection"
        );
        let configuration = record.scripted_configuration()?;
        fresh_home.require_owner_private_directory()?;
        fresh_home.ensure_path_binding()?;
        ensure!(
            fresh_home.entries_no_follow_bounded(1)?.is_empty(),
            "consumer Codex home is not fresh"
        );
        let home_identity = fresh_home.inherited_descriptor_authority()?;
        ensure!(
            !home_identity.same_file_identity(&self.root.inherited_descriptor_authority()?)?,
            "consumer Codex home aliases the imported root"
        );
        for descriptor in self.descriptors.values() {
            ensure!(
                !home_identity.same_file_identity(descriptor)?,
                "consumer Codex home aliases a product input"
            );
        }
        // Selection before writes prevents invalid delivery from causing home
        // mutation; selection afterward rejects contamination beneath products.
        let _ = self.controller_codex_executable(record)?;
        let root = self.root.try_clone()?.into_inherited_descriptor_path()?;
        let verifier = self._verifier.clone();
        let commands = crate::staging::render_consumer_command_environment(
            verifier.path(),
            root.path(),
            challenge,
        )?;
        let baseline = crate::expected_scripted_baseline(&configuration.responses_origin);
        for (name, bytes) in [
            ("config.toml", baseline.as_bytes()),
            ("environments.toml", commands.as_slice()),
        ] {
            ensure!(
                bytes.len() <= 64 * 1024,
                "consumer Codex configuration exceeds bound"
            );
            fresh_home
                .atomic_create_pinned_regular(std::ffi::OsStr::new(name), bytes, 0o400)?
                .context("consumer Codex configuration already exists")?;
        }
        let executable = self.controller_codex_executable(record)?;
        fresh_home.ensure_path_binding()?;
        let home = fresh_home.try_clone()?.into_inherited_descriptor_path()?;
        let home_path = home
            .path()
            .to_str()
            .context("consumer Codex home is not UTF-8")?
            .to_owned();
        Ok(lillux::PinnedSubordinateProcessRequest {
            executable,
            cwd: self.root.try_clone()?,
            argv0: Some("codex".into()),
            args: vec![
                "--strict-config".into(),
                "-c".into(),
                "check_for_update_on_startup=false".into(),
                "app-server".into(),
            ],
            envs: vec![
                ("CODEX_HOME".into(), home_path.clone()),
                ("HOME".into(), home_path),
                ("PATH".into(), String::new()),
                ("LANG".into(), "C".into()),
                ("LC_ALL".into(), "C".into()),
            ],
            limits: None,
            inherited_fds: vec![home, verifier, root],
        })
    }

    /// Select the controller app-server executable from the imported worker
    /// literal, not the guest runtime member. Reopening is descriptor-relative
    /// and must select the same inode as the retained, verified input FD.
    /// The enclosing private-generation owner still excludes writers.
    pub fn controller_codex_executable(
        &self,
        record: &ConsumerInputRecord,
    ) -> Result<lillux::PinnedRegularFile> {
        self.root.ensure_path_binding()?;
        ensure!(
            lillux::sha256_hex(&record.canonical_bytes()?) == self.record_sha256,
            "controller Codex selection substituted the imported record"
        );
        let content = record
            .purpose
            .consumer_content
            .as_ref()
            .context("consumer content is missing")?;
        let consumer = record
            .purpose
            .policy_source
            .policy
            .consumer_execution_context
            .as_ref()
            .context("consumer context is missing")?;
        crate::production_inputs::verify_production_descriptor_inputs(
            content,
            consumer,
            &record.requirement,
            &self.inputs,
            &self.descriptors,
        )?;
        let index = record
            .production_realizations()?
            .iter()
            .position(|entry| entry.id == "codex")
            .context("production consumer has no controller Codex literal")?;
        let input = self
            .inputs
            .get(index)
            .context("controller Codex input is missing")?;
        ensure!(
            input.authority_id == "codex"
                && input.kind == ryeos_external_execution_contract::GuestMountKind::RegularFile
                && input.access == ryeos_external_execution_contract::GuestMountAccess::ReadOnly,
            "controller Codex input differs from admitted worker literal"
        );
        let executable = self
            .root
            .open_pinned_regular(std::ffi::OsStr::new(&format!("product-{index:02}")), false)?
            .context("imported controller Codex executable is missing")?;
        let retained = self
            .descriptors
            .get(&input.descriptor)
            .context("controller Codex descriptor is missing")?;
        ensure!(
            executable
                .inherited_descriptor_authority()?
                .same_file_identity(retained)?,
            "controller Codex path no longer selects its retained input descriptor"
        );
        let observation = executable.observation()?;
        ensure!(
            observation.portable_mode()? == 0o755
                && observation.size() == input.bytes
                && (1..=268_435_456).contains(&observation.size()),
            "controller Codex executable mode or size changed"
        );
        Ok(executable)
    }

    /// One subordinate native execution in a dedicated single-threaded guest
    /// verifier process. The enclosing owner controls pipes and finish delivery.
    /// A create-only private marker refuses repeated execution/reconnection;
    /// it is not a replacement for the controller's retained contact journal.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_native_protocol(
        &self,
        record: &ConsumerInputRecord,
        private: &crate::production_inputs::ProductionPrivateStaging,
        input: lillux::inherited_pipes::InheritedPipeInput,
        output: &mut lillux::inherited_pipes::InheritedPipeOutput,
        interrupt: lillux::inherited_pipes::PipeInterrupt,
        active: lillux::time::MonotonicDeadline,
        cleanup: lillux::time::MonotonicDeadline,
    ) -> Result<crate::native_guest::NativeProtocolObservation> {
        let prepared = self.prepare_native(record, private)?;
        self.root
            .atomic_create_pinned_regular(
                std::ffi::OsStr::new("native-started"),
                self.record_sha256.as_bytes(),
                0o400,
            )?
            .context(
                "consumer native execution already attempted; live pipes cannot be recreated",
            )?;
        // Keep prepared (including its private descriptor custody) alive until
        // the shared loop has settled the whole namespace and drained output.
        crate::native_guest::run_prepared_protocol(
            &self.root,
            prepared.request().clone(),
            input,
            output,
            interrupt,
            active,
            cleanup,
        )
    }

    /// Join imported bytes to the existing production native-request builder.
    /// Descriptor custody stays borrowed for the prepared request's lifetime.
    /// This does not launch, freeze output, or establish whole-scope settlement.
    pub fn prepare_native<'a>(
        &'a self,
        record: &ConsumerInputRecord,
        private: &crate::production_inputs::ProductionPrivateStaging,
    ) -> Result<crate::production_inputs::PreparedProductionNativeRequest<'a>> {
        self.root.ensure_path_binding()?;
        ensure!(
            lillux::sha256_hex(&record.canonical_bytes()?) == self.record_sha256,
            "native consumer request substituted the imported accepted record"
        );
        let content = record
            .purpose
            .consumer_content
            .as_ref()
            .context("consumer content is missing")?;
        let consumer = record
            .purpose
            .policy_source
            .policy
            .consumer_execution_context
            .as_ref()
            .context("consumer context is missing")?;
        crate::production_inputs::prepare_production_native_request(
            content,
            consumer,
            &record.requirement,
            &self.inputs,
            &self.descriptors,
            private,
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerInputRecord {
    pub schema: u32,
    pub purpose: QualificationLaunchPurpose,
    pub coordinate: ConsumerRuntimeVerificationCoordinate,
    pub selection: ConsumerRuntimeVerifierSelection,
    pub requirement: ExternalCandidateRequirement,
}

impl ConsumerInputRecord {
    /// Decode only the retained signed policy's finite protocol parameters.
    /// Mandatory use context is compared with admission-derived retained use;
    /// no fixture paths, caller hashes or ambient configuration are consulted.
    pub fn scripted_configuration(&self) -> Result<ConsumerVerifierConfiguration> {
        self.validate()?;
        let parameters = &self.purpose.policy_source.policy.verifier_parameters;
        ensure!(
            serde_json::to_vec(parameters)?.len() <= crate::INPUT_LIMIT,
            "consumer verifier parameters exceed bound"
        );
        self.purpose
            .consumer_content
            .as_ref()
            .context("consumer content is missing")?
            .qualification_use
            .as_ref()
            .context("consumer admitted use is missing")?
            .require_qualified_use(parameters)?;
        let configuration: ConsumerVerifierConfiguration =
            serde_json::from_value(parameters.clone())?;
        configuration.validate()?;
        Ok(configuration)
    }

    /// Open a dedicated privately extracted consumer archive. The caller must
    /// authenticate the run channel and bind the archive hash before extraction;
    /// no tar entry may be followed during extraction. Ordinary upload success
    /// supplies none of those facts. No Worker base/candidate coordinates exist.
    pub fn open_imported_products(
        &self,
        root: &lillux::PinnedDirectory,
        challenge: &ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeChallenge,
    ) -> Result<ImportedConsumerInputs> {
        use ryeos_external_execution_contract::restored_runtime_measurement::CONSUMER_VERIFIER_REMOTE_NAME;
        use ryeos_external_execution_contract::{
            GuestMountAccess, GuestMountContentAuthority, GuestMountInput, GuestMountKind,
            GuestMountRole, GuestProductManifestKind,
        };
        use ryeos_state::objects::ExternalContentKind;
        challenge.validate()?;
        self.validate_attempt(&challenge.intent)?;
        let protected_selection = &challenge.selection;
        ensure!(
            &self.selection == protected_selection,
            "consumer record differs from authenticated protected selection"
        );
        root.require_owner_private_directory()?;
        root.ensure_path_binding()?;
        let realizations = self.production_realizations()?;
        let expected = self.canonical_bytes()?;
        let budget = &protected_selection.archive_budget;
        let selected = self
            .purpose
            .remote_verifier_sources
            .get(&self.coordinate.scenario_id)
            .context("consumer verifier source is missing")?;
        let mut regular_bytes = (expected.len() as u64)
            .checked_add(selected.payload_bytes)
            .context("consumer import byte total overflow")?;
        let mut entry_count = 2usize;
        ensure!(
            regular_bytes <= budget.maximum_regular_bytes && entry_count <= budget.maximum_entries,
            "consumer import records exceed protected archive budget"
        );
        let record = root
            .open_inherited_regular(std::ffi::OsStr::new(CONSUMER_INPUT_RECORD_NAME), false)?
            .context("imported consumer record is missing")?;
        ensure!(
            record
                .read_regular_file_stable_bounded(MAX_CONSUMER_INPUT_RECORD_BYTES as u64)?
                .0
                == expected,
            "imported consumer record differs from accepted input"
        );
        let verifier = root
            .open_inherited_regular(std::ffi::OsStr::new(CONSUMER_VERIFIER_REMOTE_NAME), false)?
            .context("imported consumer verifier is missing")?;
        self.verify_verifier_payload(&verifier)?;
        let destinations = ryeos_state::external_execution::admission::ExternalCandidateGuestEnvironment::expected_destinations(
            &self.requirement, &realizations,
        )?;
        let mut allowed = std::collections::BTreeSet::from([
            CONSUMER_INPUT_RECORD_NAME.to_owned(),
            CONSUMER_VERIFIER_REMOTE_NAME.to_owned(),
        ]);
        let mut inputs = Vec::new();
        let mut descriptors = std::collections::BTreeMap::new();
        for (index, realized) in realizations.iter().enumerate() {
            entry_count = entry_count
                .checked_add(realized.entry_count)
                .and_then(|count| {
                    count.checked_add(if realized.kind == ExternalContentKind::Tree {
                        2
                    } else {
                        1
                    })
                })
                .context("consumer import entry total overflow")?;
            regular_bytes = regular_bytes
                .checked_add(realized.total_bytes)
                .context("consumer import byte total overflow")?;
            ensure!(
                entry_count <= budget.maximum_entries
                    && regular_bytes <= budget.maximum_regular_bytes,
                "consumer import products exceed protected archive budget"
            );
            let name = format!("product-{index:02}");
            let record_name = format!("{name}-manifest.json");
            allowed.insert(name.clone());
            allowed.insert(record_name.clone());
            let source = match realized.kind {
                ExternalContentKind::Tree => {
                    root.open_inherited_mount_entry(std::ffi::OsStr::new(&name))?
                }
                ExternalContentKind::File => {
                    root.open_inherited_regular(std::ffi::OsStr::new(&name), false)?
                }
            }
            .context("imported consumer product is missing")?;
            let manifest = root
                .open_inherited_regular(std::ffi::OsStr::new(&record_name), false)?
                .context("imported consumer product manifest is missing")?;
            let remaining = budget.maximum_regular_bytes - regular_bytes;
            let (bytes, _) =
                manifest.read_regular_file_stable_bounded(remaining.min(8 * 1024 * 1024))?;
            regular_bytes = regular_bytes
                .checked_add(bytes.len() as u64)
                .context("consumer import manifest byte total overflow")?;
            ensure!(
                lillux::sha256_hex(&bytes) == realized.manifest_hash,
                "imported consumer manifest differs from accepted realization"
            );
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            let manifest_kind = match value.get("kind").and_then(serde_json::Value::as_str) {
                Some(ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND) => {
                    GuestProductManifestKind::Content
                }
                Some(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND) => {
                    GuestProductManifestKind::LargeContent
                }
                _ => anyhow::bail!("imported consumer manifest has unsupported tier"),
            };
            let source_fd = source.inherited_descriptor().map_err(anyhow::Error::msg)?;
            let manifest_fd = manifest
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?;
            inputs.push(GuestMountInput {
                role: GuestMountRole::Product,
                authority_id: realized.id.clone(),
                descriptor: source_fd,
                destination: destinations
                    .get(&realized.id)
                    .context("imported consumer has no admitted destination")?
                    .clone(),
                kind: match realized.kind {
                    ExternalContentKind::Tree => GuestMountKind::Directory,
                    ExternalContentKind::File => GuestMountKind::RegularFile,
                },
                access: GuestMountAccess::ReadOnly,
                normalized_mode: match realized.kind {
                    ExternalContentKind::Tree => None,
                    ExternalContentKind::File => {
                        Some(source.regular_file_observation()?.portable_mode()?)
                    }
                },
                content_authority: GuestMountContentAuthority::ProductManifest {
                    manifest_kind,
                    manifest_hash: realized.manifest_hash.clone(),
                    manifest_descriptor: manifest_fd,
                    manifest_bytes: bytes.len() as u64,
                },
                bytes: realized.total_bytes,
            });
            ensure!(
                descriptors.insert(source_fd, source).is_none()
                    && descriptors.insert(manifest_fd, manifest).is_none(),
                "imported consumer descriptors alias"
            );
        }
        let observed = root.entries_no_follow_bounded(allowed.len())?;
        ensure!(
            observed.len() == allowed.len()
                && observed.iter().all(|entry| entry
                    .name
                    .to_str()
                    .is_some_and(|name| allowed.contains(name))),
            "imported consumer archive has missing or ambient entries"
        );
        let content = self
            .purpose
            .consumer_content
            .as_ref()
            .context("consumer content is missing")?;
        let context = self
            .purpose
            .policy_source
            .policy
            .consumer_execution_context
            .as_ref()
            .context("consumer context is missing")?;
        crate::production_inputs::verify_production_descriptor_inputs(
            content,
            context,
            &self.requirement,
            &inputs,
            &descriptors,
        )?;
        Ok(ImportedConsumerInputs {
            inputs,
            descriptors,
            _record: record,
            _verifier: verifier,
            root: root.try_clone()?,
            record_sha256: lillux::sha256_hex(&expected),
        })
    }

    /// Create small immutable delivery records before any attempt reservation
    /// or provider contact. Full runtime content is supplied separately through
    /// the executor's retained realization leases, not read into this record.
    pub fn prepare_delivery_records(
        &self,
        verifier: &lillux::InheritedDescriptorAuthority,
    ) -> Result<ConsumerDeliveryRecords> {
        use ryeos_external_execution_contract::restored_runtime_measurement::CONSUMER_VERIFIER_REMOTE_NAME;
        let bytes = self.canonical_bytes()?;
        let payload_bytes = self.verify_verifier_payload(verifier)?;
        ensure!(
            self.selection.archive_budget.maximum_entries >= 2
                && payload_bytes.checked_add(bytes.len() as u64).is_some_and(
                    |total| total <= self.selection.archive_budget.maximum_regular_bytes
                ),
            "consumer delivery records exceed protected archive budget"
        );
        let record_hash = lillux::sha256_hex(&bytes);
        let record =
            lillux::sealed_memfd(c"consumer-accepted-input", &bytes).map_err(anyhow::Error::msg)?;
        Ok(ConsumerDeliveryRecords {
            entries: vec![
                GuestStagingEntry::RegularFile {
                    path: CONSUMER_INPUT_RECORD_NAME.into(),
                    mode: 0o400,
                    bytes: bytes.len() as u64,
                    sha256: record_hash,
                },
                GuestStagingEntry::RegularFile {
                    path: CONSUMER_VERIFIER_REMOTE_NAME.into(),
                    mode: 0o500,
                    bytes: payload_bytes,
                    sha256: self.selection.verifier_artifact_hash.clone(),
                },
            ],
            descriptors: std::collections::BTreeMap::from([
                (CONSUMER_INPUT_RECORD_NAME.into(), record),
                (CONSUMER_VERIFIER_REMOTE_NAME.into(), verifier.clone()),
            ]),
        })
    }

    fn verify_verifier_payload(
        &self,
        verifier: &lillux::InheritedDescriptorAuthority,
    ) -> Result<u64> {
        use ryeos_external_execution_contract::restored_runtime_measurement::MAX_RESTORATION_VERIFIER_BYTES;
        let observation = verifier.regular_file_observation()?;
        let selected = self
            .purpose
            .remote_verifier_sources
            .get(&self.coordinate.scenario_id)
            .context("consumer delivery has no retained verifier source")?;
        ensure!(
            observation.size() > 0
                && observation.size() == selected.payload_bytes
                && observation.size() <= MAX_RESTORATION_VERIFIER_BYTES
                && verifier.digest_regular_file_stable_exact(&observation)?
                    == self.selection.verifier_artifact_hash,
            "consumer verifier payload differs from accepted selection"
        );
        Ok(observation.size())
    }

    /// The deterministic archive product ordering is the existing realization
    /// set ordering. Guest import must derive it from the same accepted record,
    /// never trust caller-authored product names as source identity.
    pub fn production_realizations(
        &self,
    ) -> Result<ryeos_state::objects::ExternalContentRealizationSet> {
        Ok(self.validate()?.realizations)
    }

    /// Join archive semantics to the exact attempt received over the admitted
    /// run channel. The caller still verifies the archive digest and authenticates
    /// that channel; arbitrary deserialized intent data is not run authority.
    pub fn validate_attempt(&self, intent: &RestoredVerifierAttemptIntent) -> Result<()> {
        self.validate()?;
        let RemoteVerificationPurpose::ConsumerRuntime { coordinate, .. } = &intent.purpose else {
            anyhow::bail!("owner measurement cannot consume a consumer input archive");
        };
        ensure!(
            coordinate == &self.coordinate
                && intent.verifier_artifact_hash == self.selection.verifier_artifact_hash,
            "consumer input archive differs from exact run attempt"
        );
        ensure!(
            intent.upload_bytes > 0
                && intent.upload_bytes <= self.selection.archive_budget.maximum_framed_bytes
                && lillux::valid_hash(&intent.upload_sha256)
                && intent.attempt_deadline_ms > 0,
            "consumer attempt exceeds protected archive budget or has invalid upload identity"
        );
        intent.consumer_challenge_digest()?;
        Ok(())
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES,
            "consumer input record exceeds its bound"
        );
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        ensure!(
            lillux::canonical_json(&value)?.as_bytes() == bytes,
            "consumer input record is not canonical"
        );
        let record: Self = serde_json::from_value(value)?;
        record.validate()?;
        Ok(record)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = lillux::canonical_json(&serde_json::to_value(self)?)?.into_bytes();
        ensure!(
            bytes.len() <= MAX_CONSUMER_INPUT_RECORD_BYTES,
            "consumer input record exceeds its bound"
        );
        Ok(bytes)
    }

    pub fn validate(&self) -> Result<QualificationExecutionEnvironment> {
        ensure!(self.schema == 1, "unsupported consumer input record schema");
        self.purpose
            .validate_remote_consumer_coordinate(&self.coordinate, &self.selection)?;
        self.requirement.validate()?;
        let content = self
            .purpose
            .consumer_content
            .as_ref()
            .context("consumer input record has no accepted content")?;
        let context = self
            .purpose
            .policy_source
            .policy
            .consumer_execution_context
            .as_ref()
            .context("consumer input record has no signed consumer context")?;
        let qualified_use = content
            .qualification_use
            .as_ref()
            .context("consumer input record has no admitted use")?;
        ensure!(
            self.requirement.qualification_requirement_digest()?
                == qualified_use.requirement_digest
                && self.requirement.runtime_product_declaration_id
                    == content.runtime_realization.id
                && self.requirement.runtime_recipe.executable_relative_path
                    == content.runtime_member.relative_path,
            "consumer input recipe differs from accepted use"
        );
        QualificationExecutionEnvironment::from_retained_production_consumer(content, context)
    }
}
