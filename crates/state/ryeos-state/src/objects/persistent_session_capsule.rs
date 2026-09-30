//! Immutable admission authority for a reusable callback-free subprocess.
//!
//! This object is deliberately domain-neutral.  It retains an exact effective
//! program, direct execution closure, framed transport contract, and execution
//! realization.  The daemon may pool a process admitted by this capsule; the
//! owning adapter assigns meaning to request and response bodies.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use super::{
    AdmittedExecutionClosure, AdmittedLaunchArtifactIdentity, DirectExecutableIdentity,
    ExecutionLaunchDriver, validate_trimmed_control_free,
};

pub const PERSISTENT_SESSION_CAPSULE_KIND: &str = "persistent_session_capsule";
// v7 requires direct-plan filesystem/network ceilings and explicit logical
// mount roots in retained realizations. Predecessors are not launch authority.
// v8 also requires exact evidence-attachment bindings in the retained program.
// A v7 program omits that identity and cannot be reinterpreted during recovery.
// v9 separates exact retained product redemption proof from program identity.
// v10 requires prepared session-environment delivery. Earlier retained bridges
// consume an incompatible raw map and cannot be launched with this envelope.
// v11 places enforced typed-entry session source in the execution runtime,
// not the project namespace. Do not recover an older capsule with changed
// workload-visible source paths and project-shadow semantics.
// v12 retains the complete signed auxiliary configuration inventory. A prior
// capsule cannot authorize preparing these additional profile-home files.
// v13 additionally retains the exact immutable namespace configuration inventory.
// v14 retains external candidate program requirements separately from placement authority.
// v16 requires endpoint intent/selection in the retained ordinary provider
// process plan; an external command environment is not provider placement.
// v17 separately owns the exact guest-runtime product proof. The candidate
// executable product selection is not a substitute for placement qualification.
// v18 also binds the published product owner's principal, so retained proof
// authentication never guesses the owner from a later node configuration.
pub const PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION: u32 = 20;
pub const MAX_EXECUTABLE_SEARCH_PATH_ENTRIES: usize = 32;
pub const MAX_SESSION_PROCESS_ENVIRONMENT_ENTRIES: usize = 32;
pub const MAX_SESSION_PROCESS_ENVIRONMENT_ENCODED_BYTES: usize = 4_096;
pub const SESSION_PROCESS_ENVIRONMENT_ENV: &str = "RYEOS_SESSION_PROCESS_ENVIRONMENT";

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}
pub const MAX_PERSISTENT_SESSION_EXACT_PROGRAM_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSubjectProjectionContract {
    pub schema: u32,
    pub contract: String,
    pub json_pointers: Vec<String>,
}

