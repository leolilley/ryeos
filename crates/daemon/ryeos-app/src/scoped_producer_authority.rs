//! Process-local authority for a scoped producer admitted by a live root.
//!
//! Durable records identify the launch, but cannot recreate open executable
//! descriptors, the pinned private workspace, or executor-owned lifelines.
//! Losing this registry on daemon restart therefore fails closed. It is not a
//! cache of paths and never carries callback, vault, or provider credentials.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail, ensure};
use ryeos_engine::isolation::{
    IsolationAdmittedCommand, IsolationCommandAuthority, IsolationDescriptorBoundCommand,
    IsolationReadOnlyMountAuthority,
};
use ryeos_state::external_content::products::producer_recipe::{
    ProducerEnvironmentSource, ProducerExecutableSource, ProducerStdinSource, ProductProducerRecipe,
};
use ryeos_state::external_content::products::qualification::ProductProducerRecipeSourceIdentity;

use crate::runtime_db::LaunchOwner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedProducerAuthorityKey {
    pub root_thread_id: String,
    pub launch_owner: LaunchOwner,
}

impl ScopedProducerAuthorityKey {
    pub fn new(root_thread_id: String, launch_owner: LaunchOwner) -> Result<Self> {
        ryeos_runtime::validate_runtime_thread_id(&root_thread_id).map_err(anyhow::Error::msg)?;
        ensure!(
            root_thread_id == launch_owner.thread_id,
            "scoped producer root and launch owner differ"
        );
        ensure!(
            launch_owner.monotonic_launch_epoch > 0
                && !launch_owner.unpredictable_nonce.is_empty()
                && launch_owner.daemon_generation_id == crate::runtime_db::daemon_generation_id(),
            "scoped producer launch owner is not live in this daemon generation"
        );
        Ok(Self {
            root_thread_id,
            launch_owner,
        })
    }
}

/// Identity for the one signed scenario selected by a live verifier root.
/// This is not a launch grant: the service must still consume the live
/// registry authority and durably reserve the attempt before Lillux contact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedProducerAttemptCoordinate {
    attempt_id: String,
    scope_allocation_name: String,
    recipe_digest: String,
    recipe_generation: String,
    scenario_digest: String,
}

impl ScopedProducerAttemptCoordinate {
    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    pub fn scenario_digest(&self) -> &str {
        &self.scenario_digest
    }

    pub fn scope_allocation_name(&self) -> &str {
        &self.scope_allocation_name
    }

    pub fn derive(
        key: &ScopedProducerAuthorityKey,
        scenario_id: &str,
        source: &ProductProducerRecipeSourceIdentity,
        admitted_stdin: &str,
    ) -> Result<Self> {
        ScopedProducerAuthorityKey::new(key.root_thread_id.clone(), key.launch_owner.clone())?;
        source.validate()?;
        ensure!(
            !scenario_id.is_empty()
                && scenario_id.len() <= 128
                && scenario_id.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                }),
            "scoped producer scenario selector is not bounded and canonical"
        );
        ensure!(
            !admitted_stdin.is_empty() && admitted_stdin.len() <= 128 * 1024,
            "scoped producer admitted input is empty or unbounded"
        );
        let scenario = serde_json::json!({
            "schema": "ryeos.scoped_producer_attempt_coordinate.v1",
            "root_thread_id": key.root_thread_id,
            "launch_owner": key.launch_owner,
            "scenario_id": scenario_id,
            "recipe_source": source,
            "admitted_stdin_sha256": lillux::sha256_hex(admitted_stdin.as_bytes()),
        });
        let scenario_digest = lillux::sha256_hex(lillux::canonical_json(&scenario)?.as_bytes());
        Ok(Self {
            attempt_id: format!("scoped-{scenario_digest}"),
            scope_allocation_name: format!("scoped-{}", &scenario_digest[..32]),
            recipe_digest: source.recipe_digest.clone(),
            recipe_generation: source.bundle_generation_identity.clone(),
            scenario_digest,
        })
    }

    /// Carry the single derived coordinate into the durable reservation. The
    /// allocation must be planned by the admitted isolation provider before
    /// this value can be committed; no callback field enters the journal.
    pub fn journal_attempt(
        &self,
        key: &ScopedProducerAuthorityKey,
        scope_allocation: lillux::ProcessScopeAllocation,
    ) -> Result<crate::runtime_db::scoped_child_attempt::NewScopedChildAttempt> {
        ScopedProducerAuthorityKey::new(key.root_thread_id.clone(), key.launch_owner.clone())?;
        scope_allocation.validate().map_err(anyhow::Error::msg)?;
        Ok(
            crate::runtime_db::scoped_child_attempt::NewScopedChildAttempt {
                attempt_id: self.attempt_id.clone(),
                owner: key.launch_owner.clone(),
                recipe_digest: self.recipe_digest.clone(),
                recipe_generation: self.recipe_generation.clone(),
                scenario_digest: self.scenario_digest.clone(),
                scope_allocation,
            },
        )
    }
}

