use super::*;

/// Stable target-side root for project-relative paths retained in a direct
/// execution plan. A daemon workspace is operational state selected after
/// admission; its thread-specific host path must not fragment artifact,
/// capsule, or replay identity.
pub const ADMITTED_DIRECT_PROJECT_ROOT: &str = "/ryeos/admitted-project";

/// Result of spawning the engine pipeline.
pub struct SpawnedItemAwaitingAttachment {
    pub pid: u32,
    pub pgid: i64,
    pub process_identity: crate::process::ExecutionProcessIdentity,
    /// Spawn-time metadata derived from the engine `SubprocessSpec`
    /// (e.g. `native_async` cancellation policy). Persisted alongside
    /// pid/pgid so the daemon shutdown / cancel paths can route
    /// termination without re-loading the spec.
    pub launch_metadata: crate::launch_metadata::RuntimeLaunchMetadata,
    spawned: ryeos_engine::dispatch::SpawnedExecutionAwaitingAttachment,
}

impl SpawnedItemAwaitingAttachment {
    pub fn release_after_attachment(self) -> Result<RunningItem> {
        let running = self
            .spawned
            .release_after_attachment()
            .map_err(|error| anyhow!("release item after durable attachment: {error}"))?;
        Ok(RunningItem {
            process_identity: self.process_identity,
            launch_metadata: self.launch_metadata,
            running,
        })
    }

    pub fn abort_and_reap(self) -> Result<()> {
        self.spawned
            .abort_and_reap()
            .map(|_| ())
            .map_err(|error| anyhow!("abort item awaiting attachment: {error}"))
    }
}

pub struct RunningItem {
    pub process_identity: crate::process::ExecutionProcessIdentity,
    pub launch_metadata: crate::launch_metadata::RuntimeLaunchMetadata,
    running: ryeos_engine::dispatch::RunningExecution,
}

/// A kind-neutral persistent subprocess held at Lillux's attachment boundary.
///
/// Exclusive session owners must durably attach [`Self::process_identity`]
/// before calling [`Self::release_after_attachment`]. Pooled sessions may
/// attach ownership to their in-memory pool and release immediately.
pub struct SpawnedPersistentSessionAwaitingAttachment {
    pub process_identity: crate::process::ExecutionProcessIdentity,
    spawned: ryeos_engine::dispatch::SpawnedExecutionAwaitingAttachment,
}

impl SpawnedPersistentSessionAwaitingAttachment {
    pub fn release_after_attachment(self) -> Result<ryeos_engine::dispatch::RunningExecution> {
        self.spawned
            .release_after_attachment()
            .map_err(|error| anyhow!("release daemon-owned persistent session: {error}"))
    }

    pub fn abort_and_reap(self) -> Result<()> {
        self.spawned
            .abort_and_reap()
            .map(|_| ())
            .map_err(|error| anyhow!("abort persistent session awaiting attachment: {error}"))
    }
}

impl RunningItem {
    pub fn abort(self) {
        self.running.abort();
    }

    /// Block until subprocess completes.
    pub fn wait(self) -> ExecutionCompletion {
        self.running.wait()
    }
}

/// A verified, fully built item plan retained from callback credential minting
/// through spawn. Its timeout comes from the exact plan the engine executes, so
/// credential lifetime cannot drift from a later plan rebuild.
pub struct PreparedItemPlan {
    plan: ExecutionPlan,
    pub timeout_secs: u64,
    root_subject_source_identity: ryeos_state::objects::DirectRootSourceIdentity,
    admitted_command: Option<ryeos_engine::isolation::IsolationDescriptorBoundCommand>,
}

impl PreparedItemPlan {
    pub fn execution_plan(&self) -> &ExecutionPlan {
        &self.plan
    }