impl CredentialSubjectProjectionContract {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 || self.contract.is_empty() || self.contract.len() > 256 {
            anyhow::bail!("credential-subject projection has an invalid wire identity");
        }
        validate_trimmed_control_free(
            "credential-subject projection contract",
            &self.contract,
            false,
        )?;
        if self.json_pointers.is_empty() || self.json_pointers.len() > 32 {
            anyhow::bail!("credential-subject projection has an invalid pointer count");
        }
        let mut prior: Option<&str> = None;
        for pointer in &self.json_pointers {
            if pointer.len() > 1024
                || !pointer.starts_with('/')
                || pointer.chars().any(char::is_control)
                || prior.is_some_and(|prior| prior >= pointer.as_str())
            {
                anyhow::bail!("credential-subject pointers are not canonical and ordered");
            }
            let mut decoded = pointer.split('/').skip(1);
            if decoded.clone().any(|segment| {
                segment.is_empty()
                    || segment
                        .as_bytes()
                        .windows(2)
                        .any(|pair| pair[0] == b'~' && !matches!(pair[1], b'0' | b'1'))
                    || (segment.ends_with('~')
                        && !segment.ends_with("~0")
                        && !segment.ends_with("~1"))
            }) {
                anyhow::bail!("credential-subject projection contains an invalid JSON pointer");
            }
            let _ = decoded.next();
            prior = Some(pointer);
        }
        Ok(())
    }

    pub fn contract_digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(self)?)?.as_bytes(),
        ))
    }

    pub fn derive_subject_digest(&self, sanitized_account: &Value) -> anyhow::Result<String> {
        self.validate()?;
        if !sanitized_account.is_object() {
            anyhow::bail!("credential subject requires a sanitized account object");
        }
        let fields = self
            .json_pointers
            .iter()
            .map(|pointer| {
                let value = sanitized_account.pointer(pointer).cloned().ok_or_else(|| {
                    anyhow::anyhow!(
                        "sanitized account is missing stable credential-subject field {pointer}"
                    )
                })?;
                Ok(serde_json::json!({"pointer": pointer, "value": value}))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let projection = serde_json::json!({
            "schema": 1,
            "domain": "ryeos.credential_subject.v1",
            "contract": self.contract,
            "projection_contract_digest": self.contract_digest()?,
            "fields": fields,
        });
        let canonical = lillux::canonical_json(&projection)?;
        Ok(lillux::sha256_hex(
            &[
                b"ryeos.credential_subject.v1\0".as_slice(),
                canonical.as_bytes(),
            ]
            .concat(),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PortableSessionStateClass {
    PortableSessionState,
    NodePrivateCredentialState,
    RebuildableCache,
    ForbiddenOrUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableSessionStateSelector {
    /// Canonical profile-home-relative pattern. `*` matches bytes inside one
    /// path segment, an entire `**` segment matches zero or more segments, and
    /// `{session_id}` is replaced by the exact upstream session identity and
    /// never acts as a glob.
    pub pattern: String,
    pub class: PortableSessionStateClass,
    pub max_matches: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortableSessionStateContract {
    pub schema: u32,
    pub restore_contract: String,
    pub max_depth: u16,
    pub max_entries: u32,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub selectors: Vec<PortableSessionStateSelector>,
}

impl PortableSessionStateContract {
    /// Configuration is restored from signed source, never from a portable
    /// session attachment or a rebuildable cache classification.
    pub fn validate_configuration_exclusions(
        &self,
        configs: &[SessionConfigurationFile],
    ) -> anyhow::Result<()> {
        for config in configs {
            self.validate_configuration_destination_exclusion(
                &config.destination,
                "auxiliary configuration",
            )?;
        }
        Ok(())
    }

    /// Generated private configuration has the same capture prohibition as
    /// signed auxiliary configuration, but no portable source file. The exact
    /// destination is committed by the external-candidate requirement and
    /// rejoined to the installed provider declaration before materialization.
    pub fn validate_generated_configuration_exclusion(
        &self,
        destination: &str,
    ) -> anyhow::Result<()> {
        validate_session_configuration_destination(destination)?;
        self.validate_configuration_destination_exclusion(
            destination,
            "generated private configuration",
        )
    }

    fn validate_configuration_destination_exclusion(
        &self,
        destination: &str,
        label: &str,
    ) -> anyhow::Result<()> {
        if !self.selectors.iter().any(|selector| {
            selector.pattern == destination
                && selector.class == PortableSessionStateClass::ForbiddenOrUnknown
        }) {
            anyhow::bail!("{label} requires an exact forbidden portable-state selector");
        }
        for selector in &self.selectors {
            // '*' stands for any safe session identity here. The selector
            // matcher is used only to detect potential overlap, not to select
            // or restore a real session.
            if selector.class == PortableSessionStateClass::PortableSessionState
                && super::portable_state_selector_matches(selector, destination, "*")?
            {
                anyhow::bail!("{label} overlaps portable session state");
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1
            || self.restore_contract != "ryeos.worker_session.restore.v1"
            || self.max_depth == 0
            || self.max_depth > 64
            || self.max_entries == 0
            || self.max_entries > 100_000
            || self.max_file_bytes == 0
            || self.max_file_bytes > 16 * 1024 * 1024
            || self.max_total_bytes == 0
            || self.max_total_bytes > 16 * 1024 * 1024
            || self.max_file_bytes > self.max_total_bytes
            || self.selectors.is_empty()
            || self.selectors.len() > 64
        {
            anyhow::bail!("portable-session state contract is outside substrate bounds");
        }
        let mut patterns = BTreeSet::new();
        let mut prior: Option<&str> = None;
        let mut portable = 0usize;
        for selector in &self.selectors {
            validate_portable_state_pattern(&selector.pattern)?;
            if !patterns.insert(selector.pattern.as_str())
                || prior.is_some_and(|value| value >= selector.pattern.as_str())
                || selector.max_matches == 0
                || selector.max_matches > self.max_entries
            {
                anyhow::bail!("portable-session state selectors are not canonical and bounded");
            }
            prior = Some(&selector.pattern);
            let placeholder_count = selector.pattern.matches("{session_id}").count();
            if selector.class == PortableSessionStateClass::PortableSessionState {
                portable += 1;
                if placeholder_count != 1
                    || selector.max_matches != 1
                    || selector.pattern.split('/').any(|segment| segment == "**")
                {
                    anyhow::bail!(
                        "portable session selector must bind one exact session and one file"
                    );
                }
            } else if placeholder_count != 0 {
                anyhow::bail!("non-portable state classifiers cannot depend on a session identity");
            }
        }
        if portable == 0 {
            anyhow::bail!("portable-session contract has no portable state selector");
        }
        Ok(())
    }
}

fn validate_portable_state_pattern(pattern: &str) -> anyhow::Result<()> {
    if pattern.is_empty()
        || pattern.len() > 1024
        || pattern.starts_with('/')
        || pattern.ends_with('/')
        || pattern.chars().any(char::is_control)
    {
        anyhow::bail!("portable-session state pattern is not a bounded relative path");
    }
    for segment in pattern.split('/') {
        if segment.is_empty()
            || matches!(segment, "." | "..")
            || (segment.contains("**") && segment != "**")
            || segment.contains('{') != segment.contains("{session_id}")
            || segment.replace("{session_id}", "").contains(['{', '}'])
            || segment.bytes().any(|byte| {
                !(byte.is_ascii_alphanumeric()
                    || matches!(byte, b'.' | b'_' | b'-' | b'*' | b'{' | b'}'))
            })
        {
            anyhow::bail!("portable-session state pattern contains an unsafe segment");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionLifecycleContract {
    pub max_processes: u16,
    pub max_inflight_per_process: u16,
    pub max_address_space_bytes: u64,
    pub max_cpu_seconds: u64,
    /// Kernel RLIMIT_NPROC ceiling for the admitted process's real UID.
    pub real_uid_process_limit: u64,
    pub ready_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub idle_timeout_ms: u64,
}

impl PersistentSessionLifecycleContract {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_processes == 0
            || self.max_processes > 64
            || self.max_inflight_per_process != 1
            || self.max_address_space_bytes < 64 * 1024 * 1024
            || self.max_address_space_bytes > 1024 * 1024 * 1024 * 1024
            || self.max_cpu_seconds == 0
            || self.max_cpu_seconds > 7 * 24 * 60 * 60
            || self.real_uid_process_limit == 0
            || self.real_uid_process_limit > 4096
            || self.ready_timeout_ms == 0
            || self.ready_timeout_ms > 10 * 60 * 1000
            || self.request_timeout_ms == 0
            || self.request_timeout_ms > 60 * 60 * 1000
            || self.idle_timeout_ms == 0
            || self.idle_timeout_ms > 24 * 60 * 60 * 1000
        {
            anyhow::bail!("persistent-session lifecycle contract is outside substrate bounds");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionWireContract {
    pub channel_env: String,
    pub wire_protocol: String,
    pub wire_version: u32,
    pub max_frame_bytes: u32,
}

impl PersistentSessionWireContract {
    pub fn validate(&self) -> anyhow::Result<()> {
        let valid_env = !self.channel_env.is_empty()
            && self.channel_env.len() <= 128
            && self.channel_env.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_uppercase() || (index != 0 && byte.is_ascii_digit())
            });
        validate_trimmed_control_free(
            "persistent-session wire protocol",
            &self.wire_protocol,
            false,
        )?;
        if !valid_env
            || self.wire_protocol.len() > 128
            || self.wire_version == 0
            || self.max_frame_bytes == 0
            || self.max_frame_bytes > 16 * 1024 * 1024
        {
            anyhow::bail!("persistent-session wire contract is not canonical and bounded");
        }
        Ok(())
    }
}

/// Fields shared by the capsule and its admitted execution realization.
/// Keeping this projection explicit avoids a content-addressed cycle: the
/// realization commits this digest, while the final capsule points to the
/// realization object.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionAuthority {
    pub external_candidate:
        Option<crate::external_execution::admission::AdmittedExternalCandidateProgram>,
    pub exact_program_hash: String,
    pub lifecycle: PersistentSessionLifecycleContract,
    pub wire: PersistentSessionWireContract,
    pub artifact_identity: AdmittedLaunchArtifactIdentity,
    pub execution_closure: AdmittedExecutionClosure,
    pub runtime_ref: String,
    pub executor_ref: String,
}

/// One ordered, logical executable-search entry. The capsule retains
/// realization identities rather than host paths; materialization resolves
/// them only from the capsule's exact external-realization set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableSearchPathEntry {
    pub realization_id: String,
    pub relative_directory: String,
}

impl ExecutableSearchPathEntry {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.realization_id.is_empty()
            || self.realization_id.len() > 64
            || !self.realization_id.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
        {
            anyhow::bail!("executable-search realization id is not canonical");
        }
        if self.relative_directory != "." {
            super::validate_canonical_project_relative_path(&self.relative_directory)
                .map_err(|error| anyhow::anyhow!("executable-search directory: {error}"))?;
        }
        Ok(())
    }
}

/// One path-free environment value retained in the exact session capsule.
/// Path variants name only authorities already owned by the launch: an exact
/// pinned realization or the daemon-owned runtime view below the workspace's
/// non-bypassable capture floor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionProcessEnvironmentValue {
    Literal {
        value: String,
    },
    RealizationPath {
        realization_id: String,
        relative_path: String,
        path_kind: SessionProcessEnvironmentPathKind,
    },
    RuntimeViewDirectory {
        relative_path: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionProcessEnvironmentPathKind {
    File,
    Directory,
}

/// Operational delivery of the retained environment, prepared by the launch
/// owner. This is not an authored Config value or additional capsule authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedSessionProcessEnvironment {
    pub bindings: BTreeMap<String, SessionProcessEnvironmentValue>,
    pub runtime_view_delivery: SessionRuntimeViewDelivery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionRuntimeViewDelivery {
    DescriptorWorkspace,
    MountedNamespace {
        destinations: BTreeMap<String, std::path::PathBuf>,
    },
}

pub const SESSION_RUNTIME_VIEWS_ROOT: &str = "/ryeos/runtime-views";
pub const SESSION_RUNTIME_ENDPOINTS_ROOT: &str = "/run/ryeos/session-endpoints";
const MAX_SESSION_ENVIRONMENT_NAME_BYTES: usize = 128;
// Preserve the authored-map bound while accounting for one derived destination
// per entry, including both name spellings and fixed JSON envelope overhead.
pub const MAX_PREPARED_SESSION_PROCESS_ENVIRONMENT_BYTES: usize =
    MAX_SESSION_PROCESS_ENVIRONMENT_ENCODED_BYTES
        + MAX_SESSION_PROCESS_ENVIRONMENT_ENTRIES
            * (2 * MAX_SESSION_ENVIRONMENT_NAME_BYTES + SESSION_RUNTIME_VIEWS_ROOT.len() + 16)
        + 128;

pub fn runtime_view_mount_destination(name: &str) -> anyhow::Result<std::path::PathBuf> {
    validate_session_process_environment_name(name)?;
    Ok(std::path::Path::new(SESSION_RUNTIME_VIEWS_ROOT).join(name))
}

/// Derive one controller-owned, occurrence-private endpoint coordinate in the
/// isolated session namespace. The caller supplies only a canonical logical
/// name; neither authored content nor provider input can select an ambient
/// host path. The fixed namespace root also keeps local IPC addresses short
/// independently of the retained state-root spelling.
pub fn session_runtime_endpoint_destination(name: &str) -> anyhow::Result<std::path::PathBuf> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        anyhow::bail!("session runtime endpoint name is not canonical");
    }
    Ok(std::path::Path::new(SESSION_RUNTIME_ENDPOINTS_ROOT).join(name))
}

impl PreparedSessionProcessEnvironment {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_session_process_environment(&self.bindings)?;
        if let SessionRuntimeViewDelivery::MountedNamespace { destinations } =
            &self.runtime_view_delivery
        {
            let expected = self
                .bindings
                .iter()
                .filter(|(_, value)| {
                    matches!(
                        value,
                        SessionProcessEnvironmentValue::RuntimeViewDirectory { .. }
                    )
                })
                .map(|(name, _)| Ok((name.clone(), runtime_view_mount_destination(name)?)))
                .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
            if destinations.len() != expected.len()
                || expected.iter().any(|(name, path)| {
                    destinations.get(name).map(|value| value.as_os_str()) != Some(path.as_os_str())
                })
            {
                anyhow::bail!(
                    "prepared runtime-view mounts must exactly cover the retained bindings"
                );
            }
        }
        if serde_json::to_vec(self)?.len() > MAX_PREPARED_SESSION_PROCESS_ENVIRONMENT_BYTES {
            anyhow::bail!("prepared session process environment exceeds its encoded byte bound");
        }
        Ok(())
    }
}

pub fn validate_session_process_environment(
    environment: &BTreeMap<String, SessionProcessEnvironmentValue>,
) -> anyhow::Result<()> {
    if environment.len() > MAX_SESSION_PROCESS_ENVIRONMENT_ENTRIES {
        anyhow::bail!("session process environment exceeds its entry bound");
    }
    for (name, value) in environment {
        validate_session_process_environment_name(name)?;
        match value {
            SessionProcessEnvironmentValue::Literal { value } => {
                if value.len() > 4096 || value.chars().any(char::is_control) {
                    anyhow::bail!("session process environment literal is not bounded");
                }
            }
            SessionProcessEnvironmentValue::RealizationPath {
                realization_id,
                relative_path,
                ..
            } => {
                if realization_id.is_empty()
                    || realization_id.len() > 64
                    || !realization_id.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'_' | b'-')
                    })
                {
                    anyhow::bail!("session process environment realization id is not canonical");
                }
                validate_session_process_environment_relative_path(relative_path)?;
            }
            SessionProcessEnvironmentValue::RuntimeViewDirectory { relative_path } => {
                validate_session_process_environment_relative_path(relative_path)?;
            }
        }
    }
    let encoded = serde_json::to_vec(environment)?;
    if encoded.len() > MAX_SESSION_PROCESS_ENVIRONMENT_ENCODED_BYTES {
        anyhow::bail!(
            "session process environment exceeds its encoded byte bound of {}",
            MAX_SESSION_PROCESS_ENVIRONMENT_ENCODED_BYTES
        );
    }
    Ok(())
}

pub fn validate_session_process_environment_name(name: &str) -> anyhow::Result<()> {
    let mut bytes = name.bytes();
    if name.is_empty()
        || name.len() > MAX_SESSION_ENVIRONMENT_NAME_BYTES
        || !bytes
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_uppercase())
        || !bytes.all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
        || matches!(
            name,
            "PATH"
                | "HOME"
                | "USER"
                | "SHELL"
                | "TERM"
                | "LANG"
                | "LC_ALL"
                | "PWD"
                | "OLDPWD"
                | "BASH_ENV"
                | "ENV"
                | "PYTHONHOME"
                | "PYTHONPATH"
        )
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("RYEOS_")
        || name.starts_with("RYEOSD_")
        || name.starts_with("RUST_")
    {
        anyhow::bail!("session process environment contains a protected or invalid name");
    }
    Ok(())
}

pub fn validate_session_process_environment_relative_path(path: &str) -> anyhow::Result<()> {
    if path != "." {
        super::validate_canonical_project_relative_path(path)
            .map_err(|error| anyhow::anyhow!("session process environment path: {error}"))?;
    }
    Ok(())
}

impl PersistentSessionAuthority {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(program) = &self.external_candidate {
            program.validate()?;
        }
        super::thread_snapshot::validate_canonical_hash(
            "persistent-session exact program hash",
            &self.exact_program_hash,
        )?;
        self.lifecycle.validate()?;
        self.wire.validate()?;
        self.artifact_identity.validate()?;
        self.execution_closure.validate()?;
        if self.artifact_identity.launch_driver() != ExecutionLaunchDriver::DirectItemExecutor
            || self.execution_closure.launch_driver() != ExecutionLaunchDriver::DirectItemExecutor
        {
            anyhow::bail!("persistent session must retain a direct-item execution closure");
        }
        if self.artifact_identity.executor_ref() != self.executor_ref {
            anyhow::bail!("persistent-session artifact identity contradicts executor ref");
        }
        if matches!(
            self.artifact_identity,
            AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                executable_identity: DirectExecutableIdentity::NodePolicy,
                ..
            }
        ) {
            anyhow::bail!("persistent session cannot execute a mutable node-policy command");
        }
        validate_trimmed_control_free("persistent-session runtime ref", &self.runtime_ref, false)?;
        validate_trimmed_control_free(
            "persistent-session executor ref",
            &self.executor_ref,
            false,
        )?;
        Ok(())
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(self)?)?.as_bytes(),
        ))
    }

    pub fn artifact_identity_digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(&self.artifact_identity)?)?.as_bytes(),
        ))
    }

    pub fn execution_closure_digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(&self.execution_closure)?)?.as_bytes(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedExternalRuntimeQualification {
    pub binding_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub owner_principal: String,
    pub proof: crate::external_content::products::composition::AdmittedProductQualification,
}

impl RetainedExternalRuntimeQualification {
    pub fn validate(&self) -> anyhow::Result<()> {
        super::thread_snapshot::validate_canonical_hash(
            "external runtime binding",
            &self.binding_hash,
        )?;
        super::thread_snapshot::validate_canonical_hash(
            "external guest runtime manifest",
            &self.guest_runtime_manifest_hash,
        )?;
        super::thread_snapshot::validate_canonical_hash(
            "external runtime qualification attestation",
            &self.proof.attestation_hash,
        )?;
        let owner = self.owner_principal.strip_prefix("fp:").ok_or_else(|| {
            anyhow::anyhow!("external runtime qualification owner must be a fingerprint principal")
        })?;
        super::thread_snapshot::validate_canonical_hash("external runtime owner", owner)?;
        self.proof.evidence.validate()?;
        if self.proof.evidence.result.subject_manifest_hash != self.guest_runtime_manifest_hash {
            anyhow::bail!("retained external runtime proof differs from guest manifest");
        }
        Ok(())
    }
}

/// Historical session-runtime compatibility testimony, not guest-owner
/// snapshot qualification or a renewed permission for provider contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedExternalRuntimeContentQualification {
    pub binding_hash: String,
    pub runtime_manifest_hash: String,
    pub activation_ref: String,
    pub coordinate_id: String,
    pub attestation_hash: String,
    pub evidence: crate::external_content::qualification_evidence::ContentQualificationEvidence,
}