/// Exact live launch materials. The opaque lifeline retains executor-owned
/// mount and workspace leases without exposing their implementation to app.
/// Its destruction cannot itself prove process death; the journal and scope
/// owner must settle the launched child separately.
pub struct ScopedProducerLiveAuthority {
    command: IsolationDescriptorBoundCommand,
    scenario_commands: BTreeMap<
        String,
        (
            ProductProducerRecipeSourceIdentity,
            IsolationAdmittedCommand,
        ),
    >,
    workspace: lillux::PinnedDirectory,
    external_realizations_env: Option<String>,
    read_only_mounts: Vec<IsolationReadOnlyMountAuthority>,
    /// Registered channel prebound to this verifier root's exact process.
    /// The daemon may consume it only for the one signed ingress attempt;
    /// neither a journal row nor a callback can recreate a live endpoint.
    ingress_handoff: Mutex<Option<lillux::InheritedDuplexChannel>>,
    _lifelines: Arc<dyn Send + Sync>,
}

impl ScopedProducerLiveAuthority {
    pub fn new(
        command: IsolationDescriptorBoundCommand,
        workspace: lillux::PinnedDirectory,
        external_realizations_env: Option<String>,
        read_only_mounts: Vec<IsolationReadOnlyMountAuthority>,
        ingress_handoff: Option<lillux::InheritedDuplexChannel>,
        lifelines: Arc<dyn Send + Sync>,
    ) -> Result<Self> {
        workspace.identity()?;
        if let Some(env) = &external_realizations_env {
            ensure!(
                !env.is_empty() && env.len() <= 128 * 1024,
                "scoped producer realization identity is empty or unbounded"
            );
            let value: serde_json::Value = serde_json::from_str(env)?;
            ryeos_state::objects::ExternalContentRealizationSet::from_value(&value)?;
        }
        Ok(Self {
            command,
            scenario_commands: BTreeMap::new(),
            workspace,
            external_realizations_env,
            read_only_mounts,
            ingress_handoff: Mutex::new(ingress_handoff),
            _lifelines: lifelines,
        })
    }

    /// Consume the one live descriptor endpoint bound into this root launch.
    /// This returns no endpoint on restart, repeat callback, or a root that
    /// never selected a signed isolated-ingress scenario.
    pub fn take_ingress_handoff(&self) -> Result<lillux::InheritedDuplexChannel> {
        self.ingress_handoff
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped producer ingress handoff lock poisoned"))?
            .take()
            .ok_or_else(|| anyhow::anyhow!("scoped producer ingress handoff is not live"))
    }

    pub fn command(&self) -> &IsolationDescriptorBoundCommand {
        &self.command
    }

    /// Retain executor-promoted realization members with the exact signed
    /// recipe source selected at root admission. A callback cannot add or
    /// substitute an executable after this authority is registered.
    pub fn with_scenario_commands(
        mut self,
        commands: BTreeMap<
            String,
            (
                ProductProducerRecipeSourceIdentity,
                IsolationAdmittedCommand,
            ),
        >,
    ) -> Result<Self> {
        for (scenario, (source, command)) in &commands {
            ensure!(
                !scenario.is_empty() && scenario.len() <= 128,
                "scoped producer scenario command has invalid name"
            );
            source.validate()?;
            ensure!(
                matches!(command, IsolationAdmittedCommand::RealizationMember(_)),
                "scoped producer scenario command is not a realization member"
            );
        }
        self.scenario_commands = commands;
        Ok(self)
    }

