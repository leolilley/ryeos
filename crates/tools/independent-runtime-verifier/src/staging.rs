//! Descriptor-based staging of the selected Codex verifier occurrence.
//!
//! The destination is a unique child of the projectless Tool's private cwd.
//! It is not a durable journal, publisher, or source realization. RyeOS owns
//! thread recovery and may remove this scratch after exact settlement.

use crate::{Parameters, SelectedInput};
use anyhow::{Context as _, Result, ensure};
use lillux::{
    InheritedDescriptorAuthority, PinnedDirectory, PinnedEntryType, PinnedSubordinateProcessRequest,
};
use ryeos_state::objects::ExternalContentManifestEntryKind;
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

pub struct StagedNativeProbe {
    pub occurrence: PinnedDirectory,
    pub guest: PinnedDirectory,
    pub home: PinnedDirectory,
    codex_executable: PathBuf,
    controller_executable: PathBuf,
    guest_executable_authority: InheritedDescriptorAuthority,
    guest_cwd_authority: InheritedDescriptorAuthority,
    request_sha256: String,
    environment_configuration_sha256: String,
    controller_canary_directory: PinnedDirectory,
    controller_canary_value: String,
    controller_canary_observation: lillux::OpenRegularFileObservation,
}

/// Parent-owned inputs for the daemon's direct Codex target. These are placed
/// at the fixed signed prepared-directory coordinates before START; unlike the
/// scenario driver, no verifier-owned descriptor path is embedded in Codex's
/// configuration. This object proves prepared bytes only, not their applied
/// mount or the absence of a concurrent writer.
pub struct StagedDirectTargetProbe {
    prepared: PinnedDirectory,
    occurrence: PinnedDirectory,
    guest: PinnedDirectory,
    home: PinnedDirectory,
    controller: PinnedDirectory,
    request_sha256: String,
    environment_configuration_sha256: String,
}

pub const DIRECT_OCCURRENCE_ID: &str = "codex-occurrence";
pub const DIRECT_HOME_ID: &str = "codex-home";
const DIRECT_CONTROLLER_EXECUTABLE: &str =
    "/workspace/qualification/controller/bin/ryeos-synthetic-routed-guest";
const DIRECT_CONTROLLER_CANARY: &str = "/workspace/verifier-controller/canary";

fn direct_prepared_destination(id: &str) -> Result<PathBuf> {
    ryeos_state::external_content::products::producer_recipe::prepared_directory_mount_destination(
        id,
    )
    .map_err(anyhow::Error::msg)
}

/// Parent-authored challenge in the fresh private verifier workspace. The
/// independently joined provider uses this exact value and path; the scoped
/// child may read it to construct its signed scenario but cannot choose a
/// different canary after observing the model's responses.
pub struct ParentChallenge {
    directory: PinnedDirectory,
    value: String,
    observation: lillux::OpenRegularFileObservation,
}

pub struct ParentProviderEndpoint {
    directory: PinnedDirectory,
    expected: Vec<u8>,
    observation: lillux::OpenRegularFileObservation,
}

const PARENT_CHALLENGE_DIRECTORY: &str = "verifier-controller";

impl ParentChallenge {
    pub fn directory(&self) -> &PinnedDirectory {
        &self.directory
    }

    pub fn publish_provider_endpoint(&self, name: &OsStr) -> Result<ParentProviderEndpoint> {
        let name = name.to_str().context("non-UTF8 provider socket name")?;
        ensure!(
            !name.is_empty()
                && name.len() <= 103
                && name.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'-' | b'_')
                }),
            "provider socket name is not canonical"
        );
        let expected = format!("{name}\n").into_bytes();
        let file = self
            .directory
            .atomic_create_pinned_regular(OsStr::new("provider-endpoint"), &expected, 0o600)?
            .context("provider endpoint already published")?;
        let result = ParentProviderEndpoint {
            directory: self.directory.try_clone()?,
            expected,
            observation: file.observation()?,
        };
        result.recheck()?;
        Ok(result)
    }
    pub fn canary_path(&self) -> PathBuf {
        // The signed command must address the guest's logical workspace,
        // not the verifier's potentially relative descriptor path. The
        // controller-owned canary remains pinned separately for recheck.
        PathBuf::from(DIRECT_CONTROLLER_CANARY)
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn scripted_canary_commands(&self) -> Result<ScriptedCanaryCommands> {
        scripted_canary_commands(&self.canary_path())
    }

    pub fn recheck(&self) -> Result<()> {
        check_controller_canary(&self.directory, &self.value, &self.observation)
    }
}

impl ParentProviderEndpoint {
    pub fn recheck(&self) -> Result<()> {
        let file = self
            .directory
            .open_pinned_regular(OsStr::new("provider-endpoint"), false)?
            .context("parent provider endpoint disappeared")?;
        ensure!(
            file.permission_mode()? == 0o600
                && file.read_stable_bounded(&self.observation, 104)? == self.expected,
            "parent provider endpoint changed"
        );
        self.directory.ensure_path_binding()?;
        Ok(())
    }
}

pub const CONTROLLER_CANARY_DENIAL: &str = "controller-canary-read-denied";

pub struct ScriptedCanaryCommands {
    pub forbidden_local_write: String,
    pub guest_read: String,
}

pub(crate) const COMMAND_ENVIRONMENT_TEMPLATE: &str = r#"default = "ryeos-external-candidate"
include_local = false
[[environments]]
id = "ryeos-external-candidate"
program = "__RYEOS_VERIFIER_GUEST_PROGRAM__"
cwd = "__RYEOS_VERIFIER_GUEST_CWD__"
env = {}
"#;

#[derive(Serialize)]
struct CommandEnvironment<'a> {
    default: &'static str,
    include_local: bool,
    environments: Vec<CommandEnvironmentEntry<'a>>,
}

#[derive(Serialize)]
struct CommandEnvironmentEntry<'a> {
    id: &'static str,
    program: &'a str,
    cwd: &'a str,
    env: BTreeMap<String, String>,
}

fn materialize_command_environment(
    configurations: &PinnedDirectory,
    controller_executable: &Path,
    guest_cwd: &Path,
    expected_template_hash: &str,
) -> Result<Vec<u8>> {
    let template = configurations
        .open_pinned_regular(OsStr::new("environments.toml.template"), false)?
        .context("signed command environment template absent")?;
    let observation = template.observation()?;
    ensure!(
        observation.size() <= 64 * 1024
            && observation.portable_mode()? == 0o644
            && template.digest_stable_exact(&observation)? == expected_template_hash,
        "signed command environment template changed"
    );
    let bytes = template.read_stable_bounded(&observation, 64 * 1024)?;
    let parsed: toml::Value = std::str::from_utf8(&bytes)?.parse()?;
    let expected: toml::Value = COMMAND_ENVIRONMENT_TEMPLATE.parse()?;
    ensure!(
        parsed == expected,
        "signed command environment template has unplanned semantics"
    );
    configurations.ensure_path_binding()?;
    render_command_environment(controller_executable, guest_cwd)
}