impl RetainedExternalRuntimeContentQualification {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (label, hash) in [
            ("runtime content binding", &self.binding_hash),
            ("runtime content manifest", &self.runtime_manifest_hash),
            ("runtime content coordinate", &self.coordinate_id),
            ("runtime content attestation", &self.attestation_hash),
        ] {
            super::thread_snapshot::validate_canonical_hash(label, hash)?;
        }
        crate::external_content::products::validate_canonical_unsuffixed_ref(&self.activation_ref)?;
        if !self.activation_ref.starts_with("config:") {
            anyhow::bail!("retained runtime content requires an activation config ref");
        }
        self.evidence.validate()?;
        if self.evidence.result.subject_manifest_hash != self.runtime_manifest_hash
            || crate::external_content::qualification_publication::coordinate_id(&self.evidence)?
                != self.coordinate_id
        {
            anyhow::bail!("retained session-runtime content differs from its exact testimony");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedPersistentSessionCapsule {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub external_candidate:
        Option<crate::external_execution::admission::AdmittedExternalCandidateProgram>,
    pub schema: u32,
    pub kind: String,
    pub exact_program: Value,
    pub exact_program_hash: String,
    /// Full product authority retained independently of the semantic program.
    /// Its exact projection must match the program before any reconstruction.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub retained_product_selections:
        Option<crate::external_content::products::composition::ResolvedExternalProductSelections>,
    /// Separate CAS-owned proof of the placement guest runtime, if applicable.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub retained_external_runtime_qualification: Option<RetainedExternalRuntimeQualification>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub retained_external_runtime_content_qualification:
        Option<RetainedExternalRuntimeContentQualification>,
    pub lifecycle: PersistentSessionLifecycleContract,
    pub wire: PersistentSessionWireContract,
    pub artifact_identity: AdmittedLaunchArtifactIdentity,
    pub execution_closure: AdmittedExecutionClosure,
    pub execution_realization_hash: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub source_binding_hash: Option<String>,
    /// Admission-compiled identity for the closed structured-session protocol
    /// family. Other persistent-session protocol families retain `null`.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub structured_session_profile: Option<AdmittedStructuredSessionProfile>,
    /// Ordered search path compiled from signed content dependencies. This is
    /// a logical realization-relative contract, never an ambient host PATH.
    pub executable_search: Vec<ExecutableSearchPathEntry>,
    /// Environment bindings compiled from signed launch contributions. No
    /// absolute host path is retained in the capsule.
    pub process_environment: BTreeMap<String, SessionProcessEnvironmentValue>,
    pub runtime_ref: String,
    pub executor_ref: String,
}

fn restore_persistent_product_selections(
    exact_program: &Value,
    retained: Option<
        &crate::external_content::products::composition::ResolvedExternalProductSelections,
    >,
) -> anyhow::Result<Value> {
    use crate::external_content::products::composition::EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY;
    let slot = exact_program
        .get("resolution_output")
        .and_then(|v| v.get("composed"))
        .and_then(|v| v.get("derived"))
        .and_then(|v| v.get(EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY));
    let mut restored = exact_program.clone();
    match (retained, slot) {
        (None, None) => {}
        (Some(retained), Some(semantic)) => {
            if retained.semantic_identity_value()? != *semantic {
                anyhow::bail!(
                    "persistent-session retained product proof contradicts its semantic program"
                );
            }
            restored["resolution_output"]["composed"]["derived"]
                [EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY] = serde_json::to_value(retained)?;
        }
        _ => anyhow::bail!(
            "persistent-session selected program and retained product proof presence differ"
        ),
    }
    Ok(restored)
}

impl AdmittedPersistentSessionCapsule {
    /// Recover the full existing program DTO without guessing a source proof.
    /// Does not call `validate`, so capsule validation can use this same owner.
    pub fn retained_exact_program(&self) -> anyhow::Result<Value> {
        restore_persistent_product_selections(
            &self.exact_program,
            self.retained_product_selections.as_ref(),
        )
    }

    pub fn authority(&self) -> PersistentSessionAuthority {
        PersistentSessionAuthority {
            external_candidate: self.external_candidate.clone(),
            exact_program_hash: self.exact_program_hash.clone(),
            lifecycle: self.lifecycle.clone(),
            wire: self.wire.clone(),
            artifact_identity: self.artifact_identity.clone(),
            execution_closure: self.execution_closure.clone(),
            runtime_ref: self.runtime_ref.clone(),
            executor_ref: self.executor_ref.clone(),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION
            || self.kind != PERSISTENT_SESSION_CAPSULE_KIND
        {
            anyhow::bail!("invalid persistent-session capsule wire identity");
        }
        if !self.exact_program.is_object() {
            anyhow::bail!("persistent-session exact program must be an object");
        }
        let canonical = lillux::canonical_json(&self.exact_program)?;
        if canonical.len() > MAX_PERSISTENT_SESSION_EXACT_PROGRAM_BYTES {
            anyhow::bail!("persistent-session exact program exceeds its retained byte bound");
        }
        let observed = lillux::sha256_hex(canonical.as_bytes());
        if observed != self.exact_program_hash {
            anyhow::bail!("persistent-session exact program hash mismatch");
        }
        let retained = self.retained_exact_program()?;
        if lillux::canonical_json(&retained)?.len() > MAX_PERSISTENT_SESSION_EXACT_PROGRAM_BYTES {
            anyhow::bail!("persistent-session retained program exceeds its byte bound");
        }
        self.authority().validate()?;
        if let Some(proof) = &self.retained_external_runtime_qualification {
            if self.external_candidate.is_none() {
                anyhow::bail!("external runtime proof has no external candidate");
            }
            proof.validate()?;
        }
        match (
            &self.external_candidate,
            &self.retained_external_runtime_content_qualification,
        ) {
            (Some(program), Some(proof)) => {
                proof.validate()?;
                program.require_content_qualification(&proof.evidence)?;
                if proof.runtime_manifest_hash != program.runtime_manifest_hash {
                    anyhow::bail!(
                        "retained content qualification differs from admitted session runtime"
                    );
                }
            }
            (_, None) => {}
            _ => anyhow::bail!(
                "session-runtime content proof presence differs from external candidate"
            ),
        }
        super::thread_snapshot::validate_canonical_hash(
            "persistent-session execution realization hash",
            &self.execution_realization_hash,
        )?;
        let admitted_source = self
            .exact_program
            .get("resolution_output")
            .and_then(|resolution| resolution.get("composed"))
            .and_then(|composed| composed.get("derived"))
            .and_then(|derived| derived.get(super::SOURCE_CLOSURE_DERIVED_KEY))
            .map(super::EffectiveSourceClosureProjection::from_value)
            .transpose()?;
        if self.source_binding_hash
            != admitted_source
                .as_ref()
                .map(|projection| projection.binding_hash.clone())
        {
            anyhow::bail!("persistent-session source binding contradicts its exact program");
        }
        if let Some(hash) = &self.source_binding_hash {
            super::thread_snapshot::validate_canonical_hash(
                "persistent-session source binding",
                hash,
            )?;
        }
        if let Some(profile) = &self.structured_session_profile {
            profile.validate()?;
            if self.wire.wire_protocol != "ryeos.structured-session" {
                anyhow::bail!("structured-session profile is attached to another wire protocol");
            }
        } else if self.wire.wire_protocol == "ryeos.structured-session" {
            anyhow::bail!("structured-session capsule has no admitted profile identity");
        }
        let requirement = self
            .structured_session_profile
            .as_ref()
            .map(AdmittedStructuredSessionProfile::external_candidate_requirement)
            .transpose()?
            .flatten();
        let realized = self
            .exact_program
            .get("resolution_output")
            .and_then(|resolution| resolution.get("composed"))
            .and_then(|composed| composed.get("derived"))
            .and_then(|derived| derived.get(super::EXTERNAL_REALIZATIONS_DERIVED_KEY))
            .map(super::ExternalContentRealizationSet::from_value)
            .transpose()?;
        let expected = requirement
            .as_ref()
            .map(|requirement| {
                let profile = self.structured_session_profile.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("external candidate has no admitted session profile")
                })?;
                let source = admitted_source.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("external candidate has no admitted source closure")
                })?;
                let realizations = realized.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("external candidate has no exact provider realization")
                })?;
                let qualification_use =
                    crate::external_execution::admission::ExternalCandidateQualificationUse::from_admitted_inputs(
                        requirement,
                        profile,
                        source,
                        realizations,
                        &self.executable_search,
                        &self.process_environment,
                    )?;
                match self.external_candidate.as_ref().map(|program| &program.runtime_source) {
                    Some(crate::external_execution::admission::ExternalCandidateRuntimeSource::ActivatedContent { .. }) => {
                        let retained = self.retained_external_runtime_content_qualification.as_ref()
                            .ok_or_else(|| anyhow::anyhow!("activated candidate has no retained content authority"))?;
                        let program = requirement.resolve_for_content(retained, &qualification_use)?;
                        program.verify_runtime_authority(self.retained_product_selections.as_ref(), Some(retained))?;
                        Ok(program)
                    }
                    _ => requirement.resolve_for_use(
                        self.retained_product_selections.as_ref(), &qualification_use,
                    ),
                }
            })
            .transpose()?;
        if self.external_candidate != expected {
            anyhow::bail!(
                "external candidate program contradicts its signed profile or retained products"
            );
        }
        if self.executable_search.len() > MAX_EXECUTABLE_SEARCH_PATH_ENTRIES {
            anyhow::bail!("persistent-session executable search exceeds its entry bound");
        }
        let mut identities = BTreeSet::new();
        for entry in &self.executable_search {
            entry.validate()?;
            if !identities.insert((
                entry.realization_id.as_str(),
                entry.relative_directory.as_str(),
            )) {
                anyhow::bail!("persistent-session executable search contains a duplicate entry");
            }
            let realization = realized
                .as_ref()
                .and_then(|set| set.iter().find(|item| item.id == entry.realization_id))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "executable-search realization `{}` is absent from the exact program",
                        entry.realization_id
                    )
                })?;
            if realization.kind != super::ExternalContentKind::Tree {
                anyhow::bail!(
                    "executable-search realization `{}` is not tree-shaped",
                    entry.realization_id
                );
            }
        }
        validate_session_process_environment(&self.process_environment)?;
        for value in self.process_environment.values() {
            let SessionProcessEnvironmentValue::RealizationPath { realization_id, .. } = value
            else {
                continue;
            };
            if realized
                .as_ref()
                .and_then(|set| set.iter().find(|item| item.id == *realization_id))
                .is_none_or(|realization| {
                    realization.kind != super::ExternalContentKind::Tree
                        || realization.mode != super::ExternalContentMode::Pinned
                })
            {
                anyhow::bail!(
                    "session process environment realization `{realization_id}` is absent or not a pinned tree"
                );
            }
        }
        Ok(())
    }

    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("persistent-session capsule must be an object"))?;
        let kind = object.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind != PERSISTENT_SESSION_CAPSULE_KIND {
            anyhow::bail!("unexpected persistent-session capsule kind: {kind}");
        }
        let schema = object
            .get("schema")
            .and_then(Value::as_u64)
            .filter(|schema| *schema > 0)
            .ok_or_else(|| {
                anyhow::anyhow!("persistent-session capsule schema must be a positive integer")
            })?;
        if schema != u64::from(PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION) {
            return Err(super::IncompatibleCurrentObjectSchema::new(
                "persistent-session capsule",
                schema,
                PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
            )
            .into());
        }
        let capsule: Self = serde_json::from_value(value.clone())?;
        capsule.validate()?;
        Ok(capsule)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn content_hash(&self) -> anyhow::Result<String> {
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&self.to_value()?)?.as_bytes(),
        ))
    }
}