    /// A private admitted-input root intentionally omits ambient project
    /// dependencies such as `.venv`. Refuse a plan whose interpreter was
    /// selected from the live project instead of relocating that path into a
    /// sparse workspace where it is absent (or letting captured launcher bytes
    /// hide undeclared site-packages).
    pub fn ensure_no_project_local_interpreter(&self, project_root: &Path) -> Result<()> {
        let spec = match self.plan.nodes.first() {
            Some(ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. }) => spec,
            _ => return Ok(()),
        };
        let command_is_project_local = Path::new(&spec.cmd).starts_with(project_root);
        let interpreter_env_is_project_local = spec.env_sources.iter().any(|(name, source)| {
            *source == ryeos_engine::contracts::RuntimeEnvSource::RuntimeInterpreter
                && spec
                    .env
                    .get(name)
                    .is_some_and(|value| Path::new(value).starts_with(project_root))
        });
        if command_is_project_local || interpreter_env_is_project_local {
            bail!(
                "private admitted-input execution refused: the selected interpreter comes from the live project; declare the interpreter/dependency environment as admitted runtime content or use a non-project interpreter"
            );
        }
        Ok(())
    }

    /// Replace the concrete project workspace with its stable logical target
    /// before artifact identity and closure admission. The returned root is
    /// retained in the capsule; a mutable spawn copy is relocated to the
    /// selected concrete workspace immediately before execution.
    pub fn bind_logical_project_root(
        &mut self,
        concrete_project_root: Option<&Path>,
    ) -> Result<Option<PathBuf>> {
        let logical_project_root =
            concrete_project_root.map(|_| PathBuf::from(ADMITTED_DIRECT_PROJECT_ROOT));
        relocate_admitted_direct_plan(
            &mut self.plan,
            concrete_project_root,
            logical_project_root.as_deref(),
        )?;
        Ok(logical_project_root)
    }

    pub fn runtime_ref(&self) -> Result<&str> {
        self.plan
            .runtime_identity
            .as_ref()
            .map(|identity| identity.runtime_ref.as_str())
            .ok_or_else(|| anyhow!("direct execution plan has no verified runtime identity"))
    }

    /// Bind admitted behavior to its daemon-owned workspace. This mutation
    /// happens before artifact identity and closure admission, so the exact
    /// working directory is part of the retained execution plan.
    pub fn bind_persistent_session_workspace(&mut self, workspace: &Path) -> Result<()> {
        if !workspace.is_absolute()
            || workspace
                .components()
                .enumerate()
                .any(|(index, component)| {
                    !matches!(
                        (index, component),
                        (0, std::path::Component::RootDir) | (_, std::path::Component::Normal(_))
                    )
                })
        {
            bail!("persistent-session workspace must be an absolute normalized path");
        }
        let spec = first_subprocess_spec_mut(&mut self.plan)?;
        spec.cwd = Some(workspace.to_path_buf());
        spec.timeout_secs = 0;
        self.timeout_secs = 0;
        Ok(())
    }

    /// Add the operational realization-set binding after recovering the
    /// admitted plan. The session channel is carried separately as typed
    /// isolation authority and never written into the authored subprocess
    /// specification as a raw descriptor.
    pub fn bind_persistent_session_spawn_environment(
        &mut self,
        external_realizations: Option<&str>,
        external_root: Option<&Path>,
        admitted_source: Option<&str>,
        admitted_source_entry: Option<&Path>,
        executable_search: Option<&str>,
    ) -> Result<()> {
        if external_realizations.is_some() != external_root.is_some() {
            bail!("persistent-session external identity and root must be bound together");
        }
        if admitted_source.is_some() != admitted_source_entry.is_some() {
            bail!("persistent-session source identity and entry must be bound together");
        }
        let spec = first_subprocess_spec_mut(&mut self.plan)?;
        if let Some(realizations) = external_realizations {
            spec.env.insert(
                "RYEOS_EXTERNAL_REALIZATIONS".to_owned(),
                realizations.to_owned(),
            );
            spec.env_sources.insert(
                "RYEOS_EXTERNAL_REALIZATIONS".to_owned(),
                RuntimeEnvSource::EnginePlan,
            );
        }
        if let Some(root) = external_root {
            let root = root
                .to_str()
                .ok_or_else(|| anyhow!("persistent-session external root is not valid UTF-8"))?;
            spec.env
                .insert("RYEOS_EXTERNAL_ROOT".to_owned(), root.to_owned());
            spec.env_sources.insert(
                "RYEOS_EXTERNAL_ROOT".to_owned(),
                RuntimeEnvSource::EnginePlan,
            );
        }
        if let Some(source) = admitted_source {
            if spec.env.contains_key("RYEOS_ADMITTED_SOURCE") {
                bail!("persistent-session plan attempts to override admitted source identity");
            }
            spec.env
                .insert("RYEOS_ADMITTED_SOURCE".to_owned(), source.to_owned());
            spec.env_sources.insert(
                "RYEOS_ADMITTED_SOURCE".to_owned(),
                RuntimeEnvSource::EnginePlan,
            );
        }
        if let Some(search) = executable_search {
            if spec.env.contains_key("RYEOS_EXECUTABLE_SEARCH") {
                bail!("persistent-session plan attempts to override executable search");
            }
            spec.env
                .insert("RYEOS_EXECUTABLE_SEARCH".to_owned(), search.to_owned());
            spec.env_sources.insert(
                "RYEOS_EXECUTABLE_SEARCH".to_owned(),
                RuntimeEnvSource::EnginePlan,
            );
        }
        let source_entry = admitted_source_entry
            .map(|path| {
                path.to_str()
                    .ok_or_else(|| anyhow!("admitted source entry path is not valid UTF-8"))
            })
            .transpose()?;
        let mut bound_entries = 0usize;
        for argument in &mut spec.args {
            if matches!(argument, PlanArgument::AdmittedSourceEntry) {
                let entry = source_entry.ok_or_else(|| {
                    anyhow!("persistent-session plan requires an admitted source entry")
                })?;
                *argument = PlanArgument::literal(entry);
                bound_entries += 1;
            }
        }
        match (source_entry.is_some(), bound_entries) {
            (false, 0) | (true, 1) => {}
            (false, _) => bail!("persistent-session plan has an unowned source entry"),
            (true, 0) => {
                bail!("persistent-session plan does not consume its admitted source entry")
            }
            (true, _) => {
                bail!("persistent-session plan consumes its admitted source entry more than once")
            }
        }
        Ok(())
    }

    pub fn bind_persistent_session_runtime_environment(
        &mut self,
        environment: &std::collections::BTreeMap<String, String>,
        allowlist: &[String],
    ) -> Result<()> {
        let spec = first_subprocess_spec_mut(&mut self.plan)?;
        for (name, value) in environment {
            if !allowlist.contains(name) {
                bail!("persistent-session runtime environment `{name}` is not protocol-authorized");
            }
            if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
                bail!("persistent-session runtime environment `{name}` is not bounded");
            }
            if spec.env.contains_key(name) || spec.env_sources.contains_key(name) {
                bail!("persistent-session runtime environment attempts to override `{name}`");
            }
            spec.env.insert(name.clone(), value.clone());
            spec.env_sources
                .insert(name.clone(), RuntimeEnvSource::EnginePlan);
        }
        Ok(())
    }

    pub fn admitted_artifact_identity(
        &self,
        resolved: &ResolvedExecutionRequest,
        protocol: &ryeos_engine::protocols::VerifiedProtocol,
    ) -> Result<ryeos_state::objects::AdmittedLaunchArtifactIdentity> {
        let canonical_plan = lillux::canonical_json(&admitted_execution_plan_value(&self.plan)?)?;
        let execution_plan_hash = lillux::sha256_hex(canonical_plan.as_bytes());
        let plan_runtime = self
            .plan
            .runtime_identity
            .as_ref()
            .ok_or_else(|| anyhow!("direct execution plan has no verified runtime identity"))?;
        let runtime_signer_fingerprint = plan_runtime
            .runtime_signer_fingerprint
            .clone()
            .ok_or_else(|| {
                anyhow!(
                    "direct runtime {} has no verified signer identity",
                    plan_runtime.runtime_ref
                )
            })?;
        let runtime_identity = ryeos_state::objects::DirectRuntimeIdentity {
            runtime_ref: plan_runtime.runtime_ref.clone(),
            runtime_source_space: match plan_runtime.runtime_source_space {
                ryeos_engine::contracts::ItemSpace::Project => {
                    ryeos_state::objects::DirectRuntimeSourceSpace::Project
                }
                ryeos_engine::contracts::ItemSpace::Bundle => {
                    ryeos_state::objects::DirectRuntimeSourceSpace::Bundle
                }
                ryeos_engine::contracts::ItemSpace::Node => {
                    anyhow::bail!("node-local configuration cannot be admitted as a direct runtime")
                }
            },
            runtime_content_hash: plan_runtime.runtime_content_hash.clone(),
            runtime_signer_fingerprint,
            runtime_bundle_manifest_hash: plan_runtime.runtime_bundle_manifest_hash.clone(),
            runtime_bundle_signer_fingerprint: plan_runtime
                .runtime_bundle_signer_fingerprint
                .clone(),
        };
        let executable_identity = match self.plan.nodes.first() {
            Some(ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. }) => {
                match spec.verified_command.as_ref() {
                    Some(ryeos_engine::contracts::PlanVerifiedCommand::BundleExecutor {
                        code,
                        provider: executor_bundle,
                    }) => ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
                        content_hash: code.content_hash.clone(),
                        executor_manifest_hash: executor_bundle.manifest_hash.clone(),
                        executor_manifest_signer_fingerprint: executor_bundle
                            .signer_fingerprint
                            .clone(),
                    },
                    Some(ryeos_engine::contracts::PlanVerifiedCommand::CapturedContent {
                        code,
                    }) => ryeos_state::objects::DirectExecutableIdentity::CapturedContent {
                        content_hash: code.content_hash.clone(),
                    },
                    None => ryeos_state::objects::DirectExecutableIdentity::NodePolicy,
                }
            }
            Some(ryeos_engine::contracts::PlanNode::Complete { .. }) | None => {
                ryeos_state::objects::DirectExecutableIdentity::NodePolicy
            }
        };
        let root_subject_source_identity = self.root_subject_source_identity.clone();
        let identity = ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executor_ref: resolved.executor_ref.clone(),
            root_subject_source_content_digest: resolved.resolved_item.content_hash.clone(),
            root_subject_signer_fingerprint: resolved
                .resolved_item
                .signature_header
                .as_ref()
                .map(|header| header.signer_fingerprint.clone()),
            root_subject_source_identity,
            protocol_ref: protocol.canonical_ref.clone(),
            protocol_content_hash: protocol.raw_content_digest.clone(),
            protocol_signer_fingerprint: protocol.signer_fingerprint.clone(),
            execution_plan_hash,
            executable_identity,
            runtime_identity,
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn admit_execution_closure(
        &mut self,
        cas: &lillux::cas::CasStore,
        isolation: &ryeos_engine::isolation::IsolationRuntime,
        protocol: &ryeos_engine::protocols::VerifiedProtocol,
        protocol_trust_store: &ryeos_engine::trust::TrustStore,
        admitted_project_root: Option<&Path>,
    ) -> Result<ryeos_state::objects::AdmittedExecutionClosure> {
        validate_direct_plan_portability(&self.plan, admitted_project_root)?;
        let original_command = match self.plan.nodes.first() {
            Some(ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. }) => {
                spec.verified_command.clone()
            }
            _ => None,
        };
        let execution_plan = admitted_execution_plan_value(&self.plan)?;
        let protocol_descriptor_document = capture_signed_descriptor_document(
            &protocol.descriptor_path,
            &protocol.raw_content_digest,
            &protocol.signer_fingerprint,
            protocol_trust_store,
        )?;
        let Some(command) = original_command else {
            return Ok(
                ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
                    execution_plan,
                    protocol_descriptor_document,
                    command: ryeos_state::objects::AdmittedDirectCommandClosure::NodePolicy,
                    admitted_project_root: admitted_project_root.map(Path::to_path_buf),
                },
            );
        };
        let original = command.code().clone();
        let execution_path = admitted_direct_command_path(&original)?;
        let source = lillux::open_pinned_regular_file_no_follow(&original.source_path)
            .with_context(|| {
                format!(
                    "open admitted direct executable {} through Lillux",
                    original.source_path.display()
                )
            })?;
        let stored =
            cas.put_blob_from_open_regular(source.try_clone_descriptor()?, &original.source_path)?;
        if stored.hash != original.content_hash {
            bail!(
                "direct executable changed before admission: expected {}, captured {}",
                original.content_hash,
                stored.hash
            );
        }
        let (blob, _) = cas
            .open_blob(&stored.hash)?
            .ok_or_else(|| anyhow!("admitted direct executable blob disappeared"))?;
        let cached_identity = ryeos_engine::isolation::IsolationVerifiedCode {
            source_path: execution_path.clone(),
            content_hash: stored.hash.clone(),
        };
        self.admitted_command =
            Some(isolation.bind_admitted_verified_command(cached_identity.clone(), blob)?);
        let spec = first_subprocess_spec_mut(&mut self.plan)?;
        spec.cmd = execution_path.display().to_string();
        match spec
            .verified_command
            .as_mut()
            .expect("verified command checked above")
        {
            ryeos_engine::contracts::PlanVerifiedCommand::BundleExecutor { code, .. }
            | ryeos_engine::contracts::PlanVerifiedCommand::CapturedContent { code } => {
                *code = cached_identity;
            }
        }
        self.timeout_secs = spec.timeout_secs;

        Ok(
            ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
                execution_plan,
                protocol_descriptor_document,
                command: ryeos_state::objects::AdmittedDirectCommandClosure::ContentAddressed {
                    executable_blob_hash: stored.hash,
                    execution_path,
                },
                admitted_project_root: admitted_project_root.map(Path::to_path_buf),
            },
        )
    }

    pub fn recover_from_execution_closure(
        capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
        cas: &lillux::cas::CasStore,
        isolation: &ryeos_engine::isolation::IsolationRuntime,
        effective_project_root: Option<&Path>,
    ) -> Result<Self> {
        Self::recover_from_direct_parts(
            &capsule.execution_closure,
            &capsule.artifact_identity,
            cas,
            isolation,
            effective_project_root,
        )
    }

    pub fn recover_from_persistent_session_capsule(
        capsule: &ryeos_state::objects::AdmittedPersistentSessionCapsule,
        cas: &lillux::cas::CasStore,
        isolation: &ryeos_engine::isolation::IsolationRuntime,
        effective_workspace: &Path,
    ) -> Result<Self> {
        Self::recover_from_direct_parts(
            &capsule.execution_closure,
            &capsule.artifact_identity,
            cas,
            isolation,
            Some(effective_workspace),
        )
    }

    fn recover_from_direct_parts(
        execution_closure: &ryeos_state::objects::AdmittedExecutionClosure,
        artifact_identity: &ryeos_state::objects::AdmittedLaunchArtifactIdentity,
        cas: &lillux::cas::CasStore,
        isolation: &ryeos_engine::isolation::IsolationRuntime,
        effective_project_root: Option<&Path>,
    ) -> Result<Self> {
        let ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan,
            command,
            admitted_project_root,
            ..
        } = execution_closure
        else {
            bail!("direct recovery found a non-direct admitted execution closure");
        };
        let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            executor_ref,
            execution_plan_hash,
            executable_identity,
            runtime_identity,
            root_subject_source_identity,
            ..
        } = artifact_identity
        else {
            bail!("direct recovery found a non-direct admitted artifact identity");
        };
        let canonical_plan = lillux::canonical_json(execution_plan)?;
        let observed_plan_hash = lillux::sha256_hex(canonical_plan.as_bytes());
        if &observed_plan_hash != execution_plan_hash {
            bail!(
                "admitted direct execution plan hash mismatch: expected {execution_plan_hash}, observed {observed_plan_hash}"
            );
        }
        let expected_command_hash = match executable_identity {
            ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
                content_hash, ..
            }
            | ryeos_state::objects::DirectExecutableIdentity::CapturedContent { content_hash } => {
                content_hash
            }
            ryeos_state::objects::DirectExecutableIdentity::NodePolicy => {
                bail!("node-policy direct execution is not restart-recoverable")
            }
        };
        let ryeos_state::objects::AdmittedDirectCommandClosure::ContentAddressed {
            executable_blob_hash,
            execution_path,
        } = command
        else {
            bail!("node-policy direct execution is not restart-recoverable");
        };
        if expected_command_hash != executable_blob_hash {
            bail!("admitted direct executable blob contradicts artifact identity");
        }
        let mut plan: ExecutionPlan = serde_json::from_value(execution_plan.clone())
            .context("decode admitted direct execution plan")?;
        validate_admitted_direct_executor_binding(&plan, executor_ref)?;
        let plan_runtime = plan
            .runtime_identity
            .as_ref()
            .ok_or_else(|| anyhow!("admitted direct execution plan has no runtime identity"))?;
        let expected_runtime_space = match runtime_identity.runtime_source_space {
            ryeos_state::objects::DirectRuntimeSourceSpace::Project => ItemSpace::Project,
            ryeos_state::objects::DirectRuntimeSourceSpace::Bundle => ItemSpace::Bundle,
        };
        if plan_runtime.runtime_ref != runtime_identity.runtime_ref
            || plan_runtime.runtime_source_space != expected_runtime_space
            || plan_runtime.runtime_content_hash != runtime_identity.runtime_content_hash
            || plan_runtime.runtime_signer_fingerprint.as_deref()
                != Some(runtime_identity.runtime_signer_fingerprint.as_str())
            || plan_runtime.runtime_bundle_manifest_hash
                != runtime_identity.runtime_bundle_manifest_hash
            || plan_runtime.runtime_bundle_signer_fingerprint
                != runtime_identity.runtime_bundle_signer_fingerprint
        {
            bail!("admitted direct execution plan contradicts runtime identity");
        }
        let original_spec = match plan.nodes.first() {
            Some(ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. }) => spec,
            Some(ryeos_engine::contracts::PlanNode::Complete { .. }) => {
                bail!("admitted direct execution plan entrypoint is complete")
            }
            None => bail!("admitted direct execution plan is empty"),
        };
        let original_command = original_spec
            .verified_command
            .as_ref()
            .ok_or_else(|| anyhow!("admitted direct execution plan has no verified command"))?;
        let command_identity_matches = match (executable_identity, original_command) {
            (
                ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
                    content_hash,
                    executor_manifest_hash,
                    executor_manifest_signer_fingerprint,
                },
                ryeos_engine::contracts::PlanVerifiedCommand::BundleExecutor {
                    code,
                    provider: executor_bundle,
                },
            ) => {
                &code.content_hash == content_hash
                    && &executor_bundle.manifest_hash == executor_manifest_hash
                    && &executor_bundle.signer_fingerprint == executor_manifest_signer_fingerprint
            }
            (
                ryeos_state::objects::DirectExecutableIdentity::CapturedContent { content_hash },
                ryeos_engine::contracts::PlanVerifiedCommand::CapturedContent { code },
            ) => &code.content_hash == content_hash,
            _ => false,
        };
        if !command_identity_matches {
            bail!("admitted direct execution plan contradicts command identity");
        }
        relocate_admitted_direct_plan(
            &mut plan,
            admitted_project_root.as_deref(),
            effective_project_root,
        )?;
        let spec = first_subprocess_spec_mut(&mut plan)?;
        let serialized_command = spec
            .verified_command
            .as_ref()
            .ok_or_else(|| anyhow!("admitted direct execution plan has no verified command"))?;
        if serialized_command.code().content_hash != *executable_blob_hash {
            bail!("admitted direct execution plan command contradicts executable blob");
        }
        let (blob, _) = cas
            .open_blob(executable_blob_hash)?
            .ok_or_else(|| anyhow!("admitted direct executable blob is unavailable"))?;
        let cached_identity = ryeos_engine::isolation::IsolationVerifiedCode {
            source_path: execution_path.clone(),
            content_hash: executable_blob_hash.clone(),
        };
        let admitted_command =
            isolation.bind_admitted_verified_command(cached_identity.clone(), blob)?;
        spec.cmd = execution_path.display().to_string();
        match spec
            .verified_command
            .as_mut()
            .expect("verified command checked above")
        {
            ryeos_engine::contracts::PlanVerifiedCommand::BundleExecutor { code, .. }
            | ryeos_engine::contracts::PlanVerifiedCommand::CapturedContent { code } => {
                *code = cached_identity;
            }
        }
        let timeout_secs = spec.timeout_secs;
        Ok(Self {
            plan,
            timeout_secs,
            root_subject_source_identity: root_subject_source_identity.clone(),
            admitted_command: Some(admitted_command),
        })
    }

    /// Rebind only the operational project location of an already-admitted
    /// direct plan. The closure retains the original plan bytes; this mutable
    /// copy is the one handed to the current spawn.
    pub fn relocate_project_for_spawn(
        &mut self,
        admitted_project_root: Option<&Path>,
        effective_project_root: Option<&Path>,
    ) -> Result<()> {
        relocate_admitted_direct_plan(
            &mut self.plan,
            admitted_project_root,
            effective_project_root,
        )
    }

    /// Spawn a recovered persistent-session plan under projectless scratch
    /// authority. The daemon pool, rather than thread state, owns the returned
    /// running process; a daemon restart therefore reopens the capsule and
    /// starts a fresh matching process.
    pub fn spawn_persistent_session_held(
        self,
        state: &crate::state::AppState,
        workspace: &Path,
        external_mounts: Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
        target_channel: ryeos_engine::isolation::IsolationTargetChannelAuthority,
        lifecycle: &ryeos_state::objects::PersistentSessionLifecycleContract,
        workspace_authority: ryeos_engine::protocols::descriptor::PersistentSessionWorkspaceAuthority,
        network_authority: ryeos_engine::protocols::descriptor::PersistentSessionNetworkAuthority,
        state_root: Option<&Path>,
        session_identity: &str,
    ) -> Result<SpawnedPersistentSessionAwaitingAttachment> {
        if session_identity.is_empty() || session_identity.len() > 128 {
            bail!("persistent-session process identity is not canonical");
        }
        use ryeos_engine::protocols::descriptor::{
            PersistentSessionNetworkAuthority, PersistentSessionWorkspaceAuthority,
        };
        let (project_authority, filesystem_authority_ceiling) = match workspace_authority {
            PersistentSessionWorkspaceAuthority::EphemeralScratch => (
                ryeos_engine::isolation::IsolationProjectAuthority::EphemeralScratch,
                ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::CapturedExecution,
            ),
            PersistentSessionWorkspaceAuthority::RuntimeWorkspace => (
                ryeos_engine::isolation::IsolationProjectAuthority::RuntimeWorkspace,
                ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
            ),
        };
        if state_root.is_some()
            && !matches!(
                workspace_authority,
                PersistentSessionWorkspaceAuthority::RuntimeWorkspace
            )
        {
            bail!("persistent-session state root requires runtime-workspace authority");
        }
        let network_authority_ceiling = match network_authority {
            PersistentSessionNetworkAuthority::Isolated => {
                ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::Isolated
            }
            PersistentSessionNetworkAuthority::NodePolicy => {
                ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::NodePolicy
            }
        };
        let context = EngineContext {
            app_root: state.config.app_root.clone(),
            isolation: state.isolation.clone(),
            isolation_project_authority: project_authority,
            isolation_filesystem_authority_ceiling: filesystem_authority_ceiling,
            isolation_network_authority_ceiling: network_authority_ceiling,
            isolation_live_access_authority: None,
            isolation_state_root: state_root.map(Path::to_path_buf),
            isolation_checkpoint_dir: None,
            isolation_checkpoint_authority: None,
            isolation_daemon_socket_path: None,
            // A persistent session executes its captured command and exact
            // external realizations. Mutable bundle roots and node trust state
            // are neither inputs nor ambient read authority for that worker.
            isolation_bundle_roots: Vec::new(),
            isolation_node_trusted_keys_dir: None,
            isolation_verified_code: Vec::new(),
            isolation_verified_command: self.admitted_command,
            isolation_external_read_only_mounts: external_mounts,
            isolation_target_channel: Some(target_channel),
            isolation_workspace: Some(workspace.to_path_buf()),
            subprocess_limits: Some(lillux::SubprocessLimits {
                max_open_files: None,
                max_address_space_bytes: Some(lifecycle.max_address_space_bytes),
                max_cpu_seconds: Some(lifecycle.max_cpu_seconds),
                max_processes: Some(lifecycle.real_uid_process_limit),
                max_stdout_bytes: None,
                max_stderr_bytes: None,
            }),
            inherited_fds: Vec::new(),
            thread_id: session_identity.to_owned(),
            chain_root_id: session_identity.to_owned(),
            current_site_id: state.threads.site_id().to_owned(),
            origin_site_id: state.threads.site_id().to_owned(),
            upstream_site_id: None,
            upstream_thread_id: None,
            continuation_from_id: None,
            requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                ryeos_engine::contracts::Principal {
                    fingerprint: state.identity.fingerprint().to_owned(),
                    scopes: Vec::new(),
                },
            ),
            project_context: ProjectContext::None,
            launch_mode: LaunchMode::Wait,
        };
        let spawned = state
            .engine
            .spawn_plan(&context, &self.plan)
            .map_err(|error| anyhow!("spawn persistent session: {error}"))?;
        #[cfg(target_os = "linux")]
        let identity_result = crate::process::capture_execution_process_identity_from_pidfd(
            spawned.pid() as i64,
            Some(spawned.pgid()),
            spawned.pidfd(),
        )
        .context("capture held persistent-session identity from Lillux pidfd");
        #[cfg(not(target_os = "linux"))]
        let identity_result = crate::process::capture_execution_process_identity(
            spawned.pid() as i64,
            Some(spawned.pgid()),
        )
        .context("capture held persistent-session identity");
        let process_identity = match identity_result {
            Ok(identity) => identity,
            Err(error) => {
                let cleanup = spawned.abort_and_reap().err();
                return Err(match cleanup {
                    Some(cleanup) => {
                        error.context(format!("held persistent-session cleanup failed: {cleanup}"))
                    }
                    None => error,
                });
            }
        };
        Ok(SpawnedPersistentSessionAwaitingAttachment {
            process_identity,
            spawned,
        })
    }
}