fn render_command_environment(controller_executable: &Path, guest_cwd: &Path) -> Result<Vec<u8>> {
    ensure!(
        controller_executable.is_absolute() && guest_cwd.is_absolute(),
        "verifier command environment paths must be absolute"
    );
    let program = controller_executable
        .to_str()
        .context("non-UTF8 verifier guest program")?;
    let cwd = guest_cwd.to_str().context("non-UTF8 verifier guest cwd")?;
    let output = toml::to_string(&CommandEnvironment {
        default: "ryeos-external-candidate",
        include_local: false,
        environments: vec![CommandEnvironmentEntry {
            id: "ryeos-external-candidate",
            program,
            cwd,
            env: BTreeMap::new(),
        }],
    })?
    .into_bytes();
    ensure!(
        !output.is_empty() && output.len() <= 64 * 1024,
        "rendered command environment exceeds bound"
    );
    Ok(output)
}

/// Exact byte identity the signed direct recipe must bind before daemon
/// START. The staged template is checked separately against its signed hash
/// before these deterministic bytes are written into the prepared home.
pub fn direct_command_environment_sha256() -> Result<String> {
    let cwd = direct_prepared_destination(DIRECT_OCCURRENCE_ID)?.join("guest");
    Ok(lillux::sha256_hex(&render_command_environment(
        Path::new(DIRECT_CONTROLLER_EXECUTABLE),
        &cwd,
    )?))
}

impl StagedNativeProbe {
    pub fn provider_directory(&self) -> Result<PinnedDirectory> {
        self.controller_canary_directory.ensure_path_binding()?;
        self.controller_canary_directory.try_clone()
    }

    pub fn provider_endpoint_name(&self) -> Result<String> {
        let file = self
            .controller_canary_directory
            .open_pinned_regular(OsStr::new("provider-endpoint"), false)?
            .context("parent provider endpoint absent")?;
        let observation = file.observation()?;
        ensure!(
            file.permission_mode()? == 0o600 && observation.size() <= 104,
            "parent provider endpoint changed shape"
        );
        let bytes = file.read_stable_bounded(&observation, 104)?;
        let name = std::str::from_utf8(&bytes)?
            .strip_suffix('\n')
            .context("parent provider endpoint is not newline terminated")?;
        ensure!(
            !name.is_empty()
                && name.len() <= 103
                && name.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'-' | b'_')
                }),
            "parent provider socket name is not canonical"
        );
        Ok(name.to_owned())
    }
    /// Inspect the authoring mount only after the enclosing process scope has
    /// proved that every possible writer has settled. A matching digest alone
    /// is not a freeze witness while a descendant can still mutate the file.
    pub fn check_frozen_candidate_after_scope_empty(&self) -> Result<String> {
        let candidate = self
            .guest
            .open_child_directory(OsStr::new("candidate"))?
            .context("staged candidate directory disappeared")?;
        check_exact_candidate(&candidate)
    }
}

fn check_exact_candidate(candidate: &PinnedDirectory) -> Result<String> {
    let entries = candidate.entries_no_follow_bounded(2)?;
    ensure!(
        entries.len() == 1
            && entries[0].name.as_os_str()
                == OsStr::new(crate::scripted_provider::CANDIDATE_RELATIVE_PATH)
            && entries[0].entry_type == PinnedEntryType::Regular,
        "frozen candidate has missing or extra entries"
    );
    let file = candidate
        .open_pinned_regular(
            OsStr::new(crate::scripted_provider::CANDIDATE_RELATIVE_PATH),
            false,
        )?
        .context("frozen candidate file absent")?;
    let observation = file.observation()?;
    let expected = crate::scripted_provider::CANDIDATE_CONTENT.as_bytes();
    ensure!(
        observation.size() == expected.len() as u64 && file.permission_mode()? == 0o644,
        "frozen candidate size or mode changed"
    );
    let bytes = file.read_stable_bounded(&observation, expected.len() as u64)?;
    ensure!(bytes == expected, "frozen candidate bytes changed");
    candidate.ensure_path_binding()?;
    Ok(lillux::sha256_hex(&bytes))
}

impl StagedNativeProbe {
    /// Prepare the exact parent Codex launch. Lillux consumes the selected
    /// executable descriptor and fchdir-selected occurrence; this does not
    /// attest Codex's later command-environment child or its termination.
    pub fn prepare_codex_launch(
        &self,
        parameters: &Parameters,
    ) -> Result<PinnedSubordinateProcessRequest> {
        self.recheck_preflight(parameters)?;
        let executable = self
            .guest
            .open_pinned_regular_descendant(Path::new("runtime/bin/codex"), false)?
            .context("staged Codex executable disappeared")?;
        let observation = executable.observation()?;
        ensure!(
            observation.portable_mode()? == 0o755
                && (1..=268_435_456).contains(&observation.size())
                && executable.digest_stable_exact(&observation)?
                    == parameters.configuration.codex_sha256,
            "Codex launch executable differs from staged identity"
        );
        let home = self.home.try_clone()?.into_inherited_descriptor_path()?;
        let home_path = home
            .path()
            .to_str()
            .context("non-UTF8 inherited Codex home")?
            .to_owned();
        self.occurrence.ensure_path_binding()?;
        Ok(PinnedSubordinateProcessRequest {
            executable,
            cwd: self.occurrence.try_clone()?,
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
            inherited_fds: vec![
                home,
                self.guest_executable_authority.clone(),
                self.guest_cwd_authority.clone(),
            ],
        })
    }

    pub fn codex_executable(&self) -> &Path {
        &self.codex_executable
    }

    pub fn controller_executable(&self) -> &Path {
        &self.controller_executable
    }

    pub fn request_sha256(&self) -> &str {
        &self.request_sha256
    }

    pub fn environment_configuration_sha256(&self) -> &str {
        &self.environment_configuration_sha256
    }

    /// The provider script may name this path, but must never receive the
    /// value. It lives outside the guest tree and is opened again by pinned
    /// authority after the turn to prove it was neither read nor modified.
    pub fn controller_canary_path(&self) -> PathBuf {
        self.controller_canary_directory.path().join("canary")
    }

    pub fn controller_canary_value(&self) -> &str {
        &self.controller_canary_value
    }

    pub fn scripted_canary_commands(&self) -> Result<ScriptedCanaryCommands> {
        scripted_canary_commands(&self.controller_canary_path())
    }