/// One immutable configuration file in the admitted worker source closure.
/// Destinations are flat names in the exclusively held workload profile home;
/// they are not arbitrary filesystem write authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfigurationFile {
    pub source: String,
    pub destination: String,
}

pub const MAX_SESSION_CONFIGURATION_FILE_BYTES: usize = 64 * 1024;
/// Bounds both authored profile input and its retained canonical contract.
/// Leaves room for the 96 KiB external runtime recipe and other signed policy
/// without accepting an unbounded document before parsing.
pub const MAX_STRUCTURED_SESSION_PROFILE_BYTES: usize = 192 * 1024;
pub const MAX_SESSION_AUXILIARY_CONFIGS: usize = 16;

/// Immutable data from captured worker source, mounted at an exact path in
/// the isolated namespace. This is never a host write or executable grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRuntimeConfigurationFile {
    pub source: String,
    pub destination: String,
}

impl SessionRuntimeConfigurationFile {
    pub fn validate(&self) -> anyhow::Result<()> {
        super::validate_canonical_project_relative_path(&self.source)
            .map_err(|error| anyhow::anyhow!("invalid runtime configuration source: {error}"))?;
        if self.source.len() > 4096 {
            anyhow::bail!("runtime configuration source exceeds its path bound");
        }
        validate_session_runtime_configuration_destination(&self.destination)
    }
}