    pub fn admitted_command_for_recipe(
        &self,
        scenario: &str,
        source: &ProductProducerRecipeSourceIdentity,
        executable: &ProducerExecutableSource,
    ) -> Result<IsolationAdmittedCommand> {
        match executable {
            ProducerExecutableSource::AdmittedVerifierExecutable => Ok(
                IsolationAdmittedCommand::DescriptorBound(self.command.clone()),
            ),
            ProducerExecutableSource::AdmittedRealizationMember {
                executable_sha256, ..
            } => {
                let (admitted_source, command) =
                    self.scenario_commands.get(scenario).ok_or_else(|| {
                        anyhow::anyhow!("signed producer scenario has no promoted command")
                    })?;
                ensure!(
                    admitted_source == source,
                    "promoted producer command differs from current signed recipe source"
                );
                ensure!(
                    command.authority().identity().content_hash == *executable_sha256,
                    "promoted producer command differs from signed executable hash"
                );
                Ok(command.clone())
            }
        }
    }

    pub fn workspace(&self) -> &lillux::PinnedDirectory {
        &self.workspace
    }

    /// Borrow an inheritable duplicate of the exact private root retained by
    /// this live launch. The scoped-child isolation context must carry this
    /// view; its projectless scratch path alone is not owner authority.
    pub fn workspace_view(&self) -> Result<lillux::InheritedDescriptorAuthority> {
        self.workspace.ensure_path_binding()?;
        Ok(self.workspace.inherited_descriptor_authority()?)
    }

    pub fn external_realizations_env(&self) -> Option<&str> {
        self.external_realizations_env.as_deref()
    }

    /// Borrow the exact already-bound source and external-content mounts.
    /// The producer must not reconstruct these from a live project tree.
    pub fn read_only_mounts(&self) -> &[IsolationReadOnlyMountAuthority] {
        &self.read_only_mounts
    }

    /// Compile only the signed producer's process arguments. Isolation must
    /// still bind `self.command()` and `self.workspace_view()` to this request,
    /// while Lillux must install the recipe's scope-wide memory/process limits
    /// before any held spawn. This value alone is not executable authority.
    pub fn request_for_recipe(
        &self,
        recipe: &ProductProducerRecipe,
        admitted_stdin: &str,
        selected_command: &IsolationAdmittedCommand,
    ) -> Result<lillux::SubprocessRequest> {
        recipe.validate()?;
        let buffered_input = match (&recipe.executable_source, &recipe.stdin_source) {
            (
                ProducerExecutableSource::AdmittedVerifierExecutable,
                ProducerStdinSource::SignedVerifierParameters,
            ) => {
                ensure!(
                    matches!(selected_command, IsolationAdmittedCommand::DescriptorBound(_))
                        && selected_command.authority().identity() == self.command.identity(),
                    "scoped producer selected command differs from admitted verifier"
                );
                true
            }
            (
                ProducerExecutableSource::AdmittedRealizationMember {
                    executable_sha256, ..
                },
                ProducerStdinSource::InteractiveVerifierChannel { .. },
            ) => {
                ensure!(
                    matches!(selected_command, IsolationAdmittedCommand::RealizationMember(_))
                        && selected_command.authority().identity().content_hash
                            == *executable_sha256,
                    "scoped producer selected command differs from signed realization member"
                );
                false
            }
            _ => anyhow::bail!("scoped producer executable and input modes disagree"),
        };
        self.workspace.ensure_path_binding()?;
        ensure!(
            !admitted_stdin.is_empty() && admitted_stdin.len() <= 128 * 1024,
            "scoped producer admitted input is empty or unbounded"
        );
        let mut envs = Vec::new();
        for source in &recipe.environment_sources {
            match source {
                ProducerEnvironmentSource::AdmittedRealizations => {
                    let value = self.external_realizations_env.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("signed producer requires absent admitted realizations")
                    })?;
                    envs.push(("RYEOS_EXTERNAL_REALIZATIONS".to_owned(), value.clone()));
                }
            }
        }
        let cmd = selected_command
            .authority()
            .identity()
            .source_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("scoped producer command path is not UTF-8"))?
            .to_owned();
        let cwd = self
            .workspace
            .descriptor_path()?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("scoped producer pinned cwd is not UTF-8"))?
            .to_owned();
        Ok(lillux::SubprocessRequest {
            cmd,
            argv0: None,
            args: recipe.argv.clone(),
            cwd: Some(cwd),
            envs,
            stdin_data: buffered_input.then(|| admitted_stdin.to_owned()),
            timeout: recipe.bounds.maximum_wall_time_ms as f64 / 1000.0,
            limits: Some(lillux::SubprocessLimits {
                max_stdout_bytes: Some(recipe.bounds.maximum_stdout_bytes),
                max_stderr_bytes: Some(recipe.bounds.maximum_stderr_bytes),
                ..Default::default()
            }),
            inherited_fds: Vec::new(),
            inherited_fd_mappings: Vec::new(),
            supervised_status: None,
        })
    }
}