    /// Bind the pure notification checker to values fixed before the turn.
    /// This does not observe Codex, the guest, or terminal state.
    pub fn routing_scenario(
        &self,
        parameters: &Parameters,
        thread_id: String,
        turn_id: String,
    ) -> Result<crate::routing_observation::RoutingScenario> {
        self.recheck_preflight(parameters)?;
        let canary = self.scripted_canary_commands()?;
        let shell = crate::scripted_provider::GUEST_SHELL;
        let guest_command_script = crate::scripted_provider::GUEST_COMMAND_SCRIPT;
        Ok(crate::routing_observation::RoutingScenario {
            thread_id,
            turn_id,
            guest_cwd: "/workspace".into(),
            local_refusal_command: canary.forbidden_local_write,
            guest_command_script: guest_command_script.into(),
            secret_read_script: canary.guest_read.clone(),
            patch_input: crate::scripted_provider::PATCH_INPUT.into(),
            guest_command: format!("{shell} -c '{guest_command_script}'"),
            secret_read_command: format!("{shell} -c '{}'", canary.guest_read),
            candidate_path: crate::scripted_provider::CANDIDATE_PATH.into(),
            candidate_added_content: crate::scripted_provider::CANDIDATE_CONTENT.into(),
            expected_command_output: parameters.configuration.expected_command_output.clone(),
            secret_read_denial: CONTROLLER_CANARY_DENIAL.into(),
            controller_canary_value: self.controller_canary_value.clone(),
        })
    }

    /// The guest checker and app-server checker must consume the same
    /// pre-turn coordinate. A caller cannot swap in a more permissive guest
    /// expectation after observing the native transcript.
    pub fn guest_scenario<'a>(
        &'a self,
        parameters: &Parameters,
        routing: &'a crate::routing_observation::RoutingScenario,
    ) -> Result<crate::guest_observation::GuestScenario<'a>> {
        let expected = self.routing_scenario(
            parameters,
            routing.thread_id.clone(),
            routing.turn_id.clone(),
        )?;
        ensure!(routing == &expected, "guest and app-server scenario differ");
        Ok(crate::guest_observation::GuestScenario {
            request_sha256: &self.request_sha256,
            shell: crate::scripted_provider::GUEST_SHELL,
            guest_cwd_uri: "file:///workspace",
            guest_command: crate::scripted_provider::GUEST_COMMAND_SCRIPT,
            secret_read_command: &routing.secret_read_script,
            expected_command_output: &routing.expected_command_output,
            controller_canary_value: &self.controller_canary_value,
            secret_read_denial: CONTROLLER_CANARY_DENIAL,
            candidate_uri: crate::scripted_provider::CANDIDATE_URI,
            candidate_relative_path: crate::scripted_provider::CANDIDATE_RELATIVE_PATH,
            candidate_content: crate::scripted_provider::CANDIDATE_CONTENT.as_bytes(),
        })
    }

    pub fn recheck_controller_canary(&self) -> Result<()> {
        check_controller_canary(
            &self.controller_canary_directory,
            &self.controller_canary_value,
            &self.controller_canary_observation,
        )
    }

    /// Recheck the staged preflight bytes and directory bindings. This does
    /// not seal the writable occurrence or attest which inode a future child
    /// will execute; that handoff remains a separate qualification gate.
    pub fn recheck_preflight(&self, parameters: &Parameters) -> Result<()> {
        let scenario = &parameters.configuration;
        self.occurrence.ensure_path_binding()?;
        self.guest.ensure_path_binding()?;
        self.home.ensure_path_binding()?;
        let runtime = self
            .guest
            .open_child_directory(OsStr::new("runtime"))?
            .context("staged runtime disappeared")?;
        let runtime_bin = runtime
            .open_child_directory(OsStr::new("bin"))?
            .context("staged runtime bin disappeared")?;
        super::exact_member(
            &runtime_bin,
            "codex",
            &scenario.codex_sha256,
            0o755,
            268_435_456,
        )?;
        let controller = self
            .occurrence
            .open_child_directory(OsStr::new("controller"))?
            .context("staged controller disappeared")?;
        self.recheck_controller_canary()?;
        let controller_bin = controller
            .open_child_directory(OsStr::new("bin"))?
            .context("staged controller bin disappeared")?;
        super::exact_member(
            &controller_bin,
            "ryeos-synthetic-routed-guest",
            &scenario.relay_sha256,
            0o755,
            64 * 1024 * 1024,
        )?;
        ensure!(
            self.guest_executable_authority
                .digest_regular_file_stable_exact(
                    &self.guest_executable_authority.regular_file_observation()?
                )?
                == scenario.relay_sha256,
            "sealed guest executable differs from staged identity"
        );
        ensure!(
            self.guest_cwd_authority.directory_identity()? == self.guest.identity()?,
            "inherited guest cwd differs from staged identity"
        );
        let tools = self
            .guest
            .open_child_directory(OsStr::new("tools"))?
            .context("staged tools disappeared")?;
        ensure!(
            ryeos_state::external_content_manifest_digest(
                &ryeos_state::observe_external_content_tree_exact(&tools)?
            )? == scenario.tools_manifest_hash,
            "staged command tools changed"
        );
        super::exact_member(
            &self.home,
            "config.toml",
            &scenario.scripted_baseline_sha256,
            0o644,
            64 * 1024,
        )?;
        super::exact_member(
            &self.home,
            "environments.toml",
            &self.environment_configuration_sha256,
            0o644,
            64 * 1024,
        )?;
        let request = self
            .guest
            .open_pinned_regular(OsStr::new("guest-request.json"), false)?
            .context("staged native request disappeared")?;
        let observed_request = request.observation()?;
        ensure!(
            (1..=64 * 1024).contains(&observed_request.size())
                && request.permission_mode()? == 0o600
                && request.digest_stable_exact(&observed_request)? == self.request_sha256,
            "staged native request changed: size={}, mode={:o}",
            observed_request.size(),
            request.permission_mode()?
        );
        runtime_bin.ensure_path_binding()?;
        controller_bin.ensure_path_binding()?;
        tools.ensure_path_binding()?;
        Ok(())
    }
}

fn scripted_canary_commands(path: &Path) -> Result<ScriptedCanaryCommands> {
    let path = path.to_str().context("non-UTF8 controller canary path")?;
    ensure!(
        path.starts_with('/')
            && path.len() <= 4096
            && path
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte)),
        "controller canary path cannot be represented by exact scripted shell recipe"
    );
    Ok(ScriptedCanaryCommands {
        forbidden_local_write: format!("printf '%s' 'unexpected local write' > '{path}'"),
        // A successful read emits the controller value. A denied read has
        // one explicit exit/status marker checked on both protocol streams.
        guest_read: format!(
            "if IFS= read -r line < \"{path}\"; then printf \"%s\" \"$line\"; else printf \"%s\" {CONTROLLER_CANARY_DENIAL}; exit 73; fi"
        ),
    })
}

