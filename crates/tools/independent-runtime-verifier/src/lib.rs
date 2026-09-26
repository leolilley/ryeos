//! Exact selected-input preflight for independent runtime qualification.
//!
//! No product claim follows from these checks alone. The installed verifier
//! still needs to own the native guest and real app-server observations.

pub mod app_server;
pub mod guest_observation;
pub mod native_guest;
pub mod routing_observation;
pub mod scoped_app_server;
pub mod scoped_relay;
pub mod scripted_peer;
pub mod scripted_provider;
pub mod scripted_relay;
pub mod staging;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::LifecycleCapability;
use ryeos_state::external_content::products::producer_recipe::{
    ProducerCwdSource, ProducerEnvironmentBinding, ProducerEnvironmentSource,
    ProducerExecutableSource, ProducerPreparedImmutableFile, ProducerStdinSource,
    ProductProducerRecipe,
};
use ryeos_state::external_execution::admission::{
    ExternalCandidateProcFilesystem, ExternalCandidateQualificationUse,
    ExternalCandidateRequirement,
};
use ryeos_state::objects::{
    ExecutableSearchPathEntry, ExternalContentKind, ExternalContentManifestEntryKind,
    ExternalContentMountRoot, ExternalContentRealizationSet, SessionProcessEnvironmentValue,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::{ffi::OsStr, path::Path};

pub const SCENARIO: &str = "test.independent_routed_runtime.v1";
/// Signed policy selector for the one daemon-owned direct Codex producer.
pub const PRODUCER_SCENARIO_ID: &str = "native_codex";
pub const DIRECT_PRODUCER_RECIPE_REF: &str = "config:fixtures/independent-runtime/direct-codex";
pub const INPUT_LIMIT: usize = 8 * 1024;
pub const REALIZATIONS_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExactScenario {
    pub subject_manifest_hash: String,
    pub controller_manifest_hash: String,
    pub tools_manifest_hash: String,
    pub configurations_manifest_hash: String,
    pub codex_sha256: String,
    pub relay_sha256: String,
    pub scripted_baseline_sha256: String,
    pub command_environment_template_sha256: String,
    pub responses_origin: String,
    /// Authored before the turn from the selected command-tool product. The
    /// verifier never learns its expected result from Codex or guest output.
    pub expected_command_output: String,
    pub requirement: ExternalCandidateRequirement,
    /// Typed admission projection, authored before execution. This binds the
    /// qualification context but does not itself prove native guest delivery.
    pub execution_environment: QualificationExecutionEnvironment,
    /// Expected signed sibling Config recipe, fixed before the verifier root
    /// runs. The observation must match this typed contract, not only itself.
    pub expected_producer_recipe_ref: String,
    pub expected_producer_recipe: ProductProducerRecipe,
    pub capture_limit_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationExecutionEnvironment {
    pub realizations: ExternalContentRealizationSet,
    pub executable_search: Vec<ExecutableSearchPathEntry>,
    pub process_environment: BTreeMap<String, SessionProcessEnvironmentValue>,
}

impl QualificationExecutionEnvironment {
    /// Production-only closure-shape preflight. The caller must supply
    /// independently admitted worker/environment/product realization sets;
    /// this equality check does not authenticate their source by itself.
    /// It remains separate from the four-tree qualification fixture selector.
    pub fn validate_production_codex_closure(
        &self,
        worker: &ExternalContentRealizationSet,
        environment: &ExternalContentRealizationSet,
        product: &ExternalContentRealizationSet,
        admitted_search: &[ExecutableSearchPathEntry],
        admitted_process_environment: &BTreeMap<String, SessionProcessEnvironmentValue>,
        context: &ExternalCandidateQualificationUse,
        tools_manifest_hash: &str,
        product_manifest_hash: &str,
    ) -> Result<()> {
        self.realizations.validate()?;
        worker.validate()?;
        environment.validate()?;
        product.validate()?;
        context.validate()?;
        ensure!(
            self.digest()? == context.execution_environment_digest,
            "production closure differs from qualification context"
        );
        ensure!(
            self.executable_search == admitted_search
                && self.process_environment == *admitted_process_environment,
            "production guest environment differs from admitted definition"
        );
        let worker_ids = [
            "codex",
            "codex-bwrap",
            "codex-code-mode-host",
            "codex-rg",
            "codex-zsh",
        ];
        let expected = [
            (
                "authoring-tools",
                ExternalContentKind::Tree,
                "authoring-tools",
            ),
            ("codex", ExternalContentKind::File, "codex"),
            (
                "codex-bwrap",
                ExternalContentKind::File,
                "codex-resources/bwrap",
            ),
            (
                "codex-code-mode-host",
                ExternalContentKind::File,
                "codex-code-mode-host",
            ),
            ("codex-rg", ExternalContentKind::File, "codex-path/rg"),
            (
                "codex-zsh",
                ExternalContentKind::File,
                "codex-resources/zsh/bin/zsh",
            ),
            ("guest-runtime", ExternalContentKind::Tree, "guest-runtime"),
        ];
        ensure!(
            worker.iter().map(|r| r.id.as_str()).eq(worker_ids)
                && environment
                    .iter()
                    .map(|r| r.id.as_str())
                    .eq(["authoring-tools"])
                && product.iter().map(|r| r.id.as_str()).eq(["guest-runtime"])
                && self
                    .realizations
                    .iter()
                    .map(|r| r.id.as_str())
                    .eq(expected.iter().map(|e| e.0)),
            "production Codex closure has missing or extra realizations"
        );
        for ((actual, (id, kind, mount)), signed) in self.realizations.iter().zip(expected).zip([
            environment.iter().next().unwrap(),
            worker.iter().find(|r| r.id == "codex").unwrap(),
            worker.iter().find(|r| r.id == "codex-bwrap").unwrap(),
            worker
                .iter()
                .find(|r| r.id == "codex-code-mode-host")
                .unwrap(),
            worker.iter().find(|r| r.id == "codex-rg").unwrap(),
            worker.iter().find(|r| r.id == "codex-zsh").unwrap(),
            product.iter().next().unwrap(),
        ]) {
            ensure!(
                actual.id == id
                    && actual.kind == kind
                    && actual.mode == ryeos_state::objects::ExternalContentMode::Pinned
                    && actual.mount_root == ExternalContentMountRoot::ExecutionRuntime
                    && actual.mount == mount
                    && actual == signed,
                "production Codex realization {id} differs from signed closure"
            );
        }
        ensure!(
            self.realizations
                .iter()
                .find(|r| r.id == "codex")
                .unwrap()
                .manifest_hash
                == context.provider_executable_manifest_hash
                && self
                    .realizations
                    .iter()
                    .find(|r| r.id == "authoring-tools")
                    .unwrap()
                    .manifest_hash
                    == tools_manifest_hash
                && self
                    .realizations
                    .iter()
                    .find(|r| r.id == "guest-runtime")
                    .unwrap()
                    .manifest_hash
                    == product_manifest_hash,
            "production Codex provider, tools, or product hash differs from bound context"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        ExternalCandidateQualificationUse::admitted_execution_environment_digest(
            &self.realizations,
            &self.executable_search,
            &self.process_environment,
        )
    }

    pub fn expected_guest_environment(
        &self,
        requirement: &ExternalCandidateRequirement,
    ) -> Result<ryeos_state::external_execution::admission::ExternalCandidateGuestEnvironment> {
        let destinations = ryeos_state::external_execution::admission::ExternalCandidateGuestEnvironment::expected_destinations(
            requirement,
            &self.realizations,
        )?;
        ryeos_state::external_execution::admission::ExternalCandidateGuestEnvironment::derive(
            &requirement.runtime_recipe,
            &self.process_environment,
            &self.executable_search,
            &destinations,
        )
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Parameters {
    pub scenario: String,
    pub configuration: ExactScenario,
    pub external_candidate_qualification_context: ExternalCandidateQualificationUse,
}

#[derive(Debug, Clone)]
pub struct SelectedInput {
    subject_manifest_hash: String,
    trees: BTreeMap<String, String>,
}

pub struct SelectedRoots {
    pub subject: lillux::PinnedDirectory,
    pub controller: lillux::PinnedDirectory,
    pub tools: lillux::PinnedDirectory,
    pub configurations: lillux::PinnedDirectory,
}

fn exactly_one_binary(
    root: &lillux::PinnedDirectory,
    relative: &str,
    expected_sha256: &str,
    maximum_bytes: u64,
) -> Result<()> {
    let root_entries = root.entries_no_follow_bounded(2)?;
    ensure!(
        root_entries.len() == 1
            && root_entries[0].name.as_os_str() == OsStr::new("bin")
            && root_entries[0].entry_type == lillux::PinnedEntryType::Directory,
        "runtime root has an unexpected entry"
    );
    let bin = root
        .open_child_directory(OsStr::new("bin"))?
        .context("runtime bin directory absent")?;
    let binary_name = relative
        .strip_prefix("bin/")
        .context("runtime binary path")?;
    let bin_entries = bin.entries_no_follow_bounded(2)?;
    ensure!(
        bin_entries.len() == 1
            && bin_entries[0].name.as_os_str() == OsStr::new(binary_name)
            && bin_entries[0].entry_type == lillux::PinnedEntryType::Regular,
        "runtime bin has an unexpected entry"
    );
    let mut paths = Vec::new();
    root.visit_regular_files_bounded(
        lillux::DirectoryTraversalBudget::new(4, 2),
        |_, _| Ok(false),
        |relative, _| {
            paths.push(
                relative
                    .to_str()
                    .context("non-UTF8 binary path")?
                    .to_owned(),
            );
            Ok(())
        },
    )?;
    ensure!(
        paths == [relative.to_owned()],
        "runtime has extra or missing members"
    );
    let binary = root
        .open_pinned_regular_descendant(Path::new(relative), false)?
        .context("selected binary absent")?;
    let observation = binary.observation()?;
    ensure!(
        (1..=maximum_bytes).contains(&observation.size())
            && observation.portable_mode()? == 0o755
            && binary.digest_stable_exact(&observation)? == expected_sha256,
        "selected binary differs from signed identity"
    );
    root.ensure_path_binding()?;
    Ok(())
}

fn exact_member(
    root: &lillux::PinnedDirectory,
    relative: &str,
    expected_sha256: &str,
    expected_mode: u32,
    maximum_bytes: u64,
) -> Result<()> {
    let file = root
        .open_pinned_regular_descendant(Path::new(relative), false)?
        .with_context(|| format!("selected member {relative} absent"))?;
    let observation = file.observation()?;
    ensure!(
        (1..=maximum_bytes).contains(&observation.size())
            && observation.portable_mode()? == expected_mode
            && file.digest_stable_exact(&observation)? == expected_sha256,
        "selected member {relative} differs from signed identity"
    );
    Ok(())
}

fn validate_scripted_baseline(
    configurations: &lillux::PinnedDirectory,
    expected_origin: &str,
) -> Result<Vec<u8>> {
    let file = configurations
        .open_pinned_regular(OsStr::new("scripted.config.toml"), false)?
        .context("selected scripted baseline absent")?;
    let bytes = file.read_stable_bounded(&file.observation()?, 64 * 1024)?;
    let config: toml::Value = std::str::from_utf8(&bytes)?.parse()?;
    let providers = config
        .get("model_providers")
        .and_then(toml::Value::as_table)
        .context("scripted model provider table absent")?;
    ensure!(
        config.get("model_provider").and_then(toml::Value::as_str) == Some("routing-fixture")
            && providers.len() == 1,
        "scripted baseline selects an unexpected provider"
    );
    let provider = providers
        .get("routing-fixture")
        .and_then(toml::Value::as_table)
        .context("scripted provider absent")?;
    let exact = |name: &str, value: toml::Value| provider.get(name) == Some(&value);
    ensure!(
        provider.len() == 7
            && exact(
                "name",
                toml::Value::String("credential-free deterministic routing fixture".into()),
            )
            && exact("base_url", toml::Value::String(expected_origin.to_owned()))
            && exact("wire_api", toml::Value::String("responses".into()))
            && exact("requires_openai_auth", toml::Value::Boolean(false))
            && exact("supports_websockets", toml::Value::Boolean(false))
            && exact("request_max_retries", toml::Value::Integer(0))
            && exact("stream_max_retries", toml::Value::Integer(0)),
        "scripted provider has changed origin, authentication or retry semantics"
    );
    // The provider table alone is insufficient: an otherwise matching
    // baseline could enable MCP, web search, notifications or ambient command
    // execution elsewhere in Codex configuration. Compare the entire parsed
    // configuration against this Codex-specific, credential-free test policy.
    let expected: toml::Value = expected_scripted_baseline(expected_origin).parse()?;
    ensure!(
        config == expected,
        "scripted baseline enables an unplanned Codex setting"
    );
    configurations.ensure_path_binding()?;
    Ok(bytes)
}

fn expected_scripted_baseline(origin: &str) -> String {
    r#"model = "gpt-5.5"
model_provider = "routing-fixture"
approval_policy = "never"
default_permissions = "danger-full-access"
check_for_update_on_startup = false
web_search = "disabled"
allow_login_shell = false
mcp_servers = {}
notify = []
[permissions.danger-full-access]
filesystem = { ":root" = "write" }
network = { enabled = true }
[agents]
enabled = false
[orchestrator.skills]
enabled = false
[orchestrator.mcp]
enabled = false
[features]
apps = false
plugins = false
skill_mcp_dependency_install = false
remote_plugin = false
hooks = false
multi_agent = false
multi_agent_v2 = false
memories = false
network_proxy = false
code_mode_host = true
[features.code_mode]
enabled = false
[model_providers.routing-fixture]
name = "credential-free deterministic routing fixture"
base_url = "{ORIGIN}"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false
request_max_retries = 0
stream_max_retries = 0
"#
    .replace("{ORIGIN}", origin)
}

impl SelectedInput {
    pub fn subject_manifest_hash(&self) -> &str {
        &self.subject_manifest_hash
    }

    pub fn open_roots(
        &self,
        project: &lillux::PinnedDirectory,
        parameters: &Parameters,
    ) -> Result<SelectedRoots> {
        parameters.validate()?;
        let scenario = &parameters.configuration;
        ensure!(
            self.subject_manifest_hash == scenario.subject_manifest_hash,
            "selected subject manifest differs from verifier parameters"
        );
        let qualification = project
            .open_child_directory(OsStr::new("qualification"))?
            .context("selected qualification inputs absent")?;
        let open = |id: &str| -> Result<lillux::PinnedDirectory> {
            ensure!(
                self.trees
                    .get(id)
                    .is_some_and(|v| v == &format!("qualification/{id}")),
                "selected realization changed mount"
            );
            qualification
                .open_child_directory(OsStr::new(id))?
                .with_context(|| format!("selected {id} tree absent"))
        };
        let roots = SelectedRoots {
            subject: open("subject")?,
            controller: open("controller")?,
            tools: open("tools")?,
            configurations: open("configurations")?,
        };
        exactly_one_binary(
            &roots.subject,
            "bin/codex",
            &scenario.codex_sha256,
            268_435_456,
        )?;
        exactly_one_binary(
            &roots.controller,
            "bin/ryeos-synthetic-routed-guest",
            &scenario.relay_sha256,
            64 * 1024 * 1024,
        )?;
        for name in ["bin/zsh", "bin/rg"] {
            let binary = roots
                .tools
                .open_pinned_regular_descendant(Path::new(name), false)?
                .with_context(|| format!("required guest command tool {name} absent"))?;
            ensure!(
                binary.observation()?.portable_mode()? == 0o755,
                "guest command tool is not executable"
            );
        }
        exact_member(
            &roots.configurations,
            "scripted.config.toml",
            &scenario.scripted_baseline_sha256,
            0o644,
            64 * 1024,
        )?;
        exact_member(
            &roots.configurations,
            "environments.toml.template",
            &scenario.command_environment_template_sha256,
            0o644,
            64 * 1024,
        )?;
        exact_member(
            &roots.configurations,
            "admitted-profile.json",
            &parameters
                .external_candidate_qualification_context
                .profile_hash,
            0o644,
            u64::try_from(ryeos_state::objects::MAX_STRUCTURED_SESSION_PROFILE_BYTES)?,
        )?;
        let profile_file = roots
            .configurations
            .open_pinned_regular_descendant(Path::new("admitted-profile.json"), false)?
            .context("admitted profile member absent")?;
        let profile_bytes = profile_file.read_stable_bounded(
            &profile_file.observation()?,
            u64::try_from(ryeos_state::objects::MAX_STRUCTURED_SESSION_PROFILE_BYTES)?,
        )?;
        let profile: serde_json::Value = serde_json::from_slice(&profile_bytes)?;
        ensure!(
            lillux::sha256_hex(&profile_bytes)
                == parameters
                    .external_candidate_qualification_context
                    .profile_hash
                && lillux::canonical_json(&profile)?.as_bytes() == profile_bytes,
            "admitted profile member differs from its bound canonical identity"
        );
        let profile_requirement: ExternalCandidateRequirement = serde_json::from_value(
            profile
                .get("external_candidate")
                .cloned()
                .context("admitted profile has no external candidate requirement")?,
        )?;
        ensure!(
            profile_requirement == scenario.requirement
                && profile.get("transport").and_then(serde_json::Value::as_str)
                    == Some("stdio_jsonrpc")
                && profile
                    .get("workload_client")
                    .is_some_and(serde_json::Value::is_null)
                && profile
                    .get("workload_realization_id")
                    .and_then(serde_json::Value::as_str)
                    == Some("codex"),
            "admitted profile does not select the exact routed Codex requirement"
        );
        let mut configuration_members = Vec::new();
        roots.configurations.visit_regular_files_bounded(
            lillux::DirectoryTraversalBudget::new(3, 1),
            |_, _| Ok(false),
            |relative, _| {
                configuration_members.push(
                    relative
                        .to_str()
                        .context("non-UTF8 configuration member")?
                        .to_owned(),
                );
                Ok(())
            },
        )?;
        configuration_members.sort();
        ensure!(
            configuration_members
                == [
                    "admitted-profile.json".to_owned(),
                    "environments.toml.template".to_owned(),
                    "scripted.config.toml".to_owned(),
                ],
            "configuration tree has an extra or missing member"
        );
        // The subject is a large-content tree. `exactly_one_binary` above
        // independently checks its complete shape, executable mode and full
        // binary SHA-256 against the signed scenario. Its manifest identity is
        // separately authenticated by the daemon-protected realization set;
        // the small-content observer below cannot represent this tier.
        for (id, root, expected) in [
            (
                "controller",
                &roots.controller,
                &scenario.controller_manifest_hash,
            ),
            ("tools", &roots.tools, &scenario.tools_manifest_hash),
            (
                "configurations",
                &roots.configurations,
                &scenario.configurations_manifest_hash,
            ),
        ] {
            let manifest = ryeos_state::observe_external_content_tree_exact(root)
                .with_context(|| format!("observe selected {id} tree"))?;
            ensure!(
                ryeos_state::external_content_manifest_digest(&manifest)? == *expected,
                "selected {id} tree changed from sealed manifest"
            );
        }
        qualification.ensure_path_binding()?;
        project.ensure_path_binding()?;
        Ok(roots)
    }

    /// Prepare the native probe only from the sealed selection and opened
    /// members. This is an input-construction step, not a runtime observation
    /// or a qualification claim. In particular, no fixture-supplied recipe or
    /// effective environment is accepted here.
    pub fn prepare_native_probe_request(
        &self,
        project: &lillux::PinnedDirectory,
        parameters: &Parameters,
    ) -> Result<(SelectedRoots, Vec<u8>)> {
        let roots = self.open_roots(project, parameters)?;
        let scenario = &parameters.configuration;
        validate_scripted_baseline(&roots.configurations, &scenario.responses_origin)?;
        let environment = scenario
            .execution_environment
            .expected_guest_environment(&scenario.requirement)?
            .environment;
        // The exact observed manifest is the signed member preimage. Never
        // derive request hashes from a separate per-file observation: a tree
        // that briefly changes and changes back could otherwise self-attest
        // bytes absent from the signed manifest.
        let tools_manifest = ryeos_state::observe_external_content_tree_exact(&roots.tools)?;
        ensure!(
            ryeos_state::external_content_manifest_digest(&tools_manifest)?
                == scenario.tools_manifest_hash,
            "command tools changed while preparing native probe"
        );
        let mut tools = BTreeMap::new();
        for entry in &tools_manifest.entries {
            match entry.kind {
                ExternalContentManifestEntryKind::Dir => continue,
                ExternalContentManifestEntryKind::File => {
                    let mode = entry.mode.context("command tool mode absent")?;
                    let size = entry.size.context("command tool size absent")?;
                    let hash = entry
                        .blob_hash
                        .as_deref()
                        .context("command tool hash absent")?;
                    ensure!(
                        (1..=64 * 1024 * 1024).contains(&size)
                            && matches!(mode, 0o644 | 0o755)
                            && canonical_hash(hash),
                        "invalid command tool identity"
                    );
                    ensure!(
                        tools
                            .insert(
                                entry.path.clone(),
                                serde_json::json!({"sha256": hash, "mode": mode}),
                            )
                            .is_none(),
                        "duplicate command tool"
                    );
                }
                ExternalContentManifestEntryKind::Symlink => {
                    anyhow::bail!("command tool tree contains a symlink")
                }
            }
        }
        ensure!(tools.len() <= 64, "too many command tools");
        ensure!(
            tools
                .get("bin/zsh")
                .is_some_and(|entry| entry["mode"] == 0o755)
                && tools
                    .get("bin/rg")
                    .is_some_and(|entry| entry["mode"] == 0o755),
            "selected command tools are incomplete"
        );
        let request = serde_json::json!({
            "schema": "test.routed_guest.v1",
            "recipe": scenario.requirement.runtime_recipe,
            "effective_environment": environment,
            "runtime": {"bin/codex": {
                "sha256": scenario.codex_sha256,
                "mode": 0o755,
            }},
            "tools": tools,
            "capture_limit": scenario.capture_limit_bytes,
        });
        let bytes = lillux::canonical_json(&request)?.into_bytes();
        ensure!(
            bytes.len() <= 64 * 1024,
            "native probe request exceeds bound"
        );
        roots.subject.ensure_path_binding()?;
        roots.tools.ensure_path_binding()?;
        Ok((roots, bytes))
    }
}

fn canonical_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Parameters {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= INPUT_LIMIT,
            "verifier parameters exceed bound"
        );
        let parameters: Self = serde_json::from_slice(bytes)?;
        parameters.validate()?;
        Ok(parameters)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.scenario == SCENARIO, "unsupported verifier scenario");
        for hash in [
            &self.configuration.subject_manifest_hash,
            &self.configuration.controller_manifest_hash,
            &self.configuration.tools_manifest_hash,
            &self.configuration.configurations_manifest_hash,
            &self.configuration.codex_sha256,
            &self.configuration.relay_sha256,
            &self.configuration.scripted_baseline_sha256,
            &self.configuration.command_environment_template_sha256,
        ] {
            ensure!(canonical_hash(hash), "noncanonical scenario hash");
        }
        ensure!(
            (1..=1024 * 1024).contains(&self.configuration.capture_limit_bytes),
            "invalid capture limit"
        );
        ensure!(
            self.configuration.expected_command_output.len() <= 8192
                && self
                    .configuration
                    .expected_command_output
                    .starts_with("/workspace\nripgrep ")
                && self.configuration.expected_command_output.ends_with('\n')
                && !self.configuration.expected_command_output.contains('\0'),
            "invalid authored command output"
        );
        let origin = self
            .configuration
            .responses_origin
            .strip_prefix("http://")
            .context("scripted peer origin is not explicit HTTP")?;
        let address: std::net::SocketAddr = origin.parse()?;
        ensure!(
            address.ip().is_loopback()
                && address.port() != 0
                && self.configuration.responses_origin == format!("http://{address}"),
            "scripted peer origin is not canonical loopback"
        );
        self.configuration.requirement.validate()?;
        self.configuration.expected_producer_recipe.validate()?;
        let direct = &self.configuration.expected_producer_recipe;
        let expected_bindings = BTreeMap::from([
            (
                "CODEX_HOME".into(),
                ProducerEnvironmentBinding::PreparedDirectory {
                    id: staging::DIRECT_HOME_ID.into(),
                },
            ),
            (
                "HOME".into(),
                ProducerEnvironmentBinding::PreparedDirectory {
                    id: staging::DIRECT_HOME_ID.into(),
                },
            ),
            (
                "PATH".into(),
                ProducerEnvironmentBinding::Literal {
                    value: String::new(),
                },
            ),
            (
                "LANG".into(),
                ProducerEnvironmentBinding::Literal { value: "C".into() },
            ),
            (
                "LC_ALL".into(),
                ProducerEnvironmentBinding::Literal { value: "C".into() },
            ),
        ]);
        let expected_immutable_files = vec![
            ProducerPreparedImmutableFile {
                prepared_directory_id: staging::DIRECT_HOME_ID.into(),
                leaf_name: "config.toml".into(),
                maximum_bytes: 65536,
                expected_sha256: self.configuration.scripted_baseline_sha256.clone(),
            },
            ProducerPreparedImmutableFile {
                prepared_directory_id: staging::DIRECT_HOME_ID.into(),
                leaf_name: "environments.toml".into(),
                maximum_bytes: 65536,
                expected_sha256: staging::direct_command_environment_sha256()?,
            },
        ];
        let expected_ingress = self
            .configuration
            .responses_origin
            .strip_prefix("http://")
            .context("direct producer origin is not HTTP")?;
        ensure!(
            self.configuration.expected_producer_recipe_ref == DIRECT_PRODUCER_RECIPE_REF
                && direct.executable_source
                    == ProducerExecutableSource::AdmittedRealizationMember {
                        realization_id: "subject".into(),
                        manifest_hash: self.configuration.subject_manifest_hash.clone(),
                        relative_path: "bin/codex".into(),
                        executable_sha256: self.configuration.codex_sha256.clone(),
                    }
                && direct.argv
                    == [
                        "--strict-config",
                        "-c",
                        "check_for_update_on_startup=false",
                        "app-server"
                    ]
                && direct.stdin_source
                    == ProducerStdinSource::InteractiveVerifierChannel {
                        maximum_frame_bytes: 64 * 1024,
                        maximum_total_bytes: 1024 * 1024,
                        maximum_frames: 128,
                    }
                && direct.cwd_source
                    == ProducerCwdSource::PreparedDirectory {
                        id: staging::DIRECT_OCCURRENCE_ID.into(),
                    }
                && direct.environment_sources == [ProducerEnvironmentSource::AdmittedRealizations]
                && direct.environment_bindings == expected_bindings
                && direct.prepared_immutable_files == expected_immutable_files
                && direct
                    .loopback_ingress
                    .as_ref()
                    .is_some_and(|ingress| ingress.address == expected_ingress),
            "expected producer recipe differs from the finite direct Codex target"
        );
        ensure!(
            self.configuration.requirement.provider_declaration_id == "codex-hosted"
                && self
                    .configuration
                    .requirement
                    .runtime_product_declaration_id
                    == "guest-runtime"
                && self
                    .configuration
                    .requirement
                    .required_lifecycle_capabilities
                    == [LifecycleCapability::ExactTerminalObservation]
                        .into_iter()
                        .collect(),
            "scenario does not select the signed Codex guest-runtime lifecycle requirement"
        );
        let recipe = &self.configuration.requirement.runtime_recipe;
        ensure!(
            recipe.runtime_mount_destination == "/runtime"
                && recipe.executable_relative_path == "bin/codex"
                && recipe.argv0 == "codex"
                && recipe.arguments.iter().map(String::as_str).eq([
                    "exec-server",
                    "--listen",
                    "stdio",
                ])
                && recipe.cwd == "/workspace"
                && recipe.environment.is_empty()
                && recipe.max_stdout_bytes == 1024 * 1024
                && recipe.max_stderr_bytes == 1024 * 1024
                && recipe.proc_filesystem == ExternalCandidateProcFilesystem::PidNamespaceNested
                && !recipe.contain_process_group
                && recipe.nested_sandbox,
            "signed runtime recipe does not select the inspected Codex member"
        );
        self.external_candidate_qualification_context.validate()?;
        ensure!(
            self.external_candidate_qualification_context
                .requirement_digest
                == self
                    .configuration
                    .requirement
                    .qualification_requirement_digest()?,
            "qualification use context differs from exact requirement"
        );
        ensure!(
            self.external_candidate_qualification_context
                .execution_environment_digest
                == self.configuration.execution_environment.digest()?,
            "qualification use context differs from typed execution environment"
        );
        let provider = self
            .configuration
            .execution_environment
            .realizations
            .iter()
            .find(|entry| entry.id == "codex")
            .context("typed execution environment has no controller Codex realization")?;
        ensure!(
            provider.mode == ryeos_state::objects::ExternalContentMode::Pinned
                && provider.kind == ExternalContentKind::File
                && provider.mount_root == ExternalContentMountRoot::ExecutionRuntime
                && provider.mount == "codex"
                && provider.manifest_hash
                    == self
                        .external_candidate_qualification_context
                        .provider_executable_manifest_hash,
            "qualification use provider executable differs from typed realization"
        );
        let tools = self
            .configuration
            .execution_environment
            .realizations
            .iter()
            .find(|entry| entry.id == "authoring-tools")
            .context("typed execution environment has no authoring-tools realization")?;
        ensure!(
            tools.mode == ryeos_state::objects::ExternalContentMode::Pinned
                && tools.kind == ExternalContentKind::Tree
                && tools.mount_root == ExternalContentMountRoot::ExecutionRuntime
                && tools.mount == "authoring-tools"
                && tools.manifest_hash == self.configuration.tools_manifest_hash,
            "typed authoring tools differ from selected verifier tools"
        );
        let subject = self
            .configuration
            .execution_environment
            .realizations
            .iter()
            .find(|entry| {
                entry.id
                    == self
                        .configuration
                        .requirement
                        .runtime_product_declaration_id
            })
            .context("typed execution environment has no guest-runtime product")?;
        ensure!(
            subject.mode == ryeos_state::objects::ExternalContentMode::Pinned
                && subject.kind == ExternalContentKind::Tree
                && subject.mount_root == ExternalContentMountRoot::ExecutionRuntime
                && subject.mount == "guest-runtime"
                && subject.manifest_hash == self.configuration.subject_manifest_hash,
            "typed guest runtime differs from selected qualification subject"
        );
        self.configuration
            .execution_environment
            .expected_guest_environment(&self.configuration.requirement)?;
        Ok(())
    }

    /// Authenticate the daemon-protected sealed realization set against the
    /// policy's exact selected tuple. The source trees are then opened by
    /// descriptor in the verifier's private admitted project namespace.
    pub fn select(&self, raw: &str) -> Result<SelectedInput> {
        self.validate()?;
        ensure!(
            !raw.is_empty() && raw.len() <= REALIZATIONS_LIMIT,
            "sealed realization set absent or oversized"
        );
        let set = ExternalContentRealizationSet::from_value(&serde_json::from_str(raw)?)?;
        let expected = BTreeMap::from([
            ("subject", &self.configuration.subject_manifest_hash),
            ("controller", &self.configuration.controller_manifest_hash),
            ("tools", &self.configuration.tools_manifest_hash),
            (
                "configurations",
                &self.configuration.configurations_manifest_hash,
            ),
        ]);
        ensure!(
            set.iter().len() == expected.len(),
            "unexpected realization count"
        );
        let mut trees = BTreeMap::new();
        for realization in set.iter() {
            let digest = expected
                .get(realization.id.as_str())
                .context("unexpected realization ID")?;
            ensure!(
                realization.kind == ExternalContentKind::Tree
                    && realization.mount_root == ExternalContentMountRoot::Project
                    && realization.mode == ryeos_state::objects::ExternalContentMode::Pinned
                    && realization.mount == format!("qualification/{}", realization.id)
                    && realization.manifest_hash.as_str() == digest.as_str()
                    && realization.entry_count > 0
                    && realization.total_bytes > 0,
                "selected realization differs from signed tuple"
            );
            ensure!(
                trees
                    .insert(realization.id.clone(), realization.mount.clone())
                    .is_none(),
                "duplicate selected realization"
            );
        }
        Ok(SelectedInput {
            subject_manifest_hash: self.configuration.subject_manifest_hash.clone(),
            trees,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_fixture_configuration_bytes_match_selected_verifier_policy() {
        let origin = "http://127.0.0.1:18765";
        let scripted = include_str!(
            "../../../../tests/e2e/external-execution/fixtures/independent-scripted-config.toml.template"
        );
        assert_eq!(
            scripted.replace("{ORIGIN}", origin),
            expected_scripted_baseline(origin)
        );
        let environment = include_str!(
            "../../../../tests/e2e/external-execution/fixtures/independent-environments.toml.template"
        );
        assert_eq!(environment, staging::COMMAND_ENVIRONMENT_TEMPLATE);
    }
    use serde_json::json;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn production_codex_closure_refuses_shape_and_identity_changes() {
        use ryeos_state::objects::{ExternalContentMode, ExternalContentRealization};
        let make = |id: &str, kind, mount: &str, hash: &str| ExternalContentRealization {
            id: id.into(),
            kind,
            mode: ExternalContentMode::Pinned,
            manifest_hash: hash.into(),
            entry_count: 1,
            total_bytes: 1,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: mount.into(),
        };
        let worker = ExternalContentRealizationSet::new(vec![
            make("codex", ExternalContentKind::File, "codex", &"1".repeat(64)),
            make(
                "codex-code-mode-host",
                ExternalContentKind::File,
                "codex-code-mode-host",
                &"2".repeat(64),
            ),
            make(
                "codex-zsh",
                ExternalContentKind::File,
                "codex-resources/zsh/bin/zsh",
                &"3".repeat(64),
            ),
            make(
                "codex-bwrap",
                ExternalContentKind::File,
                "codex-resources/bwrap",
                &"4".repeat(64),
            ),
            make(
                "codex-rg",
                ExternalContentKind::File,
                "codex-path/rg",
                &"5".repeat(64),
            ),
        ])
        .unwrap();
        let environment = ExternalContentRealizationSet::new(vec![make(
            "authoring-tools",
            ExternalContentKind::Tree,
            "authoring-tools",
            &"6".repeat(64),
        )])
        .unwrap();
        let product = ExternalContentRealizationSet::new(vec![make(
            "guest-runtime",
            ExternalContentKind::Tree,
            "guest-runtime",
            &"7".repeat(64),
        )])
        .unwrap();
        let all = || {
            worker
                .iter()
                .chain(environment.iter())
                .chain(product.iter())
                .cloned()
                .collect::<Vec<_>>()
        };
        let admitted_search = vec![ExecutableSearchPathEntry {
            realization_id: "authoring-tools".into(),
            relative_directory: "bin".into(),
        }];
        let admitted_process_environment = BTreeMap::from([(
            "TZ".into(),
            SessionProcessEnvironmentValue::Literal {
                value: "UTC".into(),
            },
        )]);
        let build = |entries| QualificationExecutionEnvironment {
            realizations: ExternalContentRealizationSet::new(entries).unwrap(),
            executable_search: admitted_search.clone(),
            process_environment: admitted_process_environment.clone(),
        };
        let base = build(all());
        let mut context = ExternalCandidateQualificationUse {
            schema: ryeos_state::external_execution::admission::QUALIFICATION_CONTEXT_SCHEMA.into(),
            requirement_digest: "8".repeat(64),
            profile_hash: "8".repeat(64),
            source_binding_hash: "8".repeat(64),
            source_content_manifest_hash: "8".repeat(64),
            provider_executable_manifest_hash: "1".repeat(64),
            execution_environment_digest: base.digest().unwrap(),
        };
        let check = |value: &QualificationExecutionEnvironment,
                     context: &ExternalCandidateQualificationUse| {
            value.validate_production_codex_closure(
                &worker,
                &environment,
                &product,
                &admitted_search,
                &admitted_process_environment,
                context,
                &"6".repeat(64),
                &"7".repeat(64),
            )
        };
        check(&base, &context).unwrap();
        let mut entries = all();
        entries.retain(|r| r.id != "codex-rg");
        let missing = build(entries);
        context.execution_environment_digest = missing.digest().unwrap();
        assert!(check(&missing, &context).is_err());
        let mut entries = all();
        entries.push(make(
            "extra",
            ExternalContentKind::File,
            "extra",
            &"9".repeat(64),
        ));
        let extra = build(entries);
        context.execution_environment_digest = extra.digest().unwrap();
        assert!(check(&extra, &context).is_err());
        for mutate in [
            ("mount", "elsewhere"),
            ("kind", "tree"),
            ("mode", "captured"),
            ("mount_root", "project"),
            ("manifest_hash", "9"),
            ("entry_count", "2"),
            ("total_bytes", "2"),
        ] {
            let mut entries = all();
            let member = entries.iter_mut().find(|r| r.id == "codex-zsh").unwrap();
            match mutate.0 {
                "mount" => member.mount = mutate.1.into(),
                "kind" => member.kind = ExternalContentKind::Tree,
                "mode" => member.mode = ExternalContentMode::Captured,
                "mount_root" => member.mount_root = ExternalContentMountRoot::Project,
                "manifest_hash" => member.manifest_hash = "9".repeat(64),
                "entry_count" => member.entry_count = 2,
                "total_bytes" => member.total_bytes = 2,
                _ => unreachable!(),
            }
            let changed = build(entries);
            context.execution_environment_digest = changed.digest().unwrap();
            assert!(check(&changed, &context).is_err(), "{}", mutate.0);
        }
        context.execution_environment_digest = base.digest().unwrap();
        context.provider_executable_manifest_hash = "9".repeat(64);
        assert!(check(&base, &context).is_err());
        context.provider_executable_manifest_hash = "1".repeat(64);
        let mut changed_search = base.clone();
        changed_search.executable_search.clear();
        context.execution_environment_digest = changed_search.digest().unwrap();
        assert!(check(&changed_search, &context).is_err());
        let mut changed_environment = base.clone();
        changed_environment.process_environment.insert(
            "TZ".into(),
            SessionProcessEnvironmentValue::Literal {
                value: "Pacific/Auckland".into(),
            },
        );
        context.execution_environment_digest = changed_environment.digest().unwrap();
        assert!(check(&changed_environment, &context).is_err());
        context.execution_environment_digest = "9".repeat(64);
        assert!(check(&base, &context).is_err());
        context.execution_environment_digest = base.digest().unwrap();
        assert!(
            base.validate_production_codex_closure(
                &worker,
                &environment,
                &product,
                &admitted_search,
                &admitted_process_environment,
                &context,
                &"9".repeat(64),
                &"7".repeat(64),
            )
            .is_err()
        );
        assert!(
            base.validate_production_codex_closure(
                &worker,
                &environment,
                &product,
                &admitted_search,
                &admitted_process_environment,
                &context,
                &"6".repeat(64),
                &"9".repeat(64),
            )
            .is_err()
        );
        let mut changed_worker = worker.iter().cloned().collect::<Vec<_>>();
        changed_worker
            .iter_mut()
            .find(|realization| realization.id == "codex-rg")
            .unwrap()
            .manifest_hash = "9".repeat(64);
        let changed_worker = ExternalContentRealizationSet::new(changed_worker).unwrap();
        assert!(
            base.validate_production_codex_closure(
                &changed_worker,
                &environment,
                &product,
                &admitted_search,
                &admitted_process_environment,
                &context,
                &"6".repeat(64),
                &"7".repeat(64),
            )
            .is_err()
        );
        let changed_environment = ExternalContentRealizationSet::new(vec![make(
            "authoring-tools",
            ExternalContentKind::Tree,
            "authoring-tools",
            &"9".repeat(64),
        )])
        .unwrap();
        assert!(
            base.validate_production_codex_closure(
                &worker,
                &changed_environment,
                &product,
                &admitted_search,
                &admitted_process_environment,
                &context,
                &"6".repeat(64),
                &"7".repeat(64),
            )
            .is_err()
        );
        let changed_product = ExternalContentRealizationSet::new(vec![make(
            "guest-runtime",
            ExternalContentKind::Tree,
            "guest-runtime",
            &"9".repeat(64),
        )])
        .unwrap();
        assert!(
            base.validate_production_codex_closure(
                &worker,
                &environment,
                &changed_product,
                &admitted_search,
                &admitted_process_environment,
                &context,
                &"6".repeat(64),
                &"7".repeat(64),
            )
            .is_err()
        );
    }

    fn scenario() -> ExactScenario {
        let mut requirement =
            ryeos_state::external_execution::admission::test_support::fixture_requirement();
        requirement.runtime_recipe.executable_relative_path = "bin/codex".into();
        requirement.runtime_recipe.argv0 = "codex".into();
        requirement.runtime_recipe.arguments =
            vec!["exec-server".into(), "--listen".into(), "stdio".into()];
        requirement.runtime_product_declaration_id = "guest-runtime".into();
        requirement
            .required_lifecycle_capabilities
            .insert(LifecycleCapability::ExactTerminalObservation);
        ExactScenario {
            subject_manifest_hash: "a".repeat(64),
            controller_manifest_hash: "b".repeat(64),
            tools_manifest_hash: "c".repeat(64),
            configurations_manifest_hash: "d".repeat(64),
            codex_sha256: "e".repeat(64),
            relay_sha256: "f".repeat(64),
            scripted_baseline_sha256: "2".repeat(64),
            command_environment_template_sha256: "3".repeat(64),
            responses_origin: "http://127.0.0.1:1234".into(),
            expected_command_output: "/workspace\nripgrep fixture\n".into(),
            requirement,
            execution_environment: QualificationExecutionEnvironment {
                realizations: ExternalContentRealizationSet::new(vec![
                    ryeos_state::objects::ExternalContentRealization {
                        id: "codex".into(),
                        kind: ExternalContentKind::File,
                        mode: ryeos_state::objects::ExternalContentMode::Pinned,
                        manifest_hash: "e".repeat(64),
                        entry_count: 1,
                        total_bytes: 1,
                        mount_root: ExternalContentMountRoot::ExecutionRuntime,
                        mount: "codex".into(),
                    },
                    ryeos_state::objects::ExternalContentRealization {
                        id: "authoring-tools".into(),
                        kind: ExternalContentKind::Tree,
                        mode: ryeos_state::objects::ExternalContentMode::Pinned,
                        manifest_hash: "c".repeat(64),
                        entry_count: 2,
                        total_bytes: 2,
                        mount_root: ExternalContentMountRoot::ExecutionRuntime,
                        mount: "authoring-tools".into(),
                    },
                    ryeos_state::objects::ExternalContentRealization {
                        id: "guest-runtime".into(),
                        kind: ExternalContentKind::Tree,
                        mode: ryeos_state::objects::ExternalContentMode::Pinned,
                        manifest_hash: "a".repeat(64),
                        entry_count: 2,
                        total_bytes: 2,
                        mount_root: ExternalContentMountRoot::ExecutionRuntime,
                        mount: "guest-runtime".into(),
                    },
                ])
                .unwrap(),
                executable_search: Vec::new(),
                process_environment: BTreeMap::new(),
            },
            expected_producer_recipe_ref: DIRECT_PRODUCER_RECIPE_REF.into(),
            expected_producer_recipe: ProductProducerRecipe::from_value(serde_json::json!({
                "schema":"ryeos.product_producer_recipe.v5",
                "executable_source":{"kind":"admitted_realization_member",
                    "realization_id":"subject", "manifest_hash":"a".repeat(64),
                    "relative_path":"bin/codex", "executable_sha256":"e".repeat(64)},
                "argv":["--strict-config","-c","check_for_update_on_startup=false","app-server"],
                "stdin_source":{"kind":"interactive_verifier_channel",
                    "maximum_frame_bytes":65536,"maximum_total_bytes":1048576,"maximum_frames":128},
                "cwd_source":{"kind":"prepared_directory","id":"codex-occurrence"},
                "environment_sources":["admitted_realizations"],
                "environment_bindings":{
                    "CODEX_HOME":{"kind":"prepared_directory","id":"codex-home"},
                    "HOME":{"kind":"prepared_directory","id":"codex-home"},
                    "PATH":{"kind":"literal","value":""},
                    "LANG":{"kind":"literal","value":"C"},
                    "LC_ALL":{"kind":"literal","value":"C"}},
                "prepared_immutable_files":[
                    {"prepared_directory_id":"codex-home","leaf_name":"config.toml","maximum_bytes":65536,"expected_sha256":"2".repeat(64)},
                    {"prepared_directory_id":"codex-home","leaf_name":"environments.toml","maximum_bytes":65536,
                        "expected_sha256":staging::direct_command_environment_sha256().unwrap()}],
                "loopback_ingress":{"address":"127.0.0.1:1234"},
                "bounds":{"maximum_wall_time_ms":170000,
                    "maximum_stdout_bytes":6291456,
                    "maximum_stderr_bytes":1048576,
                    "maximum_memory_bytes":2147483648_u64,
                    "maximum_processes":64}
            }))
            .unwrap(),
            capture_limit_bytes: 4096,
        }
    }

    #[test]
    fn scripted_baseline_refuses_auth_retry_or_origin_drift() {
        let temp = tempfile::tempdir().unwrap();
        let baseline = temp.path().join("scripted.config.toml");
        let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let write = |content: String| std::fs::write(&baseline, content).unwrap();
        let expected = "http://127.0.0.1:1234";
        let clean = expected_scripted_baseline(expected);
        write(clean.clone());
        validate_scripted_baseline(&root, expected).unwrap();
        write(clean.replace(expected, "http://127.0.0.1:5678"));
        assert!(validate_scripted_baseline(&root, expected).is_err());
        write(expected_scripted_baseline(expected).replace(
            "requires_openai_auth = false",
            "requires_openai_auth = true",
        ));
        assert!(validate_scripted_baseline(&root, expected).is_err());
        write(
            expected_scripted_baseline(expected)
                .replace("request_max_retries = 0", "request_max_retries = 1"),
        );
        assert!(validate_scripted_baseline(&root, expected).is_err());
        write(expected_scripted_baseline(expected));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&baseline)
            .unwrap()
            .write_all(b"http_headers = { Authorization = \"ambient\" }\n")
            .unwrap();
        assert!(validate_scripted_baseline(&root, expected).is_err());
        write(
            expected_scripted_baseline(expected)
                .replace("web_search = \"disabled\"", "web_search = \"live\""),
        );
        assert!(validate_scripted_baseline(&root, expected).is_err());
        write(expected_scripted_baseline(expected).replace(
            "mcp_servers = {}",
            "mcp_servers = { ambient = { command = \"run\" } }",
        ));
        assert!(validate_scripted_baseline(&root, expected).is_err());
    }

    #[test]
    fn scenario_requirement_matches_current_codex_bundle_profile() {
        let profile: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../bundles/codex/.ai/workers/codex/lib/hosted/external-authoring.profile.json"
        ))
        .unwrap();
        let admitted: ExternalCandidateRequirement =
            serde_json::from_value(profile["external_candidate"].clone()).unwrap();
        assert_eq!(scenario().requirement, admitted);
        assert_eq!(
            profile["transport"],
            serde_json::Value::String("stdio_jsonrpc".into())
        );
        assert!(profile["workload_client"].is_null());
        assert_eq!(profile["workload_realization_id"], "codex");
    }

    #[test]
    fn selected_runtime_recipe_refuses_changed_process_constraints() {
        let scenario = scenario();
        let mut parameters = Parameters {
            scenario: SCENARIO.into(),
            external_candidate_qualification_context: ExternalCandidateQualificationUse {
                schema: ryeos_state::external_execution::admission::QUALIFICATION_CONTEXT_SCHEMA
                    .into(),
                requirement_digest: scenario
                    .requirement
                    .qualification_requirement_digest()
                    .unwrap(),
                profile_hash: "4".repeat(64),
                source_binding_hash: "5".repeat(64),
                source_content_manifest_hash: "6".repeat(64),
                provider_executable_manifest_hash: "e".repeat(64),
                execution_environment_digest: scenario.execution_environment.digest().unwrap(),
            },
            configuration: scenario,
        };
        parameters.validate().unwrap();
        let original = parameters.configuration.requirement.runtime_recipe.clone();
        let mutations: Vec<
            Box<
                dyn Fn(
                    &mut ryeos_state::external_execution::admission::ExternalCandidateRuntimeRecipe,
                ),
            >,
        > = vec![
            Box::new(|recipe| {
                recipe.environment.insert("HOME".into(), "/tmp".into());
            }),
            Box::new(|recipe| recipe.max_stdout_bytes = 1024),
            Box::new(|recipe| recipe.max_stderr_bytes = 1024),
            Box::new(|recipe| recipe.proc_filesystem = ExternalCandidateProcFilesystem::Empty),
            Box::new(|recipe| recipe.contain_process_group = true),
            Box::new(|recipe| recipe.nested_sandbox = false),
        ];
        for mutate in mutations {
            parameters.configuration.requirement.runtime_recipe = original.clone();
            mutate(&mut parameters.configuration.requirement.runtime_recipe);
            if let Ok(digest) = parameters
                .configuration
                .requirement
                .qualification_requirement_digest()
            {
                parameters
                    .external_candidate_qualification_context
                    .requirement_digest = digest;
            }
            assert!(parameters.validate().is_err());
        }
    }

    #[test]
    fn typed_execution_environment_preimage_must_match_qualification_use() {
        use ryeos_state::objects::{ExternalContentMode, ExternalContentRealization};
        let mut scenario = scenario();
        scenario.tools_manifest_hash = "8".repeat(64);
        scenario.execution_environment = QualificationExecutionEnvironment {
            realizations: ExternalContentRealizationSet::new(vec![
                ExternalContentRealization {
                    id: "codex".into(),
                    kind: ExternalContentKind::File,
                    mode: ExternalContentMode::Pinned,
                    manifest_hash: "7".repeat(64),
                    entry_count: 1,
                    total_bytes: 1,
                    mount_root: ExternalContentMountRoot::ExecutionRuntime,
                    mount: "codex".into(),
                },
                ExternalContentRealization {
                    id: "authoring-tools".into(),
                    kind: ExternalContentKind::Tree,
                    mode: ExternalContentMode::Pinned,
                    manifest_hash: "8".repeat(64),
                    entry_count: 2,
                    total_bytes: 2,
                    mount_root: ExternalContentMountRoot::ExecutionRuntime,
                    mount: "authoring-tools".into(),
                },
                ExternalContentRealization {
                    id: "guest-runtime".into(),
                    kind: ExternalContentKind::Tree,
                    mode: ExternalContentMode::Pinned,
                    manifest_hash: "a".repeat(64),
                    entry_count: 2,
                    total_bytes: 2,
                    mount_root: ExternalContentMountRoot::ExecutionRuntime,
                    mount: "guest-runtime".into(),
                },
            ])
            .unwrap(),
            executable_search: vec![ExecutableSearchPathEntry {
                realization_id: "authoring-tools".into(),
                relative_directory: "bin".into(),
            }],
            process_environment: BTreeMap::from([(
                "TZ".into(),
                SessionProcessEnvironmentValue::Literal {
                    value: "UTC".into(),
                },
            )]),
        };
        let context = ExternalCandidateQualificationUse {
            schema: ryeos_state::external_execution::admission::QUALIFICATION_CONTEXT_SCHEMA.into(),
            requirement_digest: scenario
                .requirement
                .qualification_requirement_digest()
                .unwrap(),
            profile_hash: "4".repeat(64),
            source_binding_hash: "5".repeat(64),
            source_content_manifest_hash: "6".repeat(64),
            provider_executable_manifest_hash: "7".repeat(64),
            execution_environment_digest: scenario.execution_environment.digest().unwrap(),
        };
        let original = Parameters {
            scenario: SCENARIO.into(),
            configuration: scenario,
            external_candidate_qualification_context: context,
        };
        original.validate().unwrap();

        let mut changed = original.clone();
        changed
            .external_candidate_qualification_context
            .provider_executable_manifest_hash = "9".repeat(64);
        assert!(changed.validate().is_err());
        for (field, value) in [
            ("kind", json!("tree")),
            ("mode", json!("captured")),
            ("mount_root", json!("project")),
            ("mount", json!("different")),
        ] {
            let mut changed = original.clone();
            let mut realizations = changed
                .configuration
                .execution_environment
                .realizations
                .to_value()
                .unwrap();
            realizations[1][field] = value;
            changed.configuration.execution_environment.realizations =
                ExternalContentRealizationSet::from_value(&realizations).unwrap();
            changed
                .external_candidate_qualification_context
                .execution_environment_digest = changed
                .configuration
                .execution_environment
                .digest()
                .unwrap();
            assert!(changed.validate().is_err(), "{field}");
        }

        let mut changed = original.clone();
        changed
            .configuration
            .execution_environment
            .executable_search[0]
            .relative_directory = ".".into();
        assert!(changed.validate().is_err());
        let mut changed = original.clone();
        changed
            .configuration
            .execution_environment
            .process_environment
            .insert(
                "TZ".into(),
                SessionProcessEnvironmentValue::Literal {
                    value: "Pacific/Auckland".into(),
                },
            );
        assert!(changed.validate().is_err());
        let mut changed = original.clone();
        let mut realizations = changed
            .configuration
            .execution_environment
            .realizations
            .to_value()
            .unwrap();
        realizations[0]["manifest_hash"] = serde_json::json!("9".repeat(64));
        changed.configuration.execution_environment.realizations =
            ExternalContentRealizationSet::from_value(&realizations).unwrap();
        assert!(changed.validate().is_err());
        let mut changed = original.clone();
        changed.configuration.execution_environment.realizations =
            ExternalContentRealizationSet::default();
        assert!(changed.validate().is_err());

        let mut changed = original.clone();
        changed
            .configuration
            .execution_environment
            .process_environment
            .insert(
                "TMPDIR".into(),
                SessionProcessEnvironmentValue::RuntimeViewDirectory {
                    relative_path: "scratch".into(),
                },
            );
        assert!(changed.validate().is_err());

        let mut wire = serde_json::to_value(&original).unwrap();
        wire["configuration"]["execution_environment"]
            .as_object_mut()
            .unwrap()
            .remove("process_environment");
        assert!(Parameters::parse(&serde_json::to_vec(&wire).unwrap()).is_err());
        let mut wire = serde_json::to_value(&original).unwrap();
        wire["configuration"]["execution_environment"]["unexpected"] = json!(true);
        assert!(Parameters::parse(&serde_json::to_vec(&wire).unwrap()).is_err());
    }

    #[test]
    fn selected_tuple_rejects_ambient_and_wrong_mounts() {
        let scenario = scenario();
        let context = ExternalCandidateQualificationUse {
            schema: ryeos_state::external_execution::admission::QUALIFICATION_CONTEXT_SCHEMA.into(),
            requirement_digest: scenario
                .requirement
                .qualification_requirement_digest()
                .unwrap(),
            profile_hash: "4".repeat(64),
            source_binding_hash: "5".repeat(64),
            source_content_manifest_hash: "6".repeat(64),
            provider_executable_manifest_hash: "e".repeat(64),
            execution_environment_digest: scenario.execution_environment.digest().unwrap(),
        };
        let parameters = Parameters {
            scenario: SCENARIO.into(),
            configuration: scenario,
            external_candidate_qualification_context: context,
        };
        let sealed = json!([
            {"id":"configurations","kind":"tree","mode":"pinned","manifest_hash":"d".repeat(64),"entry_count":1,"total_bytes":1,"mount_root":"project","mount":"qualification/configurations"},
            {"id":"controller","kind":"tree","mode":"pinned","manifest_hash":"b".repeat(64),"entry_count":1,"total_bytes":1,"mount_root":"project","mount":"qualification/controller"},
            {"id":"subject","kind":"tree","mode":"pinned","manifest_hash":"a".repeat(64),"entry_count":1,"total_bytes":1,"mount_root":"project","mount":"qualification/subject"},
            {"id":"tools","kind":"tree","mode":"pinned","manifest_hash":"c".repeat(64),"entry_count":1,"total_bytes":1,"mount_root":"project","mount":"qualification/tools"}
        ]);
        assert!(parameters.select(&sealed.to_string()).is_ok());
        let mut wrong = sealed.clone();
        wrong[2]["mount"] = "qualification/other".into();
        assert!(parameters.select(&wrong.to_string()).is_err());
        wrong[2]["mount"] = "qualification/subject".into();
        wrong[2]["manifest_hash"] = "0".repeat(64).into();
        assert!(parameters.select(&wrong.to_string()).is_err());
        wrong[2]["manifest_hash"] = "a".repeat(64).into();
        wrong[2]["mode"] = "captured".into();
        assert!(parameters.select(&wrong.to_string()).is_err());
        let mut extra = sealed.clone();
        extra.as_array_mut().unwrap().push(json!({
            "id":"ambient", "kind":"tree", "mode":"pinned",
            "manifest_hash":"8".repeat(64), "entry_count":1, "total_bytes":1,
            "mount_root":"project", "mount":"qualification/ambient"
        }));
        assert!(parameters.select(&extra.to_string()).is_err());
        let mut downgraded = parameters.clone();
        downgraded
            .configuration
            .requirement
            .required_lifecycle_capabilities
            .clear();
        downgraded
            .external_candidate_qualification_context
            .requirement_digest = downgraded
            .configuration
            .requirement
            .qualification_requirement_digest()
            .unwrap();
        assert!(downgraded.validate().is_err());
        let mut changed = parameters;
        changed
            .external_candidate_qualification_context
            .requirement_digest = "0".repeat(64);
        assert!(changed.validate().is_err());
        changed
            .external_candidate_qualification_context
            .requirement_digest = changed
            .configuration
            .requirement
            .qualification_requirement_digest()
            .unwrap();
        changed.configuration.responses_origin = "http://example.com:1234".into();
        assert!(changed.validate().is_err());
        changed.configuration.responses_origin = "http://127.0.0.1:1234".into();
        changed.configuration.expected_command_output = "observed output".into();
        assert!(changed.validate().is_err());
        changed.configuration.expected_command_output = "/workspace\nripgrep fixture\n".into();
        changed.configuration.codex_sha256 = "E".repeat(64);
        assert!(changed.validate().is_err());
    }

    #[test]
    fn selected_roots_recheck_real_members_and_refuse_drift() {
        let temp = tempfile::tempdir().unwrap();
        let inputs = temp.path().join("qualification");
        let mut scenario = scenario();
        scenario.execution_environment.process_environment.insert(
            "TZ".into(),
            SessionProcessEnvironmentValue::Literal {
                value: "UTC".into(),
            },
        );
        for name in ["subject", "controller", "tools"] {
            std::fs::create_dir_all(inputs.join(name).join("bin")).unwrap();
        }
        std::fs::create_dir_all(inputs.join("configurations")).unwrap();
        let codex = inputs.join("subject/bin/codex");
        let relay = inputs.join("controller/bin/ryeos-synthetic-routed-guest");
        std::fs::write(&codex, b"codex-fixture").unwrap();
        std::fs::write(&relay, b"relay-fixture").unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&relay, std::fs::Permissions::from_mode(0o755)).unwrap();
        for name in ["rg", "zsh"] {
            let path = inputs.join("tools/bin").join(name);
            std::fs::write(&path, b"tool-fixture").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            inputs.join("configurations/scripted.config.toml"),
            expected_scripted_baseline(&scenario.responses_origin),
        )
        .unwrap();
        std::fs::write(
            inputs.join("configurations/environments.toml.template"),
            staging::COMMAND_ENVIRONMENT_TEMPLATE,
        )
        .unwrap();
        let profile = json!({
            "external_candidate": scenario.requirement.clone(),
            "transport": "stdio_jsonrpc",
            "workload_client": null,
            "workload_realization_id": "codex",
        });
        let profile_bytes = lillux::canonical_json(&profile).unwrap();
        std::fs::write(
            inputs.join("configurations/admitted-profile.json"),
            profile_bytes.as_bytes(),
        )
        .unwrap();
        let capture = |name| {
            let dir = lillux::PinnedDirectory::open(&inputs.join(name))
                .unwrap()
                .unwrap();
            ryeos_state::external_content_manifest_digest(
                &ryeos_state::observe_external_content_tree_exact(&dir).unwrap(),
            )
            .unwrap()
        };
        scenario.codex_sha256 = lillux::sha256_hex(b"codex-fixture");
        scenario.relay_sha256 = lillux::sha256_hex(b"relay-fixture");
        scenario.scripted_baseline_sha256 = lillux::sha256_hex(
            &std::fs::read(inputs.join("configurations/scripted.config.toml")).unwrap(),
        );
        scenario.command_environment_template_sha256 =
            lillux::sha256_hex(staging::COMMAND_ENVIRONMENT_TEMPLATE.as_bytes());
        scenario.subject_manifest_hash = capture("subject");
        scenario.controller_manifest_hash = capture("controller");
        scenario.tools_manifest_hash = capture("tools");
        scenario.expected_producer_recipe.executable_source =
            ProducerExecutableSource::AdmittedRealizationMember {
                realization_id: "subject".into(),
                manifest_hash: scenario.subject_manifest_hash.clone(),
                relative_path: "bin/codex".into(),
                executable_sha256: scenario.codex_sha256.clone(),
            };
        scenario.expected_producer_recipe.prepared_immutable_files[0].expected_sha256 =
            scenario.scripted_baseline_sha256.clone();
        let mut production = scenario
            .execution_environment
            .realizations
            .to_value()
            .unwrap();
        production[0]["manifest_hash"] = json!(scenario.tools_manifest_hash);
        production[2]["manifest_hash"] = json!(scenario.subject_manifest_hash);
        scenario.execution_environment.realizations =
            ExternalContentRealizationSet::from_value(&production).unwrap();
        scenario.configurations_manifest_hash = capture("configurations");
        let parameters = Parameters {
            scenario: SCENARIO.into(),
            external_candidate_qualification_context: ExternalCandidateQualificationUse {
                schema: ryeos_state::external_execution::admission::QUALIFICATION_CONTEXT_SCHEMA
                    .into(),
                requirement_digest: scenario
                    .requirement
                    .qualification_requirement_digest()
                    .unwrap(),
                profile_hash: lillux::sha256_hex(profile_bytes.as_bytes()),
                source_binding_hash: "5".repeat(64),
                source_content_manifest_hash: "6".repeat(64),
                provider_executable_manifest_hash: "e".repeat(64),
                execution_environment_digest: scenario.execution_environment.digest().unwrap(),
            },
            configuration: scenario,
        };
        let selected = SelectedInput {
            subject_manifest_hash: parameters.configuration.subject_manifest_hash.clone(),
            trees: ["subject", "controller", "tools", "configurations"]
                .into_iter()
                .map(|name| (name.into(), format!("qualification/{name}")))
                .collect(),
        };
        let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        selected.open_roots(&root, &parameters).unwrap();
        let (_, native_request) = selected
            .prepare_native_probe_request(&root, &parameters)
            .unwrap();
        let native_request: serde_json::Value = serde_json::from_slice(&native_request).unwrap();
        assert_eq!(native_request["schema"], "test.routed_guest.v1");
        assert_eq!(
            native_request["recipe"],
            serde_json::to_value(&parameters.configuration.requirement.runtime_recipe).unwrap()
        );
        assert_eq!(native_request["effective_environment"], json!({"TZ":"UTC"}));
        assert_eq!(
            native_request["runtime"]["bin/codex"]["sha256"],
            parameters.configuration.codex_sha256
        );
        assert_eq!(
            native_request["tools"]["bin/zsh"]["sha256"],
            lillux::sha256_hex(b"tool-fixture")
        );
        assert_eq!(native_request["capture_limit"], 4096);
        let direct = staging::stage_direct_target_probe(&selected, &root, &parameters).unwrap();
        direct.recheck_preflight(&parameters).unwrap();
        let direct_home = temp.path().join("prepared/codex-home");
        let direct_guest = temp.path().join("prepared/codex-occurrence/guest");
        let direct_environment: toml::Value =
            std::fs::read_to_string(direct_home.join("environments.toml"))
                .unwrap()
                .parse()
                .unwrap();
        assert_eq!(
            direct_environment["environments"][0]["program"].as_str(),
            Some("/workspace/qualification/controller/bin/ryeos-synthetic-routed-guest")
        );
        assert_eq!(
            direct_environment["environments"][0]["cwd"].as_str(),
            Some("/ryeos/producer-prepared/codex-occurrence/guest")
        );
        assert_eq!(
            direct.environment_configuration_sha256(),
            lillux::sha256_hex(&std::fs::read(direct_home.join("environments.toml")).unwrap())
        );
        assert_eq!(
            direct.request_sha256(),
            lillux::sha256_hex(&std::fs::read(direct_guest.join("guest-request.json")).unwrap())
        );
        std::fs::write(direct_guest.join("candidate/ambient"), b"not frozen").unwrap();
        assert!(direct.recheck_preflight(&parameters).is_err());
        std::fs::remove_file(direct_guest.join("candidate/ambient")).unwrap();
        direct.recheck_preflight(&parameters).unwrap();
        let direct_bytes = std::fs::read(direct_home.join("environments.toml")).unwrap();
        std::fs::write(direct_home.join("environments.toml"), b"tampered").unwrap();
        assert!(direct.recheck_preflight(&parameters).is_err());
        std::fs::write(direct_home.join("environments.toml"), direct_bytes).unwrap();
        direct.recheck_preflight(&parameters).unwrap();
        std::fs::write(
            direct_guest
                .join("candidate")
                .join(scripted_provider::CANDIDATE_RELATIVE_PATH),
            scripted_provider::CANDIDATE_CONTENT,
        )
        .unwrap();
        std::fs::set_permissions(
            direct_guest
                .join("candidate")
                .join(scripted_provider::CANDIDATE_RELATIVE_PATH),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::write(
            direct_guest.join("guest-observation.json"),
            b"{\"schema\":\"fixture\"}",
        )
        .unwrap();
        std::fs::set_permissions(
            direct_guest.join("guest-observation.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(direct.recheck_preflight(&parameters).is_err());
        let frozen = direct
            .inspect_frozen_after_scope_empty(&parameters)
            .unwrap();
        assert_eq!(
            frozen.candidate_sha256,
            lillux::sha256_hex(scripted_provider::CANDIDATE_CONTENT.as_bytes())
        );
        assert_eq!(frozen.guest_observation["schema"], "fixture");
        std::fs::write(direct_home.join("config.toml"), b"tampered").unwrap();
        assert!(
            direct
                .inspect_frozen_after_scope_empty(&parameters)
                .is_err()
        );
        let staged = staging::stage_selected_probe(&selected, &root, &parameters).unwrap();
        assert_eq!(
            std::fs::read(staged.codex_executable()).unwrap(),
            b"codex-fixture"
        );
        assert_eq!(
            std::fs::read(staged.controller_executable()).unwrap(),
            b"relay-fixture"
        );
        assert_eq!(
            std::fs::read(staged.guest.path().join("tools/bin/rg")).unwrap(),
            b"tool-fixture"
        );
        assert_eq!(
            staged.request_sha256(),
            lillux::sha256_hex(
                &std::fs::read(staged.guest.path().join("guest-request.json")).unwrap()
            )
        );
        staged.recheck_preflight(&parameters).unwrap();
        let routing = staged
            .routing_scenario(&parameters, "thread-fixture".into(), "turn-fixture".into())
            .unwrap();
        assert_eq!(
            routing.expected_command_output,
            parameters.configuration.expected_command_output
        );
        assert_eq!(
            routing.secret_read_script,
            staged.scripted_canary_commands().unwrap().guest_read
        );
        let guest_scenario = staged.guest_scenario(&parameters, &routing).unwrap();
        assert_eq!(guest_scenario.request_sha256, staged.request_sha256());
        assert_eq!(
            guest_scenario.secret_read_command,
            routing.secret_read_script
        );
        let mut substituted = routing.clone();
        substituted.expected_command_output = "/workspace\nripgrep substituted\n".into();
        assert!(staged.guest_scenario(&parameters, &substituted).is_err());
        let launch = staged.prepare_codex_launch(&parameters).unwrap();
        assert_eq!(launch.argv0.as_deref(), Some("codex"));
        assert_eq!(
            launch.args,
            [
                "--strict-config",
                "-c",
                "check_for_update_on_startup=false",
                "app-server"
            ]
        );
        assert_eq!(launch.envs.len(), 5);
        assert_eq!(launch.inherited_fds.len(), 3);
        assert_eq!(
            launch.inherited_fds[1]
                .digest_regular_file_stable_exact(
                    &launch.inherited_fds[1].regular_file_observation().unwrap()
                )
                .unwrap(),
            parameters.configuration.relay_sha256
        );
        assert!(std::fs::write(launch.inherited_fds[1].path(), b"tampered").is_err());
        let rendered_bytes = std::fs::read(staged.home.path().join("environments.toml")).unwrap();
        let rendered: toml::Value = std::str::from_utf8(&rendered_bytes)
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(rendered["include_local"].as_bool(), Some(false));
        assert_eq!(rendered["environments"].as_array().unwrap().len(), 1);
        assert_eq!(
            rendered["environments"][0]["program"].as_str(),
            launch.inherited_fds[1].path().to_str()
        );
        assert_eq!(
            rendered["environments"][0]["cwd"].as_str(),
            launch.inherited_fds[2].path().to_str()
        );
        assert_ne!(
            rendered["environments"][0]["program"].as_str(),
            staged.controller_executable().to_str()
        );
        drop(launch);
        assert_eq!(
            staged.environment_configuration_sha256(),
            lillux::sha256_hex(
                &std::fs::read(staged.home.path().join("environments.toml")).unwrap()
            )
        );
        std::fs::write(staged.home.path().join("environments.toml"), b"tampered").unwrap();
        assert!(staged.recheck_preflight(&parameters).is_err());
        std::fs::write(staged.home.path().join("environments.toml"), rendered_bytes).unwrap();
        staged.recheck_preflight(&parameters).unwrap();
        std::fs::write(staged.codex_executable(), b"tampered").unwrap();
        assert!(staged.recheck_preflight(&parameters).is_err());
        assert!(staged.prepare_codex_launch(&parameters).is_err());
        std::fs::write(staged.codex_executable(), b"codex-fixture").unwrap();
        staged.recheck_preflight(&parameters).unwrap();
        std::fs::write(staged.controller_executable(), b"tampered").unwrap();
        assert!(staged.recheck_preflight(&parameters).is_err());
        std::fs::write(staged.controller_executable(), b"relay-fixture").unwrap();
        staged.recheck_preflight(&parameters).unwrap();
        std::fs::write(staged.guest.path().join("tools/bin/rg"), b"tampered").unwrap();
        assert!(staged.recheck_preflight(&parameters).is_err());
        std::fs::write(staged.guest.path().join("tools/bin/rg"), b"tool-fixture").unwrap();
        staged.recheck_preflight(&parameters).unwrap();
        std::fs::write(staged.guest.path().join("guest-request.json"), b"tampered").unwrap();
        assert!(staged.recheck_preflight(&parameters).is_err());
        // Reusing a selected input after source mutation must not stage an
        // alternate command closure, even when the prior occurrence survived.
        std::fs::write(inputs.join("tools/bin/rg"), b"mutated").unwrap();
        assert!(staging::stage_selected_probe(&selected, &root, &parameters).is_err());
        std::fs::write(inputs.join("tools/bin/rg"), b"tool-fixture").unwrap();
        std::fs::write(inputs.join("tools/bin/ambient"), b"ambient").unwrap();
        assert!(staging::stage_selected_probe(&selected, &root, &parameters).is_err());
        std::fs::remove_file(inputs.join("tools/bin/ambient")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("rg", inputs.join("tools/bin/alias")).unwrap();
            assert!(staging::stage_selected_probe(&selected, &root, &parameters).is_err());
            std::fs::remove_file(inputs.join("tools/bin/alias")).unwrap();
        }
        assert!(staging::stage_selected_probe(&selected, &root, &parameters).is_ok());
        let mut wrong_selected = selected.clone();
        wrong_selected.subject_manifest_hash = "0".repeat(64);
        assert!(wrong_selected.open_roots(&root, &parameters).is_err());
        let mut wrong_subject = parameters.clone();
        wrong_subject.configuration.subject_manifest_hash = "0".repeat(64);
        assert!(selected.open_roots(&root, &wrong_subject).is_err());
        let mut wrong_controller = parameters.clone();
        wrong_controller.configuration.controller_manifest_hash = "0".repeat(64);
        assert!(selected.open_roots(&root, &wrong_controller).is_err());
        let mut wrong_profile = parameters.clone();
        wrong_profile
            .external_candidate_qualification_context
            .profile_hash = "0".repeat(64);
        assert!(selected.open_roots(&root, &wrong_profile).is_err());
        std::fs::write(inputs.join("subject/bin/ambient"), b"ambient").unwrap();
        assert!(selected.open_roots(&root, &parameters).is_err());
        std::fs::remove_file(inputs.join("subject/bin/ambient")).unwrap();
        std::fs::set_permissions(&relay, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(selected.open_roots(&root, &parameters).is_err());
        std::fs::set_permissions(&relay, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(inputs.join("tools/bin/rg"), b"mutated").unwrap();
        assert!(selected.open_roots(&root, &parameters).is_err());
        assert!(
            selected
                .prepare_native_probe_request(&root, &parameters)
                .is_err()
        );
    }

    #[test]
    fn exact_subject_binary_check_accepts_large_content_tier_without_small_tree_observer() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("bin")).unwrap();
        let path = temp.path().join("bin/codex");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((32 * 1024 * 1024 + 1) as u64).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let binary = root
            .open_pinned_regular_descendant(Path::new("bin/codex"), false)
            .unwrap()
            .unwrap();
        let observation = binary.observation().unwrap();
        let sha256 = binary.digest_stable_exact(&observation).unwrap();
        assert!(ryeos_state::observe_external_content_tree_exact(&root).is_err());
        exactly_one_binary(&root, "bin/codex", &sha256, 268_435_456).unwrap();
        std::fs::write(temp.path().join("bin/ambient"), b"ambient").unwrap();
        assert!(exactly_one_binary(&root, "bin/codex", &sha256, 268_435_456).is_err());
    }
}
