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
pub const PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION: u32 = 6;
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
        || name.len() > 128
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedPersistentSessionCapsule {
    pub schema: u32,
    pub kind: String,
    pub exact_program: Value,
    pub exact_program_hash: String,
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

impl AdmittedPersistentSessionCapsule {
    pub fn authority(&self) -> PersistentSessionAuthority {
        PersistentSessionAuthority {
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
        self.authority().validate()?;
        super::thread_snapshot::validate_canonical_hash(
            "persistent-session execution realization hash",
            &self.execution_realization_hash,
        )?;
        let admitted = self
            .exact_program
            .get("resolution_output")
            .and_then(|resolution| resolution.get("composed"))
            .and_then(|composed| composed.get("derived"))
            .and_then(|derived| derived.get(super::SOURCE_CLOSURE_DERIVED_KEY))
            .map(super::EffectiveSourceClosureProjection::from_value)
            .transpose()?
            .map(|projection| projection.binding_hash);
        if self.source_binding_hash != admitted {
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
        if self.executable_search.len() > MAX_EXECUTABLE_SEARCH_PATH_ENTRIES {
            anyhow::bail!("persistent-session executable search exceeds its entry bound");
        }
        let realized = self
            .exact_program
            .get("resolution_output")
            .and_then(|resolution| resolution.get("composed"))
            .and_then(|composed| composed.get("derived"))
            .and_then(|derived| derived.get(super::EXTERNAL_REALIZATIONS_DERIVED_KEY))
            .map(super::ExternalContentRealizationSet::from_value)
            .transpose()?;
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
        let schema = object.get("schema").and_then(Value::as_u64).unwrap_or(0);
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
}

impl AdmittedStructuredSessionProfile {
    pub fn validate(&self) -> anyhow::Result<()> {
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
        self.portable_state_contract()?
            .map(|contract| contract.validate())
            .transpose()?;
        self.credential_subject_contract()?
            .map(|contract| contract.validate())
            .transpose()?;
        let canonical = lillux::canonical_json(&self.contract)?;
        if canonical.len() > 64 * 1024
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
    fn predecessor_capsule_schema_is_refused_without_translation() {
        let value = serde_json::json!({
            "schema": 1,
            "kind": PERSISTENT_SESSION_CAPSULE_KIND
        });
        let error = AdmittedPersistentSessionCapsule::from_current_value(&value).unwrap_err();
        assert!(error.to_string().contains("schema"), "got: {error:#}");
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