fn create_controller_canary(
    controller: &PinnedDirectory,
) -> Result<(String, lillux::OpenRegularFileObservation)> {
    // Hashing fresh OS randomness produces a printable, shell-safe probe
    // value without turning a deterministic fixture string into a secret.
    let value = lillux::sha256_hex(&lillux::crypto::generate_random_bytes::<32>());
    let file = controller
        .atomic_create_pinned_regular(OsStr::new("canary"), format!("{value}\n").as_bytes(), 0o600)?
        .context("controller canary already exists")?;
    let observation = file.observation()?;
    check_controller_canary(controller, &value, &observation)?;
    Ok((value, observation))
}

fn check_controller_canary(
    controller: &PinnedDirectory,
    expected: &str,
    original: &lillux::OpenRegularFileObservation,
) -> Result<()> {
    ensure!(
        lillux::valid_hash(expected),
        "controller canary identity invalid"
    );
    let file = controller
        .open_pinned_regular(OsStr::new("canary"), false)?
        .context("controller canary absent")?;
    ensure!(
        original.size() == 65
            && original.permission_mode()? == 0o600
            && file.read_stable_bounded(original, 65)? == format!("{expected}\n").as_bytes(),
        "controller canary changed"
    );
    controller.ensure_path_binding()?;
    Ok(())
}

fn copy_exact_member(
    source: &PinnedDirectory,
    relative: &Path,
    target: &PinnedDirectory,
    target_name: &OsStr,
    expected_hash: &str,
    mode: u32,
    maximum_bytes: u64,
) -> Result<()> {
    let mut file = source
        .open_pinned_regular_descendant(relative, false)?
        .context("selected staged member disappeared")?;
    let observed = file.observation()?;
    ensure!(
        (1..=maximum_bytes).contains(&observed.size()) && observed.portable_mode()? == mode,
        "selected staged member size or mode changed"
    );
    let (created, copied) = target
        .atomic_create_pinned_regular_from_reader(target_name, &mut file, maximum_bytes, mode)?
        .context("staged member already exists")?;
    ensure!(copied == observed.size(), "staged member length changed");
    let output_observation = created.observation()?;
    ensure!(
        output_observation.portable_mode()? == mode
            && created.digest_stable_exact(&output_observation)? == expected_hash
            && file.digest_stable_exact(&observed)? == expected_hash,
        "staged member differs from selected identity"
    );
    Ok(())
}

fn target_parent(root: &PinnedDirectory, relative: &Path) -> Result<PinnedDirectory> {
    let mut parent = root.try_clone()?;
    for component in relative
        .parent()
        .context("staged member has no parent")?
        .components()
    {
        let Component::Normal(name) = component else {
            anyhow::bail!("staged member has noncanonical parent");
        };
        parent = parent
            .open_child_directory(name)?
            .context("selected staged directory is missing")?;
    }
    Ok(parent)
}

fn stage_selected_tools(
    selected: &PinnedDirectory,
    target: &PinnedDirectory,
    expected_manifest_hash: &str,
) -> Result<()> {
    let manifest = ryeos_state::observe_external_content_tree_exact(selected)?;
    ensure!(
        ryeos_state::external_content_manifest_digest(&manifest)? == expected_manifest_hash,
        "selected tools changed before staging"
    );
    for entry in &manifest.entries {
        match entry.kind {
            ExternalContentManifestEntryKind::Dir => {
                let relative = Path::new(&entry.path);
                let parent = target_parent(target, relative)?;
                parent.create_child(
                    relative
                        .file_name()
                        .context("selected tool directory name absent")?,
                    0o755,
                )?;
            }
            ExternalContentManifestEntryKind::Symlink => {
                anyhow::bail!("selected tools contain a symlink")
            }
            ExternalContentManifestEntryKind::File => {
                let relative = Path::new(&entry.path);
                let parent = target_parent(target, relative)?;
                copy_exact_member(
                    selected,
                    relative,
                    &parent,
                    relative.file_name().context("selected tool name absent")?,
                    entry
                        .blob_hash
                        .as_deref()
                        .context("selected tool hash absent")?,
                    entry.mode.context("selected tool mode absent")?,
                    64 * 1024 * 1024,
                )?;
            }
        }
    }
    ensure!(
        ryeos_state::external_content_manifest_digest(
            &ryeos_state::observe_external_content_tree_exact(target)?
        )? == expected_manifest_hash,
        "staged command tools differ from selected manifest"
    );
    Ok(())
}

/// Stage selected bytes without adopting an existing destination or deriving
/// file identities from a mutable live path. A source drift refuses; it never
/// yields a different admissible request or silently updates a pin.
pub fn create_parent_challenge(projectless_scratch: &PinnedDirectory) -> Result<ParentChallenge> {
    let directory =
        projectless_scratch.create_child(OsStr::new(PARENT_CHALLENGE_DIRECTORY), 0o700)?;
    let (value, observation) = create_controller_canary(&directory)?;
    Ok(ParentChallenge {
        directory,
        value,
        observation,
    })
}

fn open_parent_challenge(projectless_scratch: &PinnedDirectory) -> Result<ParentChallenge> {
    let directory = projectless_scratch
        .open_child_directory(OsStr::new(PARENT_CHALLENGE_DIRECTORY))?
        .context("verifier parent challenge absent")?;
    let canary = directory
        .open_pinned_regular(OsStr::new("canary"), false)?
        .context("verifier parent canary absent")?;
    let observation = canary.observation()?;
    ensure!(
        observation.size() == 65 && canary.permission_mode()? == 0o600,
        "verifier parent canary has changed shape"
    );
    let bytes = canary.read_stable_bounded(&observation, 65)?;
    let value = std::str::from_utf8(&bytes)?
        .strip_suffix('\n')
        .context("verifier parent canary is not newline terminated")?
        .to_owned();
    let challenge = ParentChallenge {
        directory,
        value,
        observation,
    };
    challenge.recheck()?;
    Ok(challenge)
}

pub fn stage_selected_probe(
    selected: &SelectedInput,
    projectless_scratch: &PinnedDirectory,
    parameters: &Parameters,
) -> Result<StagedNativeProbe> {
    stage_selected_probe_with_challenge(selected, projectless_scratch, parameters, None)
}

/// A descriptor-rooted post-scope read of the one child-authored occurrence.
/// The child's stdout path is checked only after bounded enumeration has
/// selected the unique direct child; it never selects a host path to open.
/// This is a frozen filesystem observation, not proof that Codex received
/// the intended descriptor-backed command environment.
pub struct FrozenScopedOccurrence {
    pub guest_observation: serde_json::Value,
    pub candidate_sha256: String,
    pub environment_configuration_sha256: String,
}