struct Registration {
    key: ScopedProducerAuthorityKey,
    serial: u64,
    authority: Arc<ScopedProducerLiveAuthority>,
}

#[derive(Default)]
struct RegistryState {
    next_serial: u64,
    entries: Vec<Registration>,
    spent: Vec<ScopedProducerAuthorityKey>,
}

#[derive(Default)]
pub struct ScopedProducerAuthorityRegistry {
    state: Mutex<RegistryState>,
}

impl ScopedProducerAuthorityRegistry {
    pub fn register(
        self: &Arc<Self>,
        key: ScopedProducerAuthorityKey,
        authority: ScopedProducerLiveAuthority,
    ) -> Result<ScopedProducerAuthorityRegistration> {
        // Recheck at insertion, not only when the key was constructed.
        let key = ScopedProducerAuthorityKey::new(key.root_thread_id, key.launch_owner)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped producer authority registry poisoned"))?;
        if state.entries.iter().any(|entry| entry.key == key) || state.spent.contains(&key) {
            bail!("scoped producer launch authority is already registered");
        }
        state.next_serial = state
            .next_serial
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("scoped producer registration serial exhausted"))?;
        let serial = state.next_serial;
        state.entries.push(Registration {
            key: key.clone(),
            serial,
            authority: Arc::new(authority),
        });
        Ok(ScopedProducerAuthorityRegistration {
            registry: Arc::clone(self),
            key,
            serial,
        })
    }

    /// A lookup is live only for the exact root and launch attempt in this
    /// process. Callers must recheck immediately before an irreversible cut;
    /// a retained Arc is not a durable or transferable launch grant.
    pub fn get_exact(
        &self,
        key: &ScopedProducerAuthorityKey,
    ) -> Result<Arc<ScopedProducerLiveAuthority>> {
        ScopedProducerAuthorityKey::new(key.root_thread_id.clone(), key.launch_owner.clone())?;
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped producer authority registry poisoned"))?;
        let authority = state
            .entries
            .iter()
            .find(|entry| &entry.key == key)
            .map(|entry| Arc::clone(&entry.authority))
            .ok_or_else(|| anyhow::anyhow!("scoped producer launch authority is not live"))?;
        authority.workspace.ensure_path_binding()?;
        Ok(authority)
    }

    /// The irreversible local authorization cut for one child attempt. It
    /// atomically transfers the pinned materials out of root registration and
    /// burns this launch owner's one-shot key. The returned Arc is a child
    /// lifeline, not a replayable permission; the caller must still reserve
    /// the exact durable attempt before Lillux allocation or process contact.
    pub fn consume_for_attempt(
        &self,
        key: &ScopedProducerAuthorityKey,
    ) -> Result<Arc<ScopedProducerLiveAuthority>> {
        ScopedProducerAuthorityKey::new(key.root_thread_id.clone(), key.launch_owner.clone())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("scoped producer authority registry poisoned"))?;
        let index = state
            .entries
            .iter()
            .position(|entry| &entry.key == key)
            .ok_or_else(|| {
                anyhow::anyhow!("scoped producer authority was already consumed or revoked")
            })?;
        state.entries[index]
            .authority
            .workspace
            .ensure_path_binding()?;
        let entry = state.entries.remove(index);
        state.spent.push(key.clone());
        Ok(entry.authority)
    }

    pub fn ensure_live(
        &self,
        key: &ScopedProducerAuthorityKey,
        authority: &Arc<ScopedProducerLiveAuthority>,
    ) -> Result<()> {
        let current = self.get_exact(key)?;
        ensure!(
            Arc::ptr_eq(&current, authority),
            "scoped producer launch authority was replaced"
        );
        Ok(())
    }

    fn unregister(&self, key: &ScopedProducerAuthorityKey, serial: u64) {
        // Poisoning fails closed: no later lookup can return authority.
        if let Ok(mut state) = self.state.lock() {
            state
                .entries
                .retain(|entry| entry.key != *key || entry.serial != serial);
            // The root's registration lifetime has ended. Durable attempt
            // uniqueness remains in SQLite; keep this process-local set
            // bounded by active roots rather than every historical launch.
            state.spent.retain(|spent| spent != key);
        }
    }
}