pub fn validate_session_runtime_configuration_destination(destination: &str) -> anyhow::Result<()> {
    // These are portable POSIX namespace coordinates, never ambient host
    // paths. Do not let Path's normalization conceal doubled separators/dots.
    if destination.len() > 4096
        || !destination.starts_with('/')
        || destination.contains('\\')
        || destination.chars().any(char::is_control)
        || destination[1..]
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        anyhow::bail!(
            "runtime configuration destination is not an exact absolute namespace file path"
        );
    }
    let path = std::path::Path::new(destination);
    // Protected control/kernel namespaces cannot be shadowed by source data.
    for reserved in ["/proc", "/dev", "/sys", "/tmp", "/ryeos", "/run/ryeos"] {
        let reserved = std::path::Path::new(reserved);
        if path.starts_with(reserved) || reserved.starts_with(path) {
            anyhow::bail!("runtime configuration overlaps a protected namespace");
        }
    }
    Ok(())
}

pub fn validate_session_runtime_configs(
    configs: &[SessionRuntimeConfigurationFile],
) -> anyhow::Result<()> {
    if configs.len() > MAX_SESSION_AUXILIARY_CONFIGS {
        anyhow::bail!("runtime configuration inventory exceeds its bound");
    }
    let mut previous: Option<&str> = None;
    for (index, config) in configs.iter().enumerate() {
        config.validate()?;
        if previous.is_some_and(|prior| prior >= config.destination.as_str()) {
            anyhow::bail!("runtime configuration destinations must be sorted and unique");
        }
        let path = std::path::Path::new(&config.destination);
        if configs[..index].iter().any(|other| {
            let other = std::path::Path::new(&other.destination);
            path.starts_with(other) || other.starts_with(path)
        }) {
            anyhow::bail!("runtime configuration file destinations overlap");
        }
        previous = Some(&config.destination);
    }
    Ok(())
}