pub fn inspect_frozen_scoped_occurrence(
    selected: &SelectedInput,
    projectless_scratch: &PinnedDirectory,
    parameters: &Parameters,
    reported_path: &str,
) -> Result<FrozenScopedOccurrence> {
    let (_, expected_request) =
        selected.prepare_native_probe_request(projectless_scratch, parameters)?;
    let entries = projectless_scratch.entries_no_follow_bounded(512)?;
    let occurrences = entries
        .iter()
        .filter(|entry| {
            entry
                .name
                .to_str()
                .is_some_and(|name| name.starts_with("verifier-occurrence."))
        })
        .collect::<Vec<_>>();
    ensure!(
        occurrences.len() == 1 && occurrences[0].entry_type == PinnedEntryType::Directory,
        "scoped verifier has no unique direct-child occurrence"
    );
    let occurrence = projectless_scratch
        .open_child_directory(&occurrences[0].name)?
        .context("enumerated scoped verifier occurrence disappeared")?;
    ensure!(
        occurrence.path() == Path::new(reported_path),
        "scoped transcript path differs from independently selected occurrence"
    );
    let guest = occurrence
        .open_child_directory(OsStr::new("guest"))?
        .context("scoped verifier guest disappeared")?;
    let scenario = &parameters.configuration;
    let runtime = guest
        .open_child_directory(OsStr::new("runtime"))?
        .context("scoped verifier runtime disappeared")?;
    let runtime_bin = runtime
        .open_child_directory(OsStr::new("bin"))?
        .context("scoped verifier runtime bin disappeared")?;
    super::exact_member(
        &runtime_bin,
        "codex",
        &scenario.codex_sha256,
        0o755,
        268_435_456,
    )?;
    let controller = occurrence
        .open_child_directory(OsStr::new("controller"))?
        .context("scoped verifier controller disappeared")?;
    let controller_bin = controller
        .open_child_directory(OsStr::new("bin"))?
        .context("scoped verifier controller bin disappeared")?;
    super::exact_member(
        &controller_bin,
        "ryeos-synthetic-routed-guest",
        &scenario.relay_sha256,
        0o755,
        64 * 1024 * 1024,
    )?;
    let tools = guest
        .open_child_directory(OsStr::new("tools"))?
        .context("scoped verifier command tools disappeared")?;
    ensure!(
        ryeos_state::external_content_manifest_digest(
            &ryeos_state::observe_external_content_tree_exact(&tools)?
        )? == scenario.tools_manifest_hash,
        "scoped verifier command tools differ from the signed manifest"
    );
    let request = guest
        .open_pinned_regular(OsStr::new("guest-request.json"), false)?
        .context("scoped verifier request disappeared")?;
    let request_observation = request.observation()?;
    ensure!(
        request.permission_mode()? == 0o600
            && request.read_stable_bounded(&request_observation, 64 * 1024)? == expected_request,
        "scoped verifier request differs from signed selected input"
    );
    let candidate = guest
        .open_child_directory(OsStr::new("candidate"))?
        .context("scoped verifier candidate disappeared")?;
    let candidate_sha256 = check_exact_candidate(&candidate)?;
    let guest_file = guest
        .open_pinned_regular(OsStr::new("guest-observation.json"), false)?
        .context("scoped guest observation disappeared")?;
    let guest_file_observation = guest_file.observation()?;
    let guest_bytes = guest_file.read_stable_bounded(&guest_file_observation, 4 * 1024 * 1024)?;
    let guest_observation = serde_json::from_slice(&guest_bytes)?;
    let home = occurrence
        .open_child_directory(OsStr::new("codex-home"))?
        .context("scoped verifier Codex home disappeared")?;
    super::exact_member(
        &home,
        "config.toml",
        &scenario.scripted_baseline_sha256,
        0o644,
        64 * 1024,
    )?;
    let environment = home
        .open_pinned_regular(OsStr::new("environments.toml"), false)?
        .context("scoped verifier command environment disappeared")?;
    let environment_observation = environment.observation()?;
    ensure!(
        environment.permission_mode()? == 0o644 && environment_observation.size() <= 64 * 1024,
        "scoped verifier command environment changed shape"
    );
    let environment_configuration_sha256 =
        environment.digest_stable_exact(&environment_observation)?;
    projectless_scratch.ensure_path_binding()?;
    occurrence.ensure_path_binding()?;
    guest.ensure_path_binding()?;
    runtime.ensure_path_binding()?;
    runtime_bin.ensure_path_binding()?;
    controller.ensure_path_binding()?;
    controller_bin.ensure_path_binding()?;
    tools.ensure_path_binding()?;
    candidate.ensure_path_binding()?;
    home.ensure_path_binding()?;
    Ok(FrozenScopedOccurrence {
        guest_observation,
        candidate_sha256,
        environment_configuration_sha256,
    })
}

/// Prepare the direct target's two signed writable coordinates. The daemon
/// must later resolve both IDs from this same retained workspace, mount their
/// pinned descriptors, and attest the applied launch. This function does not
/// authorize START or a qualification claim by itself.
pub fn stage_direct_target_probe(
    selected: &SelectedInput,
    projectless_scratch: &PinnedDirectory,
    parameters: &Parameters,
) -> Result<StagedDirectTargetProbe> {
    let (roots, request) =
        selected.prepare_native_probe_request(projectless_scratch, parameters)?;
    let scenario = &parameters.configuration;
    let prepared = projectless_scratch.create_child(OsStr::new("prepared"), 0o700)?;
    let occurrence = prepared.create_child(OsStr::new(DIRECT_OCCURRENCE_ID), 0o700)?;
    let home = prepared.create_child(OsStr::new(DIRECT_HOME_ID), 0o700)?;
    let guest = occurrence.create_child(OsStr::new("guest"), 0o700)?;
    let tools = guest.create_child(OsStr::new("tools"), 0o700)?;
    stage_selected_tools(&roots.tools, &tools, &scenario.tools_manifest_hash)?;
    guest.create_child(OsStr::new("candidate"), 0o700)?;
    let request_sha256 = lillux::sha256_hex(&request);
    guest
        .atomic_create_pinned_regular(OsStr::new("guest-request.json"), &request, 0o600)?
        .context("direct guest request already exists")?;
    copy_exact_member(
        &roots.configurations,
        Path::new("scripted.config.toml"),
        &home,
        OsStr::new("config.toml"),
        &scenario.scripted_baseline_sha256,
        0o644,
        64 * 1024,
    )?;
    // The controller remains an exact read-only selected realization member.
    // A verifier-owned memfd path cannot be used by the daemon-owned target.
    // The selected controller tree is admitted at qualification/controller in
    // the projectless workspace. Its source descriptor path may be relative
    // (the verifier opens "."), whereas Codex needs the fixed absolute path
    // inside its isolated workspace view.
    let controller_executable = Path::new(DIRECT_CONTROLLER_EXECUTABLE);
    let guest_cwd = direct_prepared_destination(DIRECT_OCCURRENCE_ID)?.join("guest");
    let command_environment = materialize_command_environment(
        &roots.configurations,
        &controller_executable,
        &guest_cwd,
        &scenario.command_environment_template_sha256,
    )?;
    let environment_configuration_sha256 = lillux::sha256_hex(&command_environment);
    home.atomic_create_pinned_regular(
        OsStr::new("environments.toml"),
        &command_environment,
        0o644,
    )?
    .context("direct command environment already exists")?;
    let staged = StagedDirectTargetProbe {
        prepared,
        occurrence,
        guest,
        home,
        controller: roots.controller,
        request_sha256,
        environment_configuration_sha256,
    };
    staged.recheck_preflight(parameters)?;
    Ok(staged)
}