fn validate_admitted_direct_executor_binding(
    plan: &ExecutionPlan,
    admitted_executor_ref: &str,
) -> Result<()> {
    // `root_executor_id` is the resolved terminal executor. The admitted
    // artifact identity binds the executor declared by the root item, which
    // is the first hop after the root in the complete executor chain. These
    // identities differ whenever an alias/runtime chain has more than one
    // hop, so recovery must compare like with like.
    if plan.executor_chain.first().map(String::as_str) != Some(plan.root_ref.as_str())
        || plan.executor_chain.get(1).map(String::as_str) != Some(admitted_executor_ref)
    {
        bail!("admitted direct execution plan contradicts executor ref");
    }
    Ok(())
}

fn capture_signed_descriptor_document(
    path: &Path,
    expected_content_hash: &str,
    expected_signer: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<String> {
    let bytes = lillux::read_regular_file_bounded_no_follow(path, 8 * 1024 * 1024)
        .with_context(|| format!("read admitted descriptor {} through Lillux", path.display()))?;
    let document = String::from_utf8(bytes)
        .with_context(|| format!("decode admitted descriptor {} as UTF-8", path.display()))?;
    let header =
        lillux::signature::parse_signature_line(document.lines().next().unwrap_or(""), "#", None)
            .ok_or_else(|| anyhow!("admitted descriptor has no valid signature header"))?;
    let body = lillux::signature::strip_signature_lines(&document);
    let observed_hash = lillux::signature::content_hash(&body);
    if observed_hash != expected_content_hash
        || header.content_hash != expected_content_hash
        || header.signer_fingerprint != expected_signer
    {
        bail!("admitted descriptor document contradicts its verified identity");
    }
    let signer = trust_store
        .get(expected_signer)
        .ok_or_else(|| anyhow!("admitted descriptor signer is no longer trusted"))?;
    if !lillux::signature::verify_signature(
        expected_content_hash,
        &header.signature_b64,
        &signer.verifying_key,
    ) {
        bail!("admitted descriptor signature does not verify");
    }
    Ok(document)
}

/// Stable target-side name for exact retained command bytes. The host path
/// used to open those bytes is operational admission state; it must not enter
/// the retained plan/capsule identity or make equal bundle installs differ.
fn admitted_direct_command_path(
    code: &ryeos_engine::isolation::IsolationVerifiedCode,
) -> Result<PathBuf> {
    ryeos_state::objects::admitted_direct_command_execution_path(
        &code.content_hash,
        &code.source_path,
    )
}

/// Canonical retained plan value. Diagnostic source paths are skipped by the
/// plan wire, and command pathnames are rewritten to their stable admitted
/// target path before the execution-plan hash is computed.
fn admitted_execution_plan_value(plan: &ExecutionPlan) -> Result<Value> {
    let mut admitted = plan.clone();
    if let Some(ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. }) =
        admitted.nodes.first_mut()
        && let Some(command) = spec.verified_command.as_mut()
    {
        let original = command.code().source_path.clone();
        let execution_path = admitted_direct_command_path(command.code())?;
        let original_text = original
            .to_str()
            .ok_or_else(|| anyhow!("admitted direct command source path is not valid UTF-8"))?;
        let execution_text = execution_path.to_str().expect("constant path is UTF-8");
        spec.cmd = execution_text.to_owned();
        for argument in &mut spec.args {
            if argument.literal_value() == Some(original_text) {
                *argument = PlanArgument::literal(execution_text);
            }
        }
        for value in spec.env.values_mut() {
            if value == original_text {
                *value = execution_text.to_owned();
            }
        }
        match command {
            ryeos_engine::contracts::PlanVerifiedCommand::BundleExecutor { code, .. }
            | ryeos_engine::contracts::PlanVerifiedCommand::CapturedContent { code } => {
                code.source_path = execution_path;
            }
        }
    }
    serde_json::to_value(admitted).context("encode admitted direct execution plan")
}