/// The executor retains this guard for the full live root lifetime. An old
/// guard can never unregister a newer registration of the same coordinate.
pub struct ScopedProducerAuthorityRegistration {
    registry: Arc<ScopedProducerAuthorityRegistry>,
    key: ScopedProducerAuthorityKey,
    serial: u64,
}

impl Drop for ScopedProducerAuthorityRegistration {
    fn drop(&mut self) {
        self.registry.unregister(&self.key, self.serial);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DropCount(Arc<AtomicUsize>);

    #[test]
    #[ignore = "requires native local socket authority"]
    fn verifier_root_handoff_is_consumed_once() {
        let authority = authority();
        let (parent, _verifier) = lillux::inherited_duplex_channel_pair().unwrap();
        *authority.ingress_handoff.lock().unwrap() = Some(parent);
        assert!(authority.take_ingress_handoff().is_ok());
        assert!(authority.take_ingress_handoff().is_err());
    }

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn key() -> ScopedProducerAuthorityKey {
        let thread_id = "T-scoped-producer-test".to_owned();
        ScopedProducerAuthorityKey::new(
            thread_id.clone(),
            LaunchOwner {
                thread_id,
                monotonic_launch_epoch: 1,
                unpredictable_nonce: "test-nonce".to_owned(),
                daemon_generation_id: crate::runtime_db::daemon_generation_id().to_owned(),
            },
        )
        .unwrap()
    }

    fn authority() -> ScopedProducerLiveAuthority {
        let root = tempfile::tempdir().unwrap();
        let workspace = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let executable = lillux::sealed_memfd(c"scoped-producer-test", b"fixture").unwrap();
        let command = IsolationDescriptorBoundCommand::new(
            ryeos_engine::isolation::IsolationVerifiedCode {
                source_path: "/fixture".into(),
                content_hash: lillux::sha256_hex(b"fixture"),
            },
            executable,
            ryeos_engine::isolation::IsolationDescriptorFileIdentity {
                device: 1,
                inode: 1,
                size: 7,
                modified_seconds: 0,
                modified_nanoseconds: 0,
                changed_seconds: 0,
                changed_nanoseconds: 0,
                mode: 0,
                file_type: 0,
            },
        );
        ScopedProducerLiveAuthority::new(command, workspace, None, Vec::new(), None, Arc::new(root))
            .unwrap()
    }

    fn source() -> ProductProducerRecipeSourceIdentity {
        ProductProducerRecipeSourceIdentity {
            bundle_generation_identity: "generation-1".into(),
            canonical_ref: "config:fixtures/scoped-producer".into(),
            raw_content_digest: "a".repeat(64),
            effective_definition_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
            recipe_digest: "d".repeat(64),
        }
    }

    #[test]
    fn recipe_request_has_only_signed_arguments_input_and_environment() {
        let authority = authority();
        let recipe = ProductProducerRecipe::from_value(serde_json::json!({
            "schema": "ryeos.product_producer_recipe.v3",
            "executable_source": {"kind":"admitted_verifier_executable"},
            "argv": ["--scenario-driver"],
            "stdin_source": {"kind":"signed_verifier_parameters"},
            "cwd_source": "verifier_private_workspace",
            "environment_sources": [],
            "loopback_ingress": null,
            "bounds": {
                "maximum_wall_time_ms": 5000,
                "maximum_stdout_bytes": 1024,
                "maximum_stderr_bytes": 2048,
                "maximum_memory_bytes": 1048576,
                "maximum_processes": 2
            }
        }))
        .unwrap();
        let selected_command = authority
            .admitted_command_for_recipe("native_codex", &source(), &recipe.executable_source)
            .unwrap();
        let request = authority
            .request_for_recipe(&recipe, "{\"sealed\":true}", &selected_command)
            .unwrap();
        assert_eq!(request.args, vec!["--scenario-driver"]);
        assert_eq!(request.stdin_data.as_deref(), Some("{\"sealed\":true}"));
        assert_eq!(
            request.cwd.as_deref(),
            authority.workspace().descriptor_path().unwrap().to_str()
        );
        assert!(request.envs.is_empty());
        assert_eq!(request.timeout, 5.0);
        assert_eq!(request.limits.unwrap().max_stdout_bytes, Some(1024));
        let unrelated =
            IsolationAdmittedCommand::DescriptorBound(IsolationDescriptorBoundCommand::new(
                ryeos_engine::isolation::IsolationVerifiedCode {
                    source_path: "/unrelated".into(),
                    content_hash: "e".repeat(64),
                },
                lillux::sealed_memfd(c"scoped-producer-unrelated", b"different").unwrap(),
                authority.command().file_identity(),
            ));
        assert!(
            authority
                .request_for_recipe(&recipe, "{\"sealed\":true}", &unrelated)
                .is_err()
        );
        let mut requires_realizations = recipe;
        requires_realizations.environment_sources =
            vec![ProducerEnvironmentSource::AdmittedRealizations];
        assert!(
            authority
                .request_for_recipe(
                    &requires_realizations,
                    "{\"sealed\":true}",
                    &selected_command,
                )
                .is_err()
        );
    }

    #[test]
    fn interactive_realization_recipe_refuses_a_verifier_command() {
        let authority = authority();
        let recipe = ProductProducerRecipe::from_value(serde_json::json!({
            "schema": "ryeos.product_producer_recipe.v3",
            "executable_source": {
                "kind": "admitted_realization_member",
                "realization_id": "codex_runtime",
                "manifest_hash": "a".repeat(64),
                "relative_path": "bin/codex",
                "executable_sha256": "b".repeat(64)
            },
            "argv": ["app-server"],
            "stdin_source": {
                "kind": "interactive_verifier_channel",
                "maximum_frame_bytes": 1024,
                "maximum_total_bytes": 4096,
                "maximum_frames": 4
            },
            "cwd_source": "verifier_private_workspace",
            "environment_sources": [],
            "loopback_ingress": null,
            "bounds": {
                "maximum_wall_time_ms": 5000,
                "maximum_stdout_bytes": 1024,
                "maximum_stderr_bytes": 2048,
                "maximum_memory_bytes": 1048576,
                "maximum_processes": 2
            }
        }))
        .unwrap();
        assert!(
            authority
                .admitted_command_for_recipe("native_codex", &source(), &recipe.executable_source)
                .is_err()
        );
        assert!(
            authority
                .request_for_recipe(
                    &recipe,
                    "{\"sealed\":true}",
                    &IsolationAdmittedCommand::DescriptorBound(authority.command().clone()),
                )
                .err()
                .unwrap()
                .to_string()
                .contains("selected command differs from signed realization member")
        );
    }

    #[test]
    fn attempt_coordinate_binds_owner_signed_source_and_sealed_input() {
        let key = key();
        let source = source();
        let first = ScopedProducerAttemptCoordinate::derive(
            &key,
            "native_codex",
            &source,
            "{\"request\":1}",
        )
        .unwrap();
        let allocation: lillux::ProcessScopeAllocation =
            serde_json::from_value(serde_json::json!({
                "version": 2,
                "control_timeout": {"secs": 1, "nanos": 0},
                "configuration": {"version": 3, "backend": {
                    "implementation": "linux_cgroup_v2", "parent": "/fixture/delegation"
                }},
                "backend": {"implementation": "linux_cgroup_v2",
                    "boot_id": "00000000-0000-4000-8000-000000000000",
                    "parent": {"containing_device": 1, "inode": 2},
                    "name": "scoped-fixture"}
            }))
            .unwrap();
        let journal = first.journal_attempt(&key, allocation).unwrap();
        assert_eq!(journal.attempt_id, first.attempt_id);
        assert_eq!(journal.owner, key.launch_owner);
        assert_eq!(journal.recipe_digest, source.recipe_digest);
        assert_eq!(journal.scenario_digest, first.scenario_digest);
        assert_eq!(
            first,
            ScopedProducerAttemptCoordinate::derive(
                &key,
                "native_codex",
                &source,
                "{\"request\":1}",
            )
            .unwrap()
        );
        assert_ne!(
            first,
            ScopedProducerAttemptCoordinate::derive(
                &key,
                "native_codex",
                &source,
                "{\"request\":2}",
            )
            .unwrap()
        );
        let mut changed_source = source.clone();
        changed_source.recipe_digest = "e".repeat(64);
        assert_ne!(
            first,
            ScopedProducerAttemptCoordinate::derive(
                &key,
                "native_codex",
                &changed_source,
                "{\"request\":1}",
            )
            .unwrap()
        );
        assert!(
            ScopedProducerAttemptCoordinate::derive(&key, "../other", &source, "{\"request\":1}",)
                .is_err()
        );
    }

    #[test]
    fn registration_is_exact_and_drop_revokes_lookup() {
        let registry = Arc::new(ScopedProducerAuthorityRegistry::default());
        let key = key();
        let guard = registry.register(key.clone(), authority()).unwrap();
        let live = registry.get_exact(&key).unwrap();
        let view = live.workspace_view().unwrap();
        let retained = view
            .try_clone_pinned_directory(live.workspace().path().to_path_buf())
            .unwrap();
        assert!(live.workspace().is_same_directory(&retained).unwrap());
        registry.ensure_live(&key, &live).unwrap();
        assert!(registry.register(key.clone(), authority()).is_err());
        let mut wrong = key.clone();
        wrong.launch_owner.unpredictable_nonce = "other".to_owned();
        assert!(registry.get_exact(&wrong).is_err());
        drop(guard);
        assert!(registry.get_exact(&key).is_err());
        assert!(registry.ensure_live(&key, &live).is_err());
        let newer = registry.register(key.clone(), authority()).unwrap();
        assert!(registry.ensure_live(&key, &live).is_err());
        drop(newer);
    }

    #[test]
    fn wrong_root_and_old_generation_are_refused() {
        let valid = key();
        assert!(
            ScopedProducerAuthorityKey::new("T-other".into(), valid.launch_owner.clone()).is_err()
        );
        let mut old = valid.launch_owner;
        old.daemon_generation_id = "daemon-old".to_owned();
        assert!(ScopedProducerAuthorityKey::new(old.thread_id.clone(), old).is_err());
    }

    #[test]
    fn consumed_authority_retains_lifeline_but_cannot_launch_twice() {
        let registry = Arc::new(ScopedProducerAuthorityRegistry::default());
        let key = key();
        let guard = registry.register(key.clone(), authority()).unwrap();
        let child_lifeline = registry.consume_for_attempt(&key).unwrap();
        assert!(registry.get_exact(&key).is_err());
        assert!(registry.consume_for_attempt(&key).is_err());
        assert!(registry.register(key.clone(), authority()).is_err());
        drop(guard);
        child_lifeline.workspace().ensure_path_binding().unwrap();
    }

    #[test]
    fn replaced_workspace_path_refuses_lookup_and_attempt_transfer() {
        let registry = Arc::new(ScopedProducerAuthorityRegistry::default());
        let key = key();
        let authority = authority();
        let original = authority.workspace().path().to_path_buf();
        let moved = original.with_extension("retained");
        let _guard = registry.register(key.clone(), authority).unwrap();
        let live = registry.get_exact(&key).unwrap();
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        assert!(registry.get_exact(&key).is_err());
        assert!(registry.consume_for_attempt(&key).is_err());
        assert!(live.workspace_view().is_err());
        std::fs::remove_dir(&moved).unwrap();
    }

    #[test]
    fn concurrent_attempt_transfer_has_one_winner_and_retains_lifeline() {
        let registry = Arc::new(ScopedProducerAuthorityRegistry::default());
        let key = key();
        let drops = Arc::new(AtomicUsize::new(0));
        let mut authority = authority();
        let original_lifeline = std::mem::replace(&mut authority._lifelines, Arc::new(()));
        authority._lifelines = Arc::new((original_lifeline, DropCount(Arc::clone(&drops))));
        let guard = registry.register(key.clone(), authority).unwrap();
        let winners = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    let registry = Arc::clone(&registry);
                    let key = key.clone();
                    scope.spawn(move || registry.consume_for_attempt(&key).ok())
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .filter_map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(winners.len(), 1);
        drop(guard);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(winners);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