/// Reopen the fixed direct-target preparation without creating or replacing
/// anything. The caller must first reconcile the exact retained scoped
/// attempt; this function proves staged inputs, not permission to START again.
pub fn reopen_direct_target_probe(
    selected: &SelectedInput,
    projectless_scratch: &PinnedDirectory,
    parameters: &Parameters,
) -> Result<StagedDirectTargetProbe> {
    let (roots, request) =
        selected.prepare_native_probe_request(projectless_scratch, parameters)?;
    let prepared = projectless_scratch
        .open_child_directory(OsStr::new("prepared"))?
        .context("direct prepared root disappeared")?;
    let occurrence = prepared
        .open_child_directory(OsStr::new(DIRECT_OCCURRENCE_ID))?
        .context("direct occurrence disappeared")?;
    let home = prepared
        .open_child_directory(OsStr::new(DIRECT_HOME_ID))?
        .context("direct Codex home disappeared")?;
    let guest = occurrence
        .open_child_directory(OsStr::new("guest"))?
        .context("direct guest disappeared")?;
    let guest_cwd = direct_prepared_destination(DIRECT_OCCURRENCE_ID)?.join("guest");
    let command_environment = materialize_command_environment(
        &roots.configurations,
        Path::new(DIRECT_CONTROLLER_EXECUTABLE),
        &guest_cwd,
        &parameters.configuration.command_environment_template_sha256,
    )?;
    let staged = StagedDirectTargetProbe {
        prepared,
        occurrence,
        guest,
        home,
        controller: roots.controller,
        request_sha256: lillux::sha256_hex(&request),
        environment_configuration_sha256: lillux::sha256_hex(&command_environment),
    };
    staged.recheck_sealed_inputs(parameters)?;
    Ok(staged)
}

impl StagedDirectTargetProbe {
    pub fn recheck_preflight(&self, parameters: &Parameters) -> Result<()> {
        self.recheck_sealed_inputs(parameters)?;
        let candidate = self
            .guest
            .open_child_directory(OsStr::new("candidate"))?
            .context("direct candidate disappeared")?;
        ensure!(
            candidate.entries_no_follow_bounded(1)?.is_empty(),
            "direct candidate is not empty before START"
        );
        candidate.ensure_path_binding()?;
        Ok(())
    }

    /// Caller must first join the daemon's whole-scope settlement and writer
    /// exclusion. Matching bytes while a target can still write are not a
    /// frozen candidate or an effective-environment claim.
    pub fn inspect_frozen_after_scope_empty(
        &self,
        parameters: &Parameters,
    ) -> Result<FrozenScopedOccurrence> {
        self.recheck_sealed_inputs(parameters)?;
        let candidate = self
            .guest
            .open_child_directory(OsStr::new("candidate"))?
            .context("direct candidate disappeared")?;
        let candidate_sha256 = check_exact_candidate(&candidate)?;
        let guest_file = self
            .guest
            .open_pinned_regular(OsStr::new("guest-observation.json"), false)?
            .context("direct guest observation disappeared")?;
        let observed = guest_file.observation()?;
        ensure!(
            guest_file.permission_mode()? == 0o600 && observed.size() <= 4 * 1024 * 1024,
            "direct guest observation changed shape"
        );
        let guest_observation =
            serde_json::from_slice(&guest_file.read_stable_bounded(&observed, 4 * 1024 * 1024)?)?;
        candidate.ensure_path_binding()?;
        self.guest.ensure_path_binding()?;
        Ok(FrozenScopedOccurrence {
            guest_observation,
            candidate_sha256,
            environment_configuration_sha256: self.environment_configuration_sha256.clone(),
        })
    }

    fn recheck_sealed_inputs(&self, parameters: &Parameters) -> Result<()> {
        let scenario = &parameters.configuration;
        self.prepared.ensure_path_binding()?;
        self.occurrence.ensure_path_binding()?;
        self.guest.ensure_path_binding()?;
        self.home.ensure_path_binding()?;
        self.prepared.require_owner_private_directory()?;
        self.occurrence.require_owner_private_directory()?;
        self.guest.require_owner_private_directory()?;
        self.home.require_owner_private_directory()?;
        self.controller.ensure_path_binding()?;
        super::exact_member(
            &self.controller,
            "bin/ryeos-synthetic-routed-guest",
            &scenario.relay_sha256,
            0o755,
            64 * 1024 * 1024,
        )?;
        let tools = self
            .guest
            .open_child_directory(OsStr::new("tools"))?
            .context("direct command tools disappeared")?;
        ensure!(
            ryeos_state::external_content_manifest_digest(
                &ryeos_state::observe_external_content_tree_exact(&tools)?
            )? == scenario.tools_manifest_hash,
            "direct command tools changed"
        );
        let request = self
            .guest
            .open_pinned_regular(OsStr::new("guest-request.json"), false)?
            .context("direct native request disappeared")?;
        let observation = request.observation()?;
        ensure!(
            request.permission_mode()? == 0o600
                && (1..=64 * 1024).contains(&observation.size())
                && request.digest_stable_exact(&observation)? == self.request_sha256,
            "direct native request changed"
        );
        super::exact_member(
            &self.home,
            "config.toml",
            &scenario.scripted_baseline_sha256,
            0o644,
            64 * 1024,
        )?;
        super::exact_member(
            &self.home,
            "environments.toml",
            &self.environment_configuration_sha256,
            0o644,
            64 * 1024,
        )?;
        tools.ensure_path_binding()?;
        Ok(())
    }

    pub fn environment_configuration_sha256(&self) -> &str {
        &self.environment_configuration_sha256
    }

    pub fn request_sha256(&self) -> &str {
        &self.request_sha256
    }
}

/// Scoped driver variant: use only the parent-authored challenge already
/// retained in this private workspace, while this process owns its own sealed
/// guest executable and cwd descriptors for the Codex launch.
pub fn stage_scoped_driver_probe(
    selected: &SelectedInput,
    projectless_scratch: &PinnedDirectory,
    parameters: &Parameters,
) -> Result<StagedNativeProbe> {
    let challenge = open_parent_challenge(projectless_scratch)?;
    stage_selected_probe_with_challenge(selected, projectless_scratch, parameters, Some(challenge))
}