fn first_subprocess_spec_mut(plan: &mut ExecutionPlan) -> Result<&mut PlanSubprocessSpec> {
    match plan.nodes.first_mut() {
        Some(ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. }) => Ok(spec),
        Some(ryeos_engine::contracts::PlanNode::Complete { .. }) => {
            bail!("item plan entrypoint is complete, not a subprocess")
        }
        None => bail!("item plan is empty"),
    }
}

fn validate_direct_plan_portability(
    plan: &ExecutionPlan,
    admitted_project_root: Option<&Path>,
) -> Result<()> {
    let mut cloned = plan.clone();
    relocate_admitted_direct_plan(&mut cloned, admitted_project_root, admitted_project_root)
}

fn relocate_admitted_direct_plan(
    plan: &mut ExecutionPlan,
    admitted_project_root: Option<&Path>,
    effective_project_root: Option<&Path>,
) -> Result<()> {
    let (Some(admitted_root), Some(effective_root)) =
        (admitted_project_root, effective_project_root)
    else {
        if admitted_project_root.is_some() != effective_project_root.is_some() {
            bail!("admitted direct plan project authority changed during recovery");
        }
        return Ok(());
    };
    let normalized_absolute = |root: &Path| {
        root.is_absolute()
            && root.components().count() >= 2
            && root.components().enumerate().all(|(index, component)| {
                matches!(
                    (index, component),
                    (0, std::path::Component::RootDir) | (_, std::path::Component::Normal(_))
                )
            })
    };
    if !normalized_absolute(admitted_root) || !normalized_absolute(effective_root) {
        bail!("direct plan relocation roots must be absolute and normalized");
    }
    let relocate_path = |path: &mut PathBuf| -> Result<()> {
        if path.starts_with(admitted_root) {
            let relative = path
                .strip_prefix(admitted_root)
                .context("relocate admitted direct plan path")?;
            if relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                bail!("admitted direct plan path escapes its project root");
            }
            *path = effective_root.join(relative);
        }
        Ok(())
    };
    let admitted_text = admitted_root
        .to_str()
        .ok_or_else(|| anyhow!("admitted direct plan project root is not valid UTF-8"))?;
    let effective_text = effective_root
        .to_str()
        .ok_or_else(|| anyhow!("effective direct plan project root is not valid UTF-8"))?;
    let relocate_string = |label: &str, value: &mut String| -> Result<()> {
        if value == admitted_text {
            *value = effective_text.to_string();
        } else {
            let prefix = format!("{}/", admitted_text.trim_end_matches('/'));
            if value.starts_with(&prefix) {
                let relative = &value[prefix.len()..];
                if relative.is_empty()
                    || Path::new(relative)
                        .components()
                        .any(|component| !matches!(component, std::path::Component::Normal(_)))
                {
                    bail!("admitted direct plan {label} escapes its project root");
                }
                *value = format!("{}/{}", effective_text.trim_end_matches('/'), relative);
            } else if value.contains(admitted_text) {
                bail!("admitted direct plan {label} embeds its project root in an untyped string");
            }
        }
        Ok(())
    };
    for node in &mut plan.nodes {
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess {
            spec, tool_path, ..
        } = node
        else {
            continue;
        };
        if let Some(path) = tool_path {
            relocate_path(path)?;
        }
        if let Some(cwd) = &mut spec.cwd {
            relocate_path(cwd)?;
        }
        for arg in &mut spec.args {
            if let PlanArgument::Literal { value } = arg {
                relocate_string("argument", value)?;
            }
        }
        for value in spec.env.values_mut() {
            relocate_string("environment value", value)?;
        }
        if let Some(stdin) = &mut spec.stdin {
            match stdin {
                ryeos_engine::contracts::PlanStdin::Opaque { data } => {
                    if data.contains(admitted_text) {
                        bail!("admitted direct plan opaque stdin embeds its project root");
                    }
                }
                ryeos_engine::contracts::PlanStdin::RuntimeParameters {
                    parameters,
                    project_path,
                } => {
                    if project_path
                        .as_deref()
                        .is_some_and(|path| path != admitted_root)
                    {
                        bail!(
                            "admitted direct plan runtime parameter project_path contradicts its project authority"
                        );
                    }
                    if project_path.is_some() {
                        *project_path = Some(effective_root.to_path_buf());
                    }
                    if json_value_contains_text(parameters, admitted_text) {
                        bail!(
                            "admitted direct plan runtime parameters embed its project root outside the project_path binding"
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn json_value_contains_text(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value.contains(needle),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_value_contains_text(value, needle)),
        serde_json::Value::Object(values) => values
            .iter()
            .any(|(key, value)| key.contains(needle) || json_value_contains_text(value, needle)),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            false
        }
    }
}

pub fn prepare_item_plan(
    engine: &Engine,
    resolved: &ResolvedExecutionRequest,
    isolation: &ryeos_engine::isolation::IsolationRuntime,
    lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    live_access: Option<&ryeos_engine::isolation::IsolationLiveAccessAuthority>,
    sealed_content: Option<&dyn ryeos_engine::project_content::SealedDependencyBytes>,
) -> Result<PreparedItemPlan> {
    engine.with_checked_bundle_generation(|_generation| {
        let verified = super::verified_execution_subject(engine, resolved)?;
        let mut plan =
            super::build_execution_plan_for_request(engine, resolved, &verified, sealed_content)?;
        let root_subject_source_identity =
            if resolved.resolved_item.source_space == ryeos_engine::contracts::ItemSpace::Bundle {
                let expected_name = match &resolved.resolved_item.source_root {
                    ryeos_engine::contracts::ItemSourceRoot::Bundle { name } => name.as_str(),
                    other => bail!(
                        "bundle item has no registered typed source-root authority: {other:?}"
                    ),
                };
                let root = engine.registered_bundle_root(expected_name).ok_or_else(|| {
                    anyhow!(
                        "bundle `{expected_name}` is absent from the retained registered generation"
                    )
                })?;
                let identity = ryeos_engine::plan_builder::verify_bundle_source_manifest_identity(
                    root,
                    expected_name,
                    &engine.node_trust_store,
                )?;
                ryeos_state::objects::DirectRootSourceIdentity::Bundle {
                    manifest_hash: identity.body_digest,
                    manifest_signer_fingerprint: identity.signer_fingerprint,
                }
            } else {
                ryeos_state::objects::DirectRootSourceIdentity::Project
            };
        if lifecycle_authority.recovery
            == ryeos_state::objects::ExecutionRecoveryAuthority::RestartRecoverable
        {
            let spec = match plan.nodes.first_mut() {
                Some(ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. }) => spec,
                Some(ryeos_engine::contracts::PlanNode::Complete { .. }) => {
                    bail!("item plan entrypoint is complete, not a subprocess")
                }
                None => bail!("item plan is empty"),
            };
            if spec.verified_command.is_none() {
                let project_root = match &resolved.plan_context.project_context {
                    ProjectContext::LocalPath { path } => Some(path.as_path()),
                    ProjectContext::None
                    | ProjectContext::SnapshotHash { .. }
                    | ProjectContext::ProjectRef { .. } => None,
                };
                let captured = isolation
                    .capture_verified_command(Path::new(&spec.cmd), project_root, live_access)
                    .map_err(|error| anyhow!("capture direct executable identity: {error}"))?;
                spec.verified_command = Some(
                    ryeos_engine::contracts::PlanVerifiedCommand::CapturedContent {
                        code: captured,
                    },
                );
            }
        }
        let timeout_secs = match plan.nodes.first() {
            Some(ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. }) => {
                spec.timeout_secs
            }
            Some(ryeos_engine::contracts::PlanNode::Complete { .. }) => {
                bail!("item plan entrypoint is complete, not a subprocess")
            }
            None => bail!("item plan is empty"),
        };
        Ok(PreparedItemPlan {
            plan,
            timeout_secs,
            root_subject_source_identity,
            admitted_command: None,
        })
    })
}

/// Compile a direct plan from an engine-verified root carrier and the exact
/// retained root bytes captured by an outer admitted launch.  The root path is
/// provenance/parser context only; it is never reopened for program bytes.
pub fn prepare_captured_item_plan(
    engine: &Engine,
    resolved: &ResolvedExecutionRequest,
    verified: &VerifiedItem,
    root_source: &str,
    isolation: &ryeos_engine::isolation::IsolationRuntime,
    sealed_content: Option<&dyn ryeos_engine::project_content::SealedDependencyBytes>,
) -> Result<PreparedItemPlan> {
    engine.with_checked_bundle_generation(|_generation| {
        if verified.resolved.canonical_ref.to_string() != resolved.item_ref
            || verified.resolved.raw_content_digest != resolved.root_raw_content_digest
        {
            bail!("captured direct-plan subject contradicts its request carrier");
        }
        let mut plan = engine.build_plan_from_captured_root(
            &resolved.plan_context,
            verified,
            root_source,
            &resolved.parameters,
            &resolved.plan_context.execution_hints,
            sealed_content,
        )?;
        let expected_name = match &verified.resolved.source_root {
            ryeos_engine::contracts::ItemSourceRoot::Bundle { name } => name.as_str(),
            other => bail!(
                "captured bundle item has no registered typed source-root authority: {other:?}"
            ),
        };
        let root = engine
            .registered_bundle_root(expected_name)
            .ok_or_else(|| {
                anyhow!(
                    "captured bundle `{expected_name}` is absent from the registered generation"
                )
            })?;
        let identity = ryeos_engine::plan_builder::verify_bundle_source_manifest_identity(
            root,
            expected_name,
            &engine.node_trust_store,
        )?;
        let root_subject_source_identity = ryeos_state::objects::DirectRootSourceIdentity::Bundle {
            manifest_hash: identity.body_digest,
            manifest_signer_fingerprint: identity.signer_fingerprint,
        };
        let spec = first_subprocess_spec_mut(&mut plan)?;
        if spec.verified_command.is_none() {
            let captured = isolation
                .capture_verified_command(Path::new(&spec.cmd), None, None)
                .map_err(|error| anyhow!("capture persistent-session executable: {error}"))?;
            spec.verified_command = Some(
                ryeos_engine::contracts::PlanVerifiedCommand::CapturedContent { code: captured },
            );
        }
        let timeout_secs = spec.timeout_secs;
        Ok(PreparedItemPlan {
            plan,
            timeout_secs,
            root_subject_source_identity,
            admitted_command: None,
        })
    })
}

/// Run the prepared engine plan's spawn phase.
/// Returns a handle with pid/pgid that the daemon can persist before calling wait().
///
/// If `thread_state_dir` is supplied AND the resolved spec declares
/// `native_resume`, the daemon-side checkpoint directory
/// (`<thread_state_dir>/checkpoints/`) is created and injected as
/// `RYEOS_CHECKPOINT_DIR` into the subprocess env. The path is also
/// captured in `SpawnedItemAwaitingAttachment.launch_metadata.checkpoint_dir` so the
/// daemon can persist it for the resume path. When `is_resume = true`,
/// `RYEOS_RESUME=1` is also injected so replay-aware tools can branch
/// on cold-start vs. resume.
pub struct SpawnItemParams<'a> {
    pub engine: &'a Engine,
    pub resolved: &'a ResolvedExecutionRequest,
    /// Exact verified plan used to derive callback credential lifetime.
    pub prepared_plan: PreparedItemPlan,
    pub thread_id: &'a str,
    pub chain_root_id: &'a str,
    pub vault_bindings: std::collections::HashMap<String, String>,
    /// Exact signed-protocol environment, with each injection retaining its
    /// typed vocabulary source through final composition.
    pub protocol_env_bindings: Vec<EnvBinding>,
    pub roots: DaemonRootEnv,
    pub isolation: Arc<ryeos_engine::isolation::IsolationRuntime>,
    pub isolation_project_authority: ryeos_engine::isolation::IsolationProjectAuthority,
    pub isolation_live_access_authority:
        Option<ryeos_engine::isolation::IsolationLiveAccessAuthority>,
    pub isolation_external_read_only_mounts:
        Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
    /// Exact node trust-store root supplied by daemon configuration. Never
    /// reconstructed from an application or bundle filesystem layout.
    pub isolation_node_trusted_keys_dir: std::path::PathBuf,
    /// Optional daemon-owned workspace for projectless admitted mechanics.
    /// Ordinary item launches leave this absent and retain their project
    /// context as the isolation root.
    pub isolation_workspace: Option<std::path::PathBuf>,
    /// Deliberately inherited, already-open protocol descriptors.
    pub inherited_fds: Vec<std::sync::Arc<std::fs::File>>,
    /// Canonical JSON of the launch's sealed external realization set, or
    /// `None` when the program realizes nothing. Injected per spawn so a
    /// runtime references the identity it executes under without
    /// re-observing any content.
    pub external_realizations_env: Option<String>,
    /// Canonical, path-free identity of the source closure whose exact bytes
    /// are mounted for this spawn.
    pub admitted_source_env: Option<String>,
    /// Exact daemon socket requested by the verified callback channel, or
    /// `None` for a callback-free launch.
    pub isolation_daemon_socket_path: Option<&'a std::path::Path>,
    pub thread_state_dir: Option<&'a std::path::Path>,
    pub is_resume: bool,
    pub original_snapshot_hash: Option<&'a str>,
    /// The spawn's deliberate state-root override
    /// (`provenance.state_root_override()`), persisted on the resume
    /// context so a resumed run keeps the same state/callback anchor.
    pub state_root: Option<&'a std::path::Path>,
}