impl SessionConfigurationFile {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_session_configuration_destination(&self.source)?;
        validate_session_configuration_destination(&self.destination)?;
        Ok(())
    }
}

pub fn validate_session_configuration_destination(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        anyhow::bail!("session configuration must use a bounded relative file name");
    }
    Ok(())
}

pub fn validate_session_auxiliary_configs(
    baseline_destination: &str,
    configs: &[SessionConfigurationFile],
) -> anyhow::Result<()> {
    if configs.len() > MAX_SESSION_AUXILIARY_CONFIGS {
        anyhow::bail!("session auxiliary configuration inventory exceeds its bound");
    }
    let mut previous: Option<&str> = None;
    for config in configs {
        config.validate()?;
        if config.destination == baseline_destination
            || previous.is_some_and(|prior| prior >= config.destination.as_str())
        {
            anyhow::bail!(
                "session auxiliary destinations must be sorted, unique and distinct from the baseline"
            );
        }
        previous = Some(&config.destination);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedStructuredSessionProfile {
    pub profile_hash: String,
    /// The complete canonical, admission-compiled protocol contract.  This is
    /// authority-bearing policy, not a hint for the workload to reinterpret.
    pub contract: serde_json::Value,
    pub schema_hashes: std::collections::BTreeMap<String, String>,
    pub baseline_source: String,
    pub baseline_destination: String,
    pub auxiliary_configs: Vec<SessionConfigurationFile>,
    pub runtime_configs: Vec<SessionRuntimeConfigurationFile>,
}

impl AdmittedStructuredSessionProfile {
    pub fn external_candidate_requirement(
        &self,
    ) -> anyhow::Result<Option<crate::external_execution::admission::ExternalCandidateRequirement>>
    {
        let value = self.contract.get("external_candidate").ok_or_else(|| {
            anyhow::anyhow!("structured-session external candidate requirement is missing")
        })?;
        let requirement: Option<
            crate::external_execution::admission::ExternalCandidateRequirement,
        > = serde_json::from_value(value.clone())?;
        if let Some(requirement) = &requirement {
            requirement.validate()?;
        }
        Ok(requirement)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let external_candidate = self.external_candidate_requirement()?;
        super::thread_snapshot::validate_canonical_hash(
            "structured-session profile hash",
            &self.profile_hash,
        )?;
        let contract = self
            .contract
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("structured-session contract is not an object"))?;
        if contract.is_empty() {
            anyhow::bail!("structured-session contract is empty");
        }
        let workload_client = contract.get("workload_client").ok_or_else(|| {
            anyhow::anyhow!("structured-session workload client contract is missing")
        })?;
        if external_candidate.is_some() && !workload_client.is_null() {
            anyhow::bail!(
                "external candidate profile must retain workload_client null and an empty delegation boundary"
            );
        }
        let declared: Vec<SessionConfigurationFile> = serde_json::from_value(
            contract.get("auxiliary_configs").cloned().ok_or_else(|| {
                anyhow::anyhow!("structured-session auxiliary configuration inventory is missing")
            })?,
        )?;
        if declared != self.auxiliary_configs {
            anyhow::bail!("structured-session auxiliary configuration contradicts its contract");
        }
        validate_session_auxiliary_configs(&self.baseline_destination, &self.auxiliary_configs)?;
        if let Some(external_candidate) = external_candidate.as_ref() {
            let destination = &external_candidate.provider_configuration_destination;
            if destination == &self.baseline_destination
                || self
                    .auxiliary_configs
                    .iter()
                    .any(|config| &config.destination == destination)
            {
                anyhow::bail!(
                    "generated private configuration collides with signed session configuration"
                );
            }
        }
        let runtime: Vec<SessionRuntimeConfigurationFile> =
            serde_json::from_value(contract.get("runtime_configs").cloned().ok_or_else(|| {
                anyhow::anyhow!("structured-session runtime configuration inventory is missing")
            })?)?;
        if runtime != self.runtime_configs {
            anyhow::bail!("structured-session runtime configuration contradicts its contract");
        }
        validate_session_runtime_configs(&self.runtime_configs)?;
        if let Some(contract) = self.portable_state_contract()? {
            contract.validate_configuration_exclusions(&self.auxiliary_configs)?;
            if let Some(external_candidate) = external_candidate.as_ref() {
                contract.validate_generated_configuration_exclusion(
                    &external_candidate.provider_configuration_destination,
                )?;
            }
        }
        self.credential_subject_contract()?
            .map(|contract| contract.validate())
            .transpose()?;
        let canonical = lillux::canonical_json(&self.contract)?;
        if canonical.len() > MAX_STRUCTURED_SESSION_PROFILE_BYTES
            || lillux::sha256_hex(canonical.as_bytes()) != self.profile_hash
        {
            anyhow::bail!("structured-session contract contradicts its admitted hash");
        }
        if self.schema_hashes.is_empty() || self.schema_hashes.len() > 512 {
            anyhow::bail!("structured-session schema identity set is empty or too large");
        }
        for (identity, hash) in &self.schema_hashes {
            let path = std::path::Path::new(identity);
            if identity.len() > 4096
                || path.is_absolute()
                || path.as_os_str().is_empty()
                || path
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                anyhow::bail!("structured-session schema identity is not a safe local path");
            }
            super::thread_snapshot::validate_canonical_hash(
                "structured-session schema hash",
                hash,
            )?;
        }
        for (label, value) in [
            ("structured-session baseline source", &self.baseline_source),
            (
                "structured-session baseline destination",
                &self.baseline_destination,
            ),
        ] {
            let mut components = std::path::Path::new(value).components();
            if value.len() > 128
                || !matches!(components.next(), Some(std::path::Component::Normal(_)))
                || components.next().is_some()
            {
                anyhow::bail!("{label} is not one bounded relative file name");
            }
        }
        Ok(())
    }

    pub fn portable_state_contract(&self) -> anyhow::Result<Option<PortableSessionStateContract>> {
        self.contract
            .get("portable_state")
            .filter(|value| !value.is_null())
            .cloned()
            .map(|value| {
                serde_json::from_value(value)
                    .map_err(anyhow::Error::from)
                    .and_then(|contract: PortableSessionStateContract| {
                        contract.validate()?;
                        Ok(contract)
                    })
            })
            .transpose()
    }

    pub fn credential_subject_contract(
        &self,
    ) -> anyhow::Result<Option<CredentialSubjectProjectionContract>> {
        self.contract
            .get("credential_subject")
            .filter(|value| !value.is_null())
            .cloned()
            .map(|value| {
                serde_json::from_value(value)
                    .map_err(anyhow::Error::from)
                    .and_then(|contract: CredentialSubjectProjectionContract| {
                        contract.validate()?;
                        Ok(contract)
                    })
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_configuration_inventory_is_canonical_bounded_and_nonoverlapping() {
        use super::*;
        let file = SessionRuntimeConfigurationFile {
            source: "policy/requirements.toml".into(),
            destination: "/etc/qualification/requirements.toml".into(),
        };
        validate_session_runtime_configs(&[file.clone()]).unwrap();
        for destination in [
            "",
            "/",
            "relative",
            "//etc/file",
            "/etc/./file",
            "/etc/../file",
            "/etc/file/",
            "/etc/a\\b",
            "/etc/a\0b",
            "/proc/policy",
            "/dev/policy",
            "/sys/policy",
            "/tmp/policy/file",
            "/ryeos/policy",
            "/run",
            "/run/ryeos/policy",
        ] {
            let mut invalid = file.clone();
            invalid.destination = destination.into();
            assert!(
                validate_session_runtime_configs(&[invalid]).is_err(),
                "{destination:?}"
            );
        }
        for source in [
            "",
            "../policy",
            "/etc/policy",
            "a//b",
            "a/./b",
            "a/../b",
            "a\\b",
        ] {
            let mut invalid = file.clone();
            invalid.source = source.into();
            assert!(validate_session_runtime_configs(&[invalid]).is_err());
        }
        assert!(validate_session_runtime_configs(&[file.clone(), file.clone()]).is_err());
        let mut parent = file.clone();
        parent.destination = "/etc/qualification".into();
        assert!(validate_session_runtime_configs(&[parent, file.clone()]).is_err());
        assert!(
            validate_session_runtime_configs(&vec![file; MAX_SESSION_AUXILIARY_CONFIGS + 1])
                .is_err()
        );
        assert!(
            serde_json::from_value::<SessionRuntimeConfigurationFile>(serde_json::json!({
                "source":"a", "destination":"/etc/a", "writable":true
            }))
            .is_err()
        );
    }

    #[test]
    fn auxiliary_configuration_inventory_is_closed_and_bounded() {
        use super::*;
        let file = SessionConfigurationFile {
            source: "admitted.conf".into(),
            destination: "runtime.conf".into(),
        };
        validate_session_auxiliary_configs("baseline.conf", &[file.clone()]).unwrap();
        for invalid in [
            "",
            ".",
            "..",
            "/runtime",
            "dir/file",
            "file/",
            "file\\name",
            "a\0b",
        ] {
            let mut bad = file.clone();
            bad.destination = invalid.into();
            assert!(validate_session_auxiliary_configs("baseline.conf", &[bad]).is_err());
        }
        assert!(validate_session_auxiliary_configs("runtime.conf", &[file.clone()]).is_err());
        assert!(
            validate_session_auxiliary_configs("baseline.conf", &[file.clone(), file.clone()])
                .is_err()
        );
        assert!(
            validate_session_auxiliary_configs(
                "baseline.conf",
                &vec![file; MAX_SESSION_AUXILIARY_CONFIGS + 1]
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<SessionConfigurationFile>(
                serde_json::json!({"source":"a", "destination":"b", "optional":true})
            )
            .is_err()
        );
    }

    #[test]
    fn auxiliary_configuration_cannot_be_restored_as_session_state() {
        use super::*;
        let configs = vec![SessionConfigurationFile {
            source: "signed.conf".into(),
            destination: "environment.conf".into(),
        }];
        let mut contract = PortableSessionStateContract {
            schema: 1,
            restore_contract: "ryeos.worker_session.restore.v1".into(),
            max_depth: 8,
            max_entries: 8,
            max_file_bytes: 1024,
            max_total_bytes: 2048,
            selectors: vec![
                PortableSessionStateSelector {
                    pattern: "environment.conf".into(),
                    class: PortableSessionStateClass::ForbiddenOrUnknown,
                    max_matches: 1,
                },
                PortableSessionStateSelector {
                    pattern: "sessions/{session_id}.json".into(),
                    class: PortableSessionStateClass::PortableSessionState,
                    max_matches: 1,
                },
            ],
        };
        contract.validate().unwrap();
        contract
            .validate_configuration_exclusions(&configs)
            .unwrap();
        contract
            .validate_generated_configuration_exclusion("environment.conf")
            .unwrap();
        for class in [
            PortableSessionStateClass::NodePrivateCredentialState,
            PortableSessionStateClass::RebuildableCache,
        ] {
            contract.selectors[0].class = class;
            assert!(
                contract
                    .validate_configuration_exclusions(&configs)
                    .is_err()
            );
            assert!(
                contract
                    .validate_generated_configuration_exclusion("environment.conf")
                    .is_err()
            );
        }
        contract.selectors[0].class = PortableSessionStateClass::ForbiddenOrUnknown;
        contract.selectors[1].pattern = "{session_id}.conf".into();
        assert!(
            contract
                .validate_configuration_exclusions(&configs)
                .is_err()
        );
        assert!(
            contract
                .validate_generated_configuration_exclusion("environment.conf")
                .is_err()
        );
    }

    #[test]
    fn prepared_session_environment_requires_exact_runtime_view_delivery() {
        use super::*;
        let bindings = BTreeMap::from([
            (
                "CARGO_HOME".to_owned(),
                SessionProcessEnvironmentValue::RuntimeViewDirectory {
                    relative_path: "cargo/home".to_owned(),
                },
            ),
            (
                "CARGO_NET_OFFLINE".to_owned(),
                SessionProcessEnvironmentValue::Literal {
                    value: "true".to_owned(),
                },
            ),
        ]);
        let valid = PreparedSessionProcessEnvironment {
            bindings: bindings.clone(),
            runtime_view_delivery: SessionRuntimeViewDelivery::MountedNamespace {
                destinations: BTreeMap::from([(
                    "CARGO_HOME".to_owned(),
                    runtime_view_mount_destination("CARGO_HOME").unwrap(),
                )]),
            },
        };
        valid.validate().unwrap();
        let mut descriptor = valid.clone();
        descriptor.runtime_view_delivery = SessionRuntimeViewDelivery::DescriptorWorkspace;
        descriptor.validate().unwrap();
        for destinations in [
            BTreeMap::new(),
            BTreeMap::from([(
                "CARGO_HOME".to_owned(),
                std::path::PathBuf::from("/tmp/cache"),
            )]),
            BTreeMap::from([
                (
                    "CARGO_HOME".to_owned(),
                    runtime_view_mount_destination("CARGO_HOME").unwrap(),
                ),
                (
                    "CARGO_NET_OFFLINE".to_owned(),
                    runtime_view_mount_destination("CARGO_NET_OFFLINE").unwrap(),
                ),
            ]),
        ] {
            let mut changed = valid.clone();
            changed.runtime_view_delivery =
                SessionRuntimeViewDelivery::MountedNamespace { destinations };
            assert!(changed.validate().is_err());
        }
        assert!(
            serde_json::from_value::<PreparedSessionProcessEnvironment>(
                serde_json::to_value(&bindings).unwrap()
            )
            .is_err()
        );
        for name in ["../CACHE", "CACHE/subdir", "RYEOS_CACHE", "PATH"] {
            assert!(runtime_view_mount_destination(name).is_err());
        }
        for path in [
            "/ryeos//runtime-views/CARGO_HOME",
            "/ryeos/runtime-views/./CARGO_HOME",
        ] {
            let mut changed = valid.clone();
            changed.runtime_view_delivery = SessionRuntimeViewDelivery::MountedNamespace {
                destinations: BTreeMap::from([("CARGO_HOME".to_owned(), path.into())]),
            };
            assert!(changed.validate().is_err());
        }
        assert!(
            serde_json::from_value::<BTreeMap<String, SessionProcessEnvironmentValue>>(
                serde_json::to_value(&valid).unwrap()
            )
            .is_err(),
            "prepared delivery is not authored environment authority"
        );
    }

    #[test]
    fn persistent_product_proof_restoration_requires_exact_semantic_equality() {
        use crate::external_content::products::composition::*;
        use crate::external_content::products::transfer::ProductWitnessSource;
        let evidence = crate::external_content::products::qualification::tests::dynamic_evidence();
        let selections = evidence.verifier_root_selections.unwrap();
        let full = serde_json::json!({"resolution_output":{"composed":{"derived":{
            (EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY): serde_json::to_value(&selections).unwrap()
        }}}});
        let semantic = serde_json::json!({"resolution_output": project_resolution_product_selections_for_identity(&full["resolution_output"]).unwrap()});
        assert_eq!(
            super::restore_persistent_product_selections(&semantic, Some(&selections)).unwrap(),
            full
        );
        assert!(super::restore_persistent_product_selections(&semantic, None).is_err());
        assert!(
            super::restore_persistent_product_selections(&serde_json::json!({}), Some(&selections))
                .is_err()
        );
        let mut changed = selections.clone().into_inner();
        let first = changed.values_mut().next().unwrap();
        first.witness_source = ProductWitnessSource::Received {
            acceptance_hash: "a".repeat(64),
        };
        let received = ResolvedExternalProductSelections::new(changed).unwrap();
        let restored =
            super::restore_persistent_product_selections(&semantic, Some(&received)).unwrap();
        assert_ne!(restored, full);
        assert_eq!(
            project_resolution_product_selections_for_identity(&restored["resolution_output"])
                .unwrap(),
            semantic["resolution_output"]
        );
        let mut mismatched = semantic.clone();
        let id = selections.iter().next().unwrap().0;
        mismatched["resolution_output"]["composed"]["derived"]
            [EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY][id]["manifest_hash"] =
            serde_json::json!("b".repeat(64));
        assert!(
            super::restore_persistent_product_selections(&mismatched, Some(&received)).is_err()
        );
    }

    use super::*;

    #[test]
    fn lifecycle_refuses_unimplemented_process_multiplexing() {
        let contract = PersistentSessionLifecycleContract {
            max_processes: 1,
            max_inflight_per_process: 2,
            max_address_space_bytes: 64 * 1024 * 1024,
            max_cpu_seconds: 1,
            real_uid_process_limit: 1,
            ready_timeout_ms: 1,
            request_timeout_ms: 1,
            idle_timeout_ms: 1,
        };
        assert!(contract.validate().is_err());
    }

    #[test]
    fn malformed_capsule_envelope_is_not_predecessor_history() {
        for value in [
            serde_json::json!({"kind": PERSISTENT_SESSION_CAPSULE_KIND}),
            serde_json::json!({"kind": PERSISTENT_SESSION_CAPSULE_KIND, "schema": null}),
            serde_json::json!({"kind": PERSISTENT_SESSION_CAPSULE_KIND, "schema": 0}),
            serde_json::json!({"kind": PERSISTENT_SESSION_CAPSULE_KIND, "schema": "10"}),
            serde_json::json!({"kind": PERSISTENT_SESSION_CAPSULE_KIND, "schema": PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION}),
        ] {
            let error = AdmittedPersistentSessionCapsule::from_current_value(&value).unwrap_err();
            assert!(
                error
                    .downcast_ref::<super::super::IncompatibleCurrentObjectSchema>()
                    .is_none()
            );
        }
    }

    #[test]
    fn predecessor_capsule_schema_is_refused_without_translation() {
        for schema in [1, 9, 10, 13] {
            let value = serde_json::json!({
                "schema": schema,
                "kind": PERSISTENT_SESSION_CAPSULE_KIND
            });
            let error = AdmittedPersistentSessionCapsule::from_current_value(&value).unwrap_err();
            assert!(error.to_string().contains("schema"), "got: {error:#}");
        }
    }

    #[test]
    fn credential_subject_projection_excludes_unselected_mutable_fields() {
        let contract = CredentialSubjectProjectionContract {
            schema: 1,
            contract: "example.account.v1".to_string(),
            json_pointers: vec!["/email".to_string(), "/type".to_string()],
        };
        let first = contract
            .derive_subject_digest(&serde_json::json!({
                "email": "owner@example.test",
                "type": "subscription",
                "plan_type": "one"
            }))
            .unwrap();
        let changed_plan = contract
            .derive_subject_digest(&serde_json::json!({
                "email": "owner@example.test",
                "type": "subscription",
                "plan_type": "two"
            }))
            .unwrap();
        assert_eq!(first, changed_plan);
        assert_ne!(
            first,
            contract
                .derive_subject_digest(&serde_json::json!({
                    "email": "another@example.test",
                    "type": "subscription"
                }))
                .unwrap()
        );
    }

    #[test]
    fn session_process_environment_accepts_only_typed_unprotected_bindings() {
        let valid = BTreeMap::from([
            (
                "CARGO_HOME".to_owned(),
                SessionProcessEnvironmentValue::RuntimeViewDirectory {
                    relative_path: "cargo/home".to_owned(),
                },
            ),
            (
                "RUSTUP_HOME".to_owned(),
                SessionProcessEnvironmentValue::RealizationPath {
                    realization_id: "rust-toolchain".to_owned(),
                    relative_path: "rustup".to_owned(),
                    path_kind: SessionProcessEnvironmentPathKind::Directory,
                },
            ),
            (
                "CARGO_NET_OFFLINE".to_owned(),
                SessionProcessEnvironmentValue::Literal {
                    value: "true".to_owned(),
                },
            ),
        ]);
        validate_session_process_environment(&valid).unwrap();
        validate_session_process_environment_relative_path(".").unwrap();

        for name in ["PATH", "HOME", "RYEOS_WORKSPACE", "LD_PRELOAD", "RUST_LOG"] {
            let invalid = BTreeMap::from([(
                name.to_owned(),
                SessionProcessEnvironmentValue::Literal {
                    value: "value".to_owned(),
                },
            )]);
            assert!(
                validate_session_process_environment(&invalid).is_err(),
                "{name}"
            );
        }

        let oversized = BTreeMap::from([(
            "CARGO_TARGET_DIR".to_owned(),
            SessionProcessEnvironmentValue::Literal {
                value: "x".repeat(MAX_SESSION_PROCESS_ENVIRONMENT_ENCODED_BYTES),
            },
        )]);
        assert!(validate_session_process_environment(&oversized).is_err());
    }
}