fn stage_selected_probe_with_challenge(
    selected: &SelectedInput,
    projectless_scratch: &PinnedDirectory,
    parameters: &Parameters,
    challenge: Option<ParentChallenge>,
) -> Result<StagedNativeProbe> {
    let (roots, request) =
        selected.prepare_native_probe_request(projectless_scratch, parameters)?;
    let scenario = &parameters.configuration;
    let (_, occurrence) = projectless_scratch.create_unique_child("verifier-occurrence", 0o700)?;
    let guest = occurrence.create_child(OsStr::new("guest"), 0o700)?;
    let runtime = guest.create_child(OsStr::new("runtime"), 0o700)?;
    let runtime_bin = runtime.create_child(OsStr::new("bin"), 0o700)?;
    copy_exact_member(
        &roots.subject,
        Path::new("bin/codex"),
        &runtime_bin,
        OsStr::new("codex"),
        &scenario.codex_sha256,
        0o755,
        268_435_456,
    )?;
    let tools = guest.create_child(OsStr::new("tools"), 0o700)?;
    stage_selected_tools(&roots.tools, &tools, &scenario.tools_manifest_hash)?;
    let controller = occurrence.create_child(OsStr::new("controller"), 0o700)?;
    let (controller_canary_directory, controller_canary_value, controller_canary_observation) =
        if let Some(challenge) = challenge {
            (challenge.directory, challenge.value, challenge.observation)
        } else {
            let (value, observation) = create_controller_canary(&controller)?;
            (controller.try_clone()?, value, observation)
        };
    let controller_bin = controller.create_child(OsStr::new("bin"), 0o700)?;
    copy_exact_member(
        &roots.controller,
        Path::new("bin/ryeos-synthetic-routed-guest"),
        &controller_bin,
        OsStr::new("ryeos-synthetic-routed-guest"),
        &scenario.relay_sha256,
        0o755,
        64 * 1024 * 1024,
    )?;
    guest.create_child(OsStr::new("candidate"), 0o700)?;
    let request_sha256 = lillux::sha256_hex(&request);
    guest
        .atomic_create_pinned_regular(OsStr::new("guest-request.json"), &request, 0o600)?
        .context("native probe request already exists")?;
    let home = occurrence.create_child(OsStr::new("codex-home"), 0o700)?;
    copy_exact_member(
        &roots.configurations,
        Path::new("scripted.config.toml"),
        &home,
        OsStr::new("config.toml"),
        &scenario.scripted_baseline_sha256,
        0o644,
        64 * 1024,
    )?;
    let controller_executable = controller_bin.path().join("ryeos-synthetic-routed-guest");
    let staged_guest_executable = controller_bin
        .open_pinned_regular(OsStr::new("ryeos-synthetic-routed-guest"), false)?
        .context("staged guest executable disappeared")?;
    let guest_observation = staged_guest_executable.observation()?;
    let guest_bytes =
        staged_guest_executable.read_stable_bounded(&guest_observation, 64 * 1024 * 1024)?;
    ensure!(
        lillux::sha256_hex(&guest_bytes) == scenario.relay_sha256,
        "staged guest executable changed before sealing"
    );
    let guest_executable_authority =
        lillux::sealed_executable_memfd(c"ryeos-verifier-guest", &guest_bytes)
            .map_err(anyhow::Error::msg)?;
    let guest_cwd_authority = guest.try_clone()?.into_inherited_descriptor_path()?;
    let command_environment = materialize_command_environment(
        &roots.configurations,
        guest_executable_authority.path(),
        guest_cwd_authority.path(),
        &scenario.command_environment_template_sha256,
    )?;
    let environment_configuration_sha256 = lillux::sha256_hex(&command_environment);
    home.atomic_create_pinned_regular(
        OsStr::new("environments.toml"),
        &command_environment,
        0o644,
    )?
    .context("rendered command environment already exists")?;
    roots.subject.ensure_path_binding()?;
    roots.controller.ensure_path_binding()?;
    roots.tools.ensure_path_binding()?;
    let staged = StagedNativeProbe {
        codex_executable: runtime_bin.path().join("codex"),
        controller_executable,
        guest_executable_authority,
        guest_cwd_authority,
        occurrence,
        guest,
        home,
        request_sha256,
        environment_configuration_sha256,
        controller_canary_directory,
        controller_canary_value,
        controller_canary_observation,
    };
    staged.recheck_preflight(parameters)?;
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_prepared_paths_are_fixed_namespace_coordinates() {
        assert_eq!(
            DIRECT_CONTROLLER_CANARY,
            "/workspace/verifier-controller/canary"
        );
        assert_eq!(
            Path::new(DIRECT_CONTROLLER_EXECUTABLE),
            Path::new("/workspace/qualification/controller/bin/ryeos-synthetic-routed-guest")
        );
        assert_eq!(
            direct_prepared_destination(DIRECT_HOME_ID).unwrap(),
            Path::new("/ryeos/producer-prepared/codex-home")
        );
        assert_eq!(
            direct_prepared_destination(DIRECT_OCCURRENCE_ID).unwrap(),
            Path::new("/ryeos/producer-prepared/codex-occurrence")
        );
        assert!(direct_prepared_destination("../outside").is_err());
    }

    #[test]
    fn selected_tools_stage_exact_manifest_and_refuse_drift() {
        let temp = tempfile::tempdir().unwrap();
        let root = PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let source = root.create_child(OsStr::new("source"), 0o700).unwrap();
        let bin = source.create_child(OsStr::new("bin"), 0o755).unwrap();
        bin.atomic_create_pinned_regular(OsStr::new("zsh"), b"signed-shell", 0o755)
            .unwrap();
        bin.atomic_create_pinned_regular(OsStr::new("rg"), b"signed-search", 0o755)
            .unwrap();
        let target = root.create_child(OsStr::new("target"), 0o700).unwrap();
        let expected = ryeos_state::external_content_manifest_digest(
            &ryeos_state::observe_external_content_tree_exact(&source).unwrap(),
        )
        .unwrap();
        stage_selected_tools(&source, &target, &expected).unwrap();
        assert_eq!(
            ryeos_state::external_content_manifest_digest(
                &ryeos_state::observe_external_content_tree_exact(&target).unwrap()
            )
            .unwrap(),
            expected
        );
        assert!(stage_selected_tools(&source, &target, &expected).is_err());
        assert!(stage_selected_tools(&source, &root, &"0".repeat(64)).is_err());
    }

    #[test]
    fn parent_challenge_is_exact_shared_private_authority() {
        let temp = tempfile::tempdir().unwrap();
        let root = PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let parent = create_parent_challenge(&root).unwrap();
        let child = open_parent_challenge(&root).unwrap();
        assert_eq!(parent.value(), child.value());
        assert_eq!(parent.canary_path(), child.canary_path());
        parent.recheck().unwrap();
        child.recheck().unwrap();
        let endpoint = parent
            .publish_provider_endpoint(OsStr::new("provider-exact.sock"))
            .unwrap();
        endpoint.recheck().unwrap();
        assert!(
            parent
                .publish_provider_endpoint(OsStr::new("provider-other.sock"))
                .is_err()
        );
        assert!(create_parent_challenge(&root).is_err());
    }

    #[test]
    fn frozen_candidate_requires_exact_single_regular_file_and_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let candidate = PinnedDirectory::open(temp.path()).unwrap().unwrap();
        assert!(check_exact_candidate(&candidate).is_err());
        candidate
            .atomic_create_pinned_regular(
                OsStr::new(crate::scripted_provider::CANDIDATE_RELATIVE_PATH),
                crate::scripted_provider::CANDIDATE_CONTENT.as_bytes(),
                0o644,
            )
            .unwrap();
        assert_eq!(
            check_exact_candidate(&candidate).unwrap(),
            lillux::sha256_hex(crate::scripted_provider::CANDIDATE_CONTENT.as_bytes())
        );
        candidate
            .atomic_create_pinned_regular(OsStr::new("extra"), b"x", 0o644)
            .unwrap();
        assert!(check_exact_candidate(&candidate).is_err());
    }

    #[test]
    fn frozen_candidate_rejects_wrong_mode_and_content() {
        for (content, mode) in [
            (
                crate::scripted_provider::CANDIDATE_CONTENT.as_bytes(),
                0o600,
            ),
            (b"substituted candidate\n".as_slice(), 0o644),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let candidate = PinnedDirectory::open(temp.path()).unwrap().unwrap();
            candidate
                .atomic_create_pinned_regular(
                    OsStr::new(crate::scripted_provider::CANDIDATE_RELATIVE_PATH),
                    content,
                    mode,
                )
                .unwrap();
            assert!(check_exact_candidate(&candidate).is_err());
        }
    }

    #[test]
    fn controller_canary_is_real_private_and_checked_again_after_execution() {
        let temp = tempfile::tempdir().unwrap();
        let controller = PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let (value, original) = create_controller_canary(&controller).unwrap();
        assert!(lillux::valid_hash(&value));
        assert_eq!(
            std::fs::read_to_string(temp.path().join("canary")).unwrap(),
            format!("{value}\n")
        );
        check_controller_canary(&controller, &value, &original).unwrap();
        assert!(check_controller_canary(&controller, &"0".repeat(64), &original).is_err());
        std::fs::write(temp.path().join("canary"), "0".repeat(64)).unwrap();
        assert!(check_controller_canary(&controller, &value, &original).is_err());
        std::fs::remove_file(temp.path().join("canary")).unwrap();
        std::fs::write(temp.path().join("canary"), &value).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                temp.path().join("canary"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }
        assert!(check_controller_canary(&controller, &value, &original).is_err());
    }

    #[test]
    fn scripted_canary_commands_target_the_real_path_and_refuse_ambiguous_paths() {
        let path = Path::new("/private/occurrence-1/controller/canary");
        let commands = scripted_canary_commands(path).unwrap();
        assert!(
            commands
                .forbidden_local_write
                .contains(path.to_str().unwrap())
        );
        assert!(commands.guest_read.contains(path.to_str().unwrap()));
        assert!(commands.guest_read.contains(CONTROLLER_CANARY_DENIAL));
        assert!(scripted_canary_commands(Path::new("relative/canary")).is_err());
        assert!(scripted_canary_commands(Path::new("/private/space here/canary")).is_err());
    }

    #[test]
    fn readable_canary_cannot_masquerade_as_denied_shell_read() {
        let temp = tempfile::tempdir().unwrap();
        let controller = PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let (value, _) = create_controller_canary(&controller).unwrap();
        let canary = temp.path().join("canary");
        let commands = scripted_canary_commands(&canary).unwrap();
        let run = |script: &str| {
            lillux::run(lillux::SubprocessRequest {
                cmd: "/bin/sh".into(),
                argv0: None,
                args: vec!["-c".into(), script.into()],
                cwd: None,
                envs: vec![("PATH".into(), String::new())],
                stdin_data: None,
                timeout: 5.0,
                limits: None,
                inherited_fds: vec![],
                inherited_fd_mappings: vec![],
                supervised_status: None,
            })
        };
        let readable = run(&commands.guest_read);
        assert!(readable.success);
        assert_eq!(readable.stdout, value);
        assert!(!readable.stdout.contains(CONTROLLER_CANARY_DENIAL));
        let absent = scripted_canary_commands(&temp.path().join("absent")).unwrap();
        let denied = run(&absent.guest_read);
        assert!(!denied.success);
        assert_eq!(denied.exit_code, 73);
        assert_eq!(denied.stdout, CONTROLLER_CANARY_DENIAL);
    }

    #[test]
    fn signed_command_template_rejects_extra_routes_and_local_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let configurations = temp.path().join("configurations");
        let guest = temp.path().join("guest");
        std::fs::create_dir(&configurations).unwrap();
        std::fs::create_dir(&guest).unwrap();
        let template_path = configurations.join("environments.toml.template");
        let configurations = PinnedDirectory::open(&configurations).unwrap().unwrap();
        let guest = PinnedDirectory::open(&guest).unwrap().unwrap();
        let program = temp.path().join("controller");
        for invalid in [
            COMMAND_ENVIRONMENT_TEMPLATE.replace("include_local = false", "include_local = true"),
            format!(
                "{COMMAND_ENVIRONMENT_TEMPLATE}\n[[environments]]\nid = \"ambient\"\nprogram = \"/bin/sh\"\n"
            ),
            COMMAND_ENVIRONMENT_TEMPLATE.replace("env = {}", "env = { TOKEN = \"ambient\" }"),
        ] {
            std::fs::write(&template_path, &invalid).unwrap();
            assert!(
                materialize_command_environment(
                    &configurations,
                    &program,
                    guest.path(),
                    &lillux::sha256_hex(invalid.as_bytes()),
                )
                .is_err()
            );
        }
        std::fs::write(&template_path, COMMAND_ENVIRONMENT_TEMPLATE).unwrap();
        let output = materialize_command_environment(
            &configurations,
            &program,
            guest.path(),
            &lillux::sha256_hex(COMMAND_ENVIRONMENT_TEMPLATE.as_bytes()),
        )
        .unwrap();
        let parsed: toml::Value = std::str::from_utf8(&output).unwrap().parse().unwrap();
        assert_eq!(parsed["include_local"].as_bool(), Some(false));
        assert_eq!(parsed["environments"].as_array().unwrap().len(), 1);
        assert_eq!(
            parsed["environments"][0]["program"].as_str(),
            program.to_str()
        );
        assert_eq!(
            parsed["environments"][0]["cwd"].as_str(),
            guest.path().to_str()
        );
        assert_eq!(
            parsed["environments"][0]["env"].as_table().unwrap().len(),
            0
        );
    }
}