#[tracing::instrument(
    name = "thread:spawn",
    skip(params),
    fields(
        thread_id = %params.thread_id,
        chain_root_id = %params.chain_root_id,
        item_ref = %params.resolved.item_ref,
        is_resume = params.is_resume,
        snapshot_pinned = params.original_snapshot_hash.is_some(),
    )
)]
pub fn spawn_item(params: SpawnItemParams<'_>) -> Result<SpawnedItemAwaitingAttachment> {
    let SpawnItemParams {
        engine,
        resolved,
        prepared_plan,
        thread_id,
        chain_root_id,
        vault_bindings,
        protocol_env_bindings,
        roots,
        isolation,
        isolation_project_authority,
        isolation_live_access_authority,
        isolation_external_read_only_mounts,
        isolation_node_trusted_keys_dir,
        isolation_workspace,
        inherited_fds,
        external_realizations_env,
        admitted_source_env,
        isolation_daemon_socket_path,
        thread_state_dir,
        is_resume,
        original_snapshot_hash: _,
        state_root,
    } = params;
    let app_root = roots
        .app_root
        .as_deref()
        .map(std::path::PathBuf::from)
        .context("spawn roots missing RYEOS_APP_ROOT")?;
    // vault_bindings: user-provided secret/capability env vars.
    // protocol_env_bindings: the verified terminator protocol's exact signed
    // env contract. Values are produced from daemon-owned launch facts by the
    // runner, never inherited from the daemon's process environment.
    let mut plan = prepared_plan.plan;

    // Compose every subprocess node from allowlisted parent env, daemon roots,
    // declared secrets, engine-plan bindings, and the verified terminator
    // protocol's exact typed injections. No callback key is manufactured here;
    // callback-free protocols therefore stay callback-free through `env_clear`.
    let secret_map: std::collections::BTreeMap<String, String> = vault_bindings
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Allocate native-resume env before final env composition so the
    // builder remains the final authority. Preserve the existing
    // FirstWins behavior: only the first native_resume subprocess gets
    // the checkpoint/resume bindings.
    let mut allocated_checkpoint_dir: Option<std::path::PathBuf> = None;
    let mut allocated_checkpoint_authority: Option<Arc<lillux::PinnedDirectory>> = None;
    let mut resume_env_for_first_native_resume: Option<Vec<EnvBinding>> = None;
    // This is the executor-chain spawn path: native_resume here is the
    // SUBPROCESS/tool form declared via the `native_resume` decorate handler
    // (`spec.execution.native_resume`). A runtime-registry kind (graph) declares
    // native_resume on its runtime YAML and resumes through the managed
    // `spawn_runtime` path instead (it needs a LaunchEnvelope this path can't
    // build), so it never reaches here.
    if let Some(ts_dir) = thread_state_dir {
        for node in &plan.nodes {
            if let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = node
                && spec.execution.native_resume.is_some()
            {
                let thread_state = lillux::PinnedDirectory::open_or_create(ts_dir)
                    .with_context(|| format!("pin daemon thread state dir {}", ts_dir.display()))?;
                let checkpoint = thread_state
                    .open_or_create_child(
                        std::ffi::OsStr::new(crate::launch_metadata::CHECKPOINTS_SUBDIR),
                        0o700,
                    )
                    .with_context(|| {
                        format!("create pinned checkpoint dir below {}", ts_dir.display())
                    })?;
                let ckpt = checkpoint.path().to_path_buf();
                let mut bindings = vec![EnvBinding::new(
                    "RYEOS_CHECKPOINT_DIR",
                    ckpt.display().to_string(),
                    EnvSourceDetail::DaemonResume,
                )];
                if is_resume {
                    bindings.push(EnvBinding::new(
                        "RYEOS_RESUME",
                        "1",
                        EnvSourceDetail::DaemonResume,
                    ));
                }
                allocated_checkpoint_dir = Some(ckpt);
                allocated_checkpoint_authority = Some(Arc::new(checkpoint));
                resume_env_for_first_native_resume = Some(bindings);
                break; // first DispatchSubprocess wins, mirrors FirstWins
            }
        }
    }

    for node in &mut plan.nodes {
        if let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = node {
            let mut builder = crate::env_contract::EnvContractBuilder::new()
                .with_base_allowlist(std::env::vars_os().map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                }))?
                .with_daemon_roots(roots.clone())?
                .with_bindings(
                    EnvSourceKind::DeclaredSecret,
                    secret_map.iter().map(|(k, v)| (k.clone(), v.clone())),
                )?;

            builder = builder.with_typed_bindings([
                EnvBinding::new(
                    "RYEOS_THREAD_ID",
                    thread_id.to_string(),
                    EnvSourceDetail::EnginePlanEnv,
                ),
                EnvBinding::new(
                    "RYEOS_CHAIN_ROOT_ID",
                    chain_root_id.to_string(),
                    EnvSourceDetail::EnginePlanEnv,
                ),
            ])?;

            let runtime_bindings = spec.env.iter().map(|(key, value)| {
                let source = match spec.env_sources.get(key).copied() {
                    Some(RuntimeEnvSource::EnginePlan) => EnvSourceDetail::EnginePlanEnv,
                    Some(RuntimeEnvSource::RuntimeInterpreter) => {
                        EnvSourceDetail::RuntimeInterpreter
                    }
                    Some(RuntimeEnvSource::RuntimePathMutation) => {
                        EnvSourceDetail::RuntimePathMutation
                    }
                    Some(RuntimeEnvSource::RuntimeDescriptor) | None => {
                        EnvSourceDetail::RuntimeDescriptor
                    }
                };
                EnvBinding::new(key.clone(), value.clone(), source)
            });
            builder = builder.with_typed_bindings(runtime_bindings)?;

            builder = builder.with_typed_bindings(protocol_env_bindings.iter().cloned())?;

            // The sealed realization identity travels with the spawn: a
            // runtime (or any tool it hosts) references the admitted set from
            // here rather than re-observing content the contract forbids it
            // to re-verify live.
            if let Some(sealed) = &external_realizations_env {
                builder = builder.with_typed_bindings([EnvBinding::new(
                    "RYEOS_EXTERNAL_REALIZATIONS",
                    sealed.clone(),
                    EnvSourceDetail::PerSpawnDaemon,
                )])?;
            }
            if let Some(sealed) = &admitted_source_env {
                builder = builder.with_typed_bindings([EnvBinding::new(
                    "RYEOS_ADMITTED_SOURCE",
                    sealed.clone(),
                    EnvSourceDetail::PerSpawnDaemon,
                )])?;
            }

            if spec.execution.native_resume.is_some()
                && let Some(resume_bindings) = resume_env_for_first_native_resume.take()
            {
                builder = builder.with_typed_bindings(resume_bindings)?;
            }

            spec.env = builder.build().into_iter().collect();
            spec.env_sources.clear();
        }
    }

    let isolation_project_root = match &resolved.plan_context.project_context {
        ryeos_engine::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
        _ => None,
    };
    let isolation_resolution_roots = engine.resolution_roots(isolation_project_root);
    let isolation_bundle_roots = isolation_resolution_roots
        .ordered
        .iter()
        .filter(|root| root.space == ryeos_engine::contracts::ItemSpace::Bundle)
        .map(|root| {
            root.content_root.clone().ok_or_else(|| {
                anyhow!(
                    "isolation root `{}` has no registered content-root authority",
                    root.label
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let isolation_verified_code = plan
        .nodes
        .iter()
        .filter_map(|node| match node {
            ryeos_engine::contracts::PlanNode::DispatchSubprocess {
                tool_path: Some(source_path),
                ..
            } => Some(ryeos_engine::isolation::IsolationVerifiedCode {
                source_path: source_path.clone(),
                content_hash: resolved.resolved_item.content_hash.clone(),
            }),
            _ => None,
        })
        .collect();
    let engine_ctx = EngineContext {
        app_root,
        isolation,
        isolation_project_authority,
        isolation_filesystem_authority_ceiling:
            ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
        isolation_network_authority_ceiling:
            ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::NodePolicy,
        isolation_live_access_authority,
        isolation_state_root: state_root.map(std::path::Path::to_path_buf),
        isolation_checkpoint_dir: allocated_checkpoint_dir.clone(),
        isolation_checkpoint_authority: allocated_checkpoint_authority,
        isolation_daemon_socket_path: isolation_daemon_socket_path
            .map(std::path::Path::to_path_buf),
        isolation_bundle_roots,
        isolation_node_trusted_keys_dir: Some(isolation_node_trusted_keys_dir),
        isolation_verified_code,
        isolation_verified_command: prepared_plan.admitted_command,
        isolation_external_read_only_mounts,
        isolation_target_channel: None,
        isolation_workspace,
        subprocess_limits: None,
        inherited_fds,
        thread_id: thread_id.to_string(),
        chain_root_id: chain_root_id.to_string(),
        current_site_id: resolved.current_site_id.clone(),
        origin_site_id: resolved.origin_site_id.clone(),
        upstream_site_id: None,
        upstream_thread_id: None,
        continuation_from_id: None,
        requested_by: resolved.plan_context.requested_by.clone(),
        project_context: resolved.plan_context.project_context.clone(),
        launch_mode: if resolved.launch_mode == "detached" {
            LaunchMode::Detached
        } else {
            LaunchMode::Wait
        },
    };

    // Derive spawn-time launch metadata from the first DispatchSubprocess
    // node before handing the plan off to the engine. The engine remains
    // canonical for engine-known data (in `SubprocessSpec`); this snapshots
    // the daemon-relevant slice so shutdown/cancel can route without
    // re-loading the spec.
    let mut launch_metadata = plan
        .nodes
        .iter()
        .find_map(|n| match n {
            ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } => Some(
                crate::launch_metadata::RuntimeLaunchMetadata::from_spec(spec),
            ),
            _ => None,
        })
        .unwrap_or_default();
    if let Some(ckpt) = allocated_checkpoint_dir {
        launch_metadata = launch_metadata.with_checkpoint_dir(ckpt);
    }
    let spawned = engine
        .spawn_plan(&engine_ctx, &plan)
        .map_err(|e| anyhow!("spawn failed: {e}"))?;
    #[cfg(target_os = "linux")]
    let process_identity_result = crate::process::capture_execution_process_identity_from_pidfd(
        spawned.pid() as i64,
        Some(spawned.pgid()),
        spawned.pidfd(),
    )
    .context("capture held spawned target identity from Lillux pidfd");
    #[cfg(not(target_os = "linux"))]
    let process_identity_result = crate::process::capture_execution_process_identity(
        spawned.pid() as i64,
        Some(spawned.pgid()),
    )
    .context("capture held spawned target identity");
    let process_identity = match process_identity_result {
        Ok(identity) => identity,
        Err(error) => {
            let cleanup = spawned.abort_and_reap().err();
            return Err(match cleanup {
                Some(cleanup) => {
                    error.context(format!("pending-process cleanup failed: {cleanup}"))
                }
                None => error,
            });
        }
    };
    let durable_pid = spawned.pid();

    Ok(SpawnedItemAwaitingAttachment {
        pid: durable_pid,
        pgid: spawned.pgid(),
        process_identity,
        launch_metadata,
        spawned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn portable_direct_plan(project_root: &Path) -> ExecutionPlan {
        let mut plan: ExecutionPlan = serde_json::from_value(serde_json::json!({
            "plan_id": "plan:test",
            "root_executor_id": "tool:test/runtime",
            "root_ref": "tool:test/run",
            "item_kind": "tool",
            "nodes": [{
                "node_type": "dispatch_subprocess",
                "id": "spawn",
                "spec": {
                    "cmd": "/usr/bin/python3",
                    "verified_command": {
                        "authority": "captured_content",
                        "code": {
                            "source_path": "/usr/bin/python3",
                            "content_hash": "a".repeat(64)
                        }
                    },
                    "args": [{
                        "kind": "literal",
                        "value": project_root.join("tool.py")
                    }],
                    "cwd": project_root,
                    "env": {
                        "RYEOS_PROJECT_FILE": project_root.join("data.json")
                    },
                    "stdin": {
                        "kind": "runtime_parameters",
                        "parameters": {"message": "hello"},
                        "project_path": project_root
                    }
                },
                "executor_chain": ["tool:test/run", "tool:test/runtime"]
            }],
            "entrypoint": "spawn",
            "capabilities": {
                "requires_model": false,
                "requires_subprocess": true,
                "requires_network": false,
                "custom": []
            },
            "materialization_requirements": [],
            "cache_key": "test",
            "thread_kind": "tool",
            "executor_chain": ["tool:test/run", "tool:test/runtime"],
            "executor_authorities": [],
            "runtime_identity": null,
            "debug_raw": false
        }))
        .unwrap();
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { tool_path, .. } =
            &mut plan.nodes[0]
        else {
            unreachable!("portable direct-plan fixture must dispatch")
        };
        *tool_path = Some(project_root.join(".ai/tools/test/run.yaml"));
        plan
    }

    fn prepared_plan(plan: ExecutionPlan) -> PreparedItemPlan {
        PreparedItemPlan {
            timeout_secs: 300,
            plan,
            root_subject_source_identity: ryeos_state::objects::DirectRootSourceIdentity::Project,
            admitted_command: None,
        }
    }

    #[test]
    fn persistent_session_binds_one_typed_source_entry_and_protected_identity() {
        let mut plan = portable_direct_plan(Path::new("/ryeos/persistent-session-workspace"));
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = &mut plan.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        spec.args = vec![PlanArgument::AdmittedSourceEntry];
        let mut prepared = prepared_plan(plan);
        let entry = Path::new(
            "/ryeos/persistent-session-workspace/.ai/workers/local-inference/lib/local-tinygrad/bootstrap.py",
        );

        prepared
            .bind_persistent_session_spawn_environment(
                None,
                None,
                Some("{\"binding_hash\":\"fixture\"}"),
                Some(entry),
                None,
            )
            .unwrap();

        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } =
            &prepared.plan.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        assert_eq!(spec.args[0].literal_value(), Some(entry.to_str().unwrap()));
        assert_eq!(
            spec.env.get("RYEOS_ADMITTED_SOURCE").map(String::as_str),
            Some("{\"binding_hash\":\"fixture\"}")
        );
        assert_eq!(
            spec.env_sources.get("RYEOS_ADMITTED_SOURCE"),
            Some(&RuntimeEnvSource::EnginePlan)
        );
    }

    #[test]
    fn persistent_session_refuses_incoherent_or_authored_source_bindings() {
        let project_root = Path::new("/ryeos/persistent-session-workspace");
        let mut plan = portable_direct_plan(project_root);
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = &mut plan.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        spec.args = vec![PlanArgument::AdmittedSourceEntry];
        spec.env
            .insert("RYEOS_ADMITTED_SOURCE".to_owned(), "authored".to_owned());
        let mut prepared = prepared_plan(plan);
        assert!(
            prepared
                .bind_persistent_session_spawn_environment(
                    None,
                    None,
                    Some("sealed"),
                    Some(&project_root.join("bootstrap.py")),
                    None,
                )
                .unwrap_err()
                .to_string()
                .contains("override admitted source identity")
        );

        let mut plan = portable_direct_plan(project_root);
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = &mut plan.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        spec.args = vec![PlanArgument::AdmittedSourceEntry];
        let mut prepared = prepared_plan(plan);
        assert!(
            prepared
                .bind_persistent_session_spawn_environment(None, None, Some("sealed"), None, None,)
                .unwrap_err()
                .to_string()
                .contains("must be bound together")
        );
    }

    #[test]
    fn direct_recovery_binds_declared_executor_hop_not_terminal_executor() {
        let mut plan = portable_direct_plan(Path::new("/tmp/admitted-project"));
        plan.root_executor_id = "tool:test/subprocess-terminal".to_string();
        plan.executor_chain
            .push("tool:test/subprocess-terminal".to_string());

        validate_admitted_direct_executor_binding(&plan, "tool:test/runtime")
            .expect("the admitted executor is the first hop after the root");
        assert!(
            validate_admitted_direct_executor_binding(&plan, "tool:test/subprocess-terminal")
                .is_err()
        );
    }

    #[test]
    fn admitted_direct_plan_relocates_only_typed_project_paths() {
        let admitted = Path::new("/tmp/admitted-project");
        let effective = Path::new("/tmp/materialized-project");
        let mut plan = portable_direct_plan(admitted);

        relocate_admitted_direct_plan(&mut plan, Some(admitted), Some(effective)).unwrap();

        let ryeos_engine::contracts::PlanNode::DispatchSubprocess {
            spec, tool_path, ..
        } = &plan.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        assert_eq!(spec.cwd.as_deref(), Some(effective));
        assert_eq!(
            spec.args[0].literal_value(),
            Some(effective.join("tool.py").display().to_string().as_str())
        );
        assert_eq!(
            spec.env["RYEOS_PROJECT_FILE"],
            effective.join("data.json").display().to_string()
        );
        assert_eq!(
            tool_path.as_deref(),
            Some(effective.join(".ai/tools/test/run.yaml").as_path())
        );
        let Some(ryeos_engine::contracts::PlanStdin::RuntimeParameters { project_path, .. }) =
            &spec.stdin
        else {
            panic!("fixture must carry runtime parameters");
        };
        assert_eq!(project_path.as_deref(), Some(effective));
    }

    #[test]
    fn private_inputs_refuse_a_project_local_interpreter() {
        let project_root = Path::new("/tmp/live-project");
        let mut plan = portable_direct_plan(project_root);
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = &mut plan.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        spec.cmd = project_root
            .join(".venv/bin/python")
            .to_string_lossy()
            .into_owned();
        spec.verified_command = None;
        spec.env.insert("RYE_PYTHON".into(), spec.cmd.clone());
        spec.env_sources.insert(
            "RYE_PYTHON".into(),
            ryeos_engine::contracts::RuntimeEnvSource::RuntimeInterpreter,
        );

        let error = prepared_plan(plan)
            .ensure_no_project_local_interpreter(project_root)
            .expect_err("ambient project interpreter must not enter a private input root");
        assert!(
            error
                .to_string()
                .contains("selected interpreter comes from the live project")
        );
    }

    #[test]
    fn direct_admission_identity_is_stable_across_concrete_project_roots() {
        let first_root = Path::new("/var/lib/ryeos/executions/thread-a/project");
        let second_root = Path::new("/var/lib/ryeos/executions/thread-b/project");
        let mut first = prepared_plan(portable_direct_plan(first_root));
        let mut second = prepared_plan(portable_direct_plan(second_root));

        assert_eq!(
            first.bind_logical_project_root(Some(first_root)).unwrap(),
            Some(PathBuf::from(ADMITTED_DIRECT_PROJECT_ROOT))
        );
        assert_eq!(
            second.bind_logical_project_root(Some(second_root)).unwrap(),
            Some(PathBuf::from(ADMITTED_DIRECT_PROJECT_ROOT))
        );

        let first =
            lillux::canonical_json(&admitted_execution_plan_value(first.execution_plan()).unwrap())
                .unwrap();
        let second = lillux::canonical_json(
            &admitted_execution_plan_value(second.execution_plan()).unwrap(),
        )
        .unwrap();
        assert_eq!(first, second);
        assert!(first.contains(ADMITTED_DIRECT_PROJECT_ROOT));
        assert!(!first.contains(first_root.to_str().unwrap()));
        assert!(!first.contains(second_root.to_str().unwrap()));
    }

    #[test]
    fn persistent_session_plan_identity_is_stable_across_runtime_roots() {
        let admitted = Path::new("/ryeos/persistent-session-workspace");
        let plan = portable_direct_plan(admitted);
        let admitted_bytes = lillux::canonical_json(&serde_json::to_value(&plan).unwrap()).unwrap();
        let admitted_hash = lillux::sha256_hex(admitted_bytes.as_bytes());

        let mut first = plan.clone();
        let mut second = plan;
        relocate_admitted_direct_plan(
            &mut first,
            Some(admitted),
            Some(Path::new("/var/lib/ryeos-a/session")),
        )
        .unwrap();
        relocate_admitted_direct_plan(
            &mut second,
            Some(admitted),
            Some(Path::new("/srv/ryeos-b/session")),
        )
        .unwrap();

        let cwd = |plan: &ExecutionPlan| match &plan.nodes[0] {
            ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } => {
                spec.cwd.clone().unwrap()
            }
            _ => panic!("fixture must dispatch"),
        };
        assert_eq!(cwd(&first), PathBuf::from("/var/lib/ryeos-a/session"));
        assert_eq!(cwd(&second), PathBuf::from("/srv/ryeos-b/session"));
        assert_ne!(cwd(&first), cwd(&second));

        // Runtime relocation is applied to mutable spawn copies. The retained
        // plan/capsule identity remains the single logical-root hash.
        assert_eq!(
            lillux::sha256_hex(
                lillux::canonical_json(
                    &serde_json::to_value(&portable_direct_plan(admitted)).unwrap()
                )
                .unwrap()
                .as_bytes()
            ),
            admitted_hash
        );
    }

    #[test]
    fn audit_tool_path_does_not_fragment_admitted_plan_identity() {
        let admitted = Path::new("/ryeos/persistent-session-workspace");
        let mut first = portable_direct_plan(admitted);
        let mut second = first.clone();
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess {
            tool_path: first_path,
            ..
        } = &mut first.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess {
            tool_path: second_path,
            ..
        } = &mut second.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        *first_path = Some(PathBuf::from("/opt/first/.ai/workers/example.yaml"));
        *second_path = Some(PathBuf::from("/srv/second/.ai/workers/example.yaml"));

        for (plan, root) in [(&mut first, "/opt/first"), (&mut second, "/srv/second")] {
            let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } =
                &mut plan.nodes[0]
            else {
                panic!("fixture must dispatch");
            };
            let command = PathBuf::from(root).join("bin/ryeos-core-tools");
            spec.cmd = command.display().to_string();
            match spec.verified_command.as_mut().unwrap() {
                ryeos_engine::contracts::PlanVerifiedCommand::BundleExecutor { code, .. }
                | ryeos_engine::contracts::PlanVerifiedCommand::CapturedContent { code } => {
                    code.source_path = command;
                }
            }
        }

        let first =
            lillux::canonical_json(&admitted_execution_plan_value(&first).unwrap()).unwrap();
        let second =
            lillux::canonical_json(&admitted_execution_plan_value(&second).unwrap()).unwrap();
        assert_eq!(first, second);
        assert!(!first.contains("/opt/first"));
        assert!(!first.contains("/srv/second"));
    }

    #[test]
    fn admitted_direct_plan_rejects_embedded_untyped_project_paths() {
        let admitted = Path::new("/tmp/admitted-project");
        let mut plan = portable_direct_plan(admitted);
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = &mut plan.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        spec.args[0] = format!("--script={}/tool.py", admitted.display()).into();

        let error = validate_direct_plan_portability(&plan, Some(admitted)).unwrap_err();
        assert!(
            error.to_string().contains("untyped string"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn admitted_direct_plan_rejects_project_root_outside_runtime_binding() {
        let admitted = Path::new("/tmp/admitted-project");
        let mut plan = portable_direct_plan(admitted);
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = &mut plan.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        let Some(ryeos_engine::contracts::PlanStdin::RuntimeParameters { parameters, .. }) =
            &mut spec.stdin
        else {
            panic!("fixture must carry runtime parameters");
        };
        parameters["payload"] =
            serde_json::Value::String(format!("read {}/data.json", admitted.display()));

        let error = validate_direct_plan_portability(&plan, Some(admitted)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the project_path binding"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn admitted_direct_plan_rejects_project_root_in_opaque_stdin() {
        let admitted = Path::new("/tmp/admitted-project");
        let mut plan = portable_direct_plan(admitted);
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = &mut plan.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        spec.stdin = Some(ryeos_engine::contracts::PlanStdin::Opaque {
            data: format!("read {}/data.json", admitted.display()),
        });

        let error = validate_direct_plan_portability(&plan, Some(admitted)).unwrap_err();
        assert!(
            error.to_string().contains("opaque stdin"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn admitted_direct_plan_rejects_parent_components_after_project_prefix() {
        let admitted = Path::new("/tmp/admitted-project");
        let effective = Path::new("/tmp/materialized-project");
        let mut plan = portable_direct_plan(admitted);
        let ryeos_engine::contracts::PlanNode::DispatchSubprocess {
            spec, tool_path, ..
        } = &mut plan.nodes[0]
        else {
            panic!("fixture must dispatch");
        };
        spec.args[0] = format!("{}/../outside", admitted.display()).into();
        *tool_path = Some(admitted.join("../outside.yaml"));

        let error =
            relocate_admitted_direct_plan(&mut plan, Some(admitted), Some(effective)).unwrap_err();
        assert!(
            error.to_string().contains("escapes its project root"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn admitted_direct_plan_rejects_filesystem_root_relocation() {
        let mut plan = portable_direct_plan(Path::new("/"));
        let error = relocate_admitted_direct_plan(
            &mut plan,
            Some(Path::new("/")),
            Some(Path::new("/tmp/materialized-project")),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("absolute and normalized"),
            "unexpected error: {error:#}"
        );
    }
}
