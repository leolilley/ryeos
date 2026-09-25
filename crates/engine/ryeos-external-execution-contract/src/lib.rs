//! Strict, provider-neutral wire contract for external lifecycle adapters.
//!
//! The contract deliberately contains no host filesystem path, command, URL,
//! credential value, clock, database handle, or application state. Its one
//! bundle-relative provider-spec path is a signed artifact identity, not an
//! ambient path; its exact bounded contents are supplied out of band by the
//! controller that admitted the adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use serde::de::{DeserializeOwned, DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use zeroize::Zeroize as _;

pub const LIFECYCLE_ADAPTER_PROTOCOL: &str = "ryeos.external-execution.lifecycle-adapter.v2";
pub const PROVIDER_CONFIGURATION_PROTOCOL: &str =
    "ryeos.external-execution.provider-configuration.v1";
pub const MAX_LIFECYCLE_REQUEST_BYTES: usize = 256 * 1024;
pub const MAX_LIFECYCLE_PROVIDER_SPEC_BYTES: u64 = 256 * 1024;
pub const MAX_GUEST_INPUT_PROJECTION_BYTES: usize = 192 * 1024;
pub const EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA: u32 = 4;
pub const MAX_GUEST_SOURCE_RECORD_BYTES: u64 = 1024 * 1024;
pub const MAX_GUEST_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_LIFECYCLE_RESPONSE_BYTES: usize = 64 * 1024;
pub const MAX_JSON_DEPTH: usize = 32;
pub const MAX_PROVIDER_CONFIGURATION_REQUEST_BYTES: usize = 64 * 1024;
pub const MAX_PROVIDER_CONFIGURATION_BYTES: usize = 1024 * 1024;
pub const LIFECYCLE_REQUEST_FD_ENV: &str = "RYEOS_LIFECYCLE_REQUEST_FD";
pub const LIFECYCLE_SETTINGS_FD_ENV: &str = "RYEOS_LIFECYCLE_SETTINGS_FD";
pub const LIFECYCLE_CREDENTIAL_FD_ENV: &str = "RYEOS_LIFECYCLE_CREDENTIAL_FD";
pub const LIFECYCLE_BOOTSTRAP_FD_ENV: &str = "RYEOS_LIFECYCLE_BOOTSTRAP_FD";
pub const LIFECYCLE_SUPERVISOR_FD_ENV: &str = "RYEOS_LIFECYCLE_SUPERVISOR_FD";
pub const LIFECYCLE_LAUNCHER_FD_ENV: &str = "RYEOS_LIFECYCLE_LAUNCHER_FD";
pub const LIFECYCLE_ADAPTER_EXECUTABLE_FD_ENV: &str = "RYEOS_LIFECYCLE_ADAPTER_EXECUTABLE_FD";
pub const LIFECYCLE_PROVIDER_SPEC_FD_ENV: &str = "RYEOS_LIFECYCLE_PROVIDER_SPEC_FD";
pub const LIFECYCLE_PROVIDER_SPEC_SHA256_ENV: &str = "RYEOS_LIFECYCLE_PROVIDER_SPEC_SHA256";
pub const LIFECYCLE_RESOLVER_FD_ENV: &str = "RYEOS_LIFECYCLE_RESOLVER_FD";
pub const LIFECYCLE_HOSTS_FD_ENV: &str = "RYEOS_LIFECYCLE_HOSTS_FD";
pub const LIFECYCLE_RESOLVER_SHA256_ENV: &str = "RYEOS_LIFECYCLE_RESOLVER_SHA256";
pub const LIFECYCLE_HOSTS_SHA256_ENV: &str = "RYEOS_LIFECYCLE_HOSTS_SHA256";
pub const LIFECYCLE_NETWORK_POLICY_SHA256_ENV: &str = "RYEOS_LIFECYCLE_NETWORK_POLICY_SHA256";
pub const LIFECYCLE_REMAINING_TIMEOUT_MS_ENV: &str = "RYEOS_LIFECYCLE_REMAINING_TIMEOUT_MS";
pub const MAX_GUEST_INPUTS: usize = 64;
pub const MAX_GUEST_ENVIRONMENT_ENTRIES: usize = 256;
pub const MAX_GUEST_EXECUTABLE_SEARCH_ENTRIES: usize = 64;

/// The admitted command protocol, not a choice the executable may change.
/// Direct commands have no structured-session stdin or candidate export lane.
/// Raw stream ceilings do not reserve authenticated journal space: the channel
/// independently bounds encoded frames and may refuse before either raw cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalExecutionMode {
    StructuredSession {},
    DirectCommand {
        stdout_max_bytes: u64,
        stderr_max_bytes: u64,
    },
}

impl ExternalExecutionMode {
    pub fn validate(&self) -> Result<()> {
        if let Self::DirectCommand {
            stdout_max_bytes,
            stderr_max_bytes,
        } = self
        {
            ensure!(
                *stdout_max_bytes > 0
                    && *stderr_max_bytes > 0
                    && stdout_max_bytes
                        .checked_add(*stderr_max_bytes)
                        .is_some_and(|n| n <= 64 * 1024 * 1024),
                "external command output bounds are invalid"
            );
        }
        Ok(())
    }

    pub fn output_limit(&self, stream: ExternalCommandOutputStream) -> Result<u64> {
        self.validate()?;
        match self {
            Self::DirectCommand {
                stdout_max_bytes,
                stderr_max_bytes,
            } => Ok(match stream {
                ExternalCommandOutputStream::Stdout => *stdout_max_bytes,
                ExternalCommandOutputStream::Stderr => *stderr_max_bytes,
            }),
            Self::StructuredSession {} => bail!("session mode has no direct command output"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalCommandOutputStream {
    Stdout,
    Stderr,
}

/// The actual admitted executable's wait status, not a cleanup init's status or
/// a surrogate inferred from pipe EOF or supervisor termination. An executable
/// proven to occupy namespace PID 1 remains the target, not a surrogate init.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExternalTargetExit {
    Code(i32),
    Signal(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalCommandTerminationReason {
    TargetExited,
    Cancelled,
    Deadline,
    OutputLimit,
    Fault,
}

/// Commits exactly the retained prefix of one stream. The receiving journal
/// must independently compare contiguous chunks, length and digest before
/// accepting a terminal observation. `truncated` means the output cap was hit,
/// not that a cancelled computation produced a complete semantic answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCommandOutputCommitment {
    pub bytes: u64,
    pub sha256: String,
    pub truncated: bool,
}

impl ExternalCommandOutputCommitment {
    fn validate(&self, limit: u64) -> Result<()> {
        digest(&self.sha256, "external command output digest")?;
        ensure!(self.bytes <= limit, "external command output exceeds bound");
        ensure!(
            !self.truncated || self.bytes == limit,
            "truncated external command output is not a bounded prefix"
        );
        ensure!(
            self.bytes != 0 || self.sha256 == hex::encode(Sha256::digest([])),
            "empty external command output digest is invalid"
        );
        Ok(())
    }
}

/// Authenticated target termination is not writer exclusion, occurrence death,
/// candidate completion, or permission to publish an evaluation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalCommandTermination {
    pub target_exit: ExternalTargetExit,
    pub reason: ExternalCommandTerminationReason,
    pub stdout: ExternalCommandOutputCommitment,
    pub stderr: ExternalCommandOutputCommitment,
}

impl ExternalCommandTermination {
    pub fn validate(&self, mode: ExternalExecutionMode) -> Result<()> {
        self.stdout
            .validate(mode.output_limit(ExternalCommandOutputStream::Stdout)?)?;
        self.stderr
            .validate(mode.output_limit(ExternalCommandOutputStream::Stderr)?)?;
        ensure!(
            match self.target_exit {
                ExternalTargetExit::Code(code) => (0..=255).contains(&code),
                ExternalTargetExit::Signal(signal) => (1..=64).contains(&signal),
            },
            "external command target exit is invalid"
        );
        let truncated = self.stdout.truncated || self.stderr.truncated;
        ensure!(
            self.reason != ExternalCommandTerminationReason::TargetExited || !truncated,
            "truncated external command output cannot be a normal completion"
        );
        ensure!(
            self.reason != ExternalCommandTerminationReason::OutputLimit || truncated,
            "output-limit termination lacks a truncated stream"
        );
        Ok(())
    }
}

/// Complete descriptor-bound environment delivered to one external guest.
///
/// Descriptor coordinates are launch transport only. `identity_digest()`
/// commits the semantic view independently of process-local descriptor
/// numbers so the durable activation intent survives daemon restart while an
/// initial activation still receives only the exact inherited authorities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalGuestInputProjection {
    pub schema: u32,
    pub base_snapshot: GuestBaseSnapshotInput,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub workspace_outputs: Option<GuestWorkspaceOutputAuthorityInput>,
    pub inputs: Vec<GuestMountInput>,
    pub executable_search: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

/// Exact admitted workspace-output contract transferred as one immutable
/// descriptor. The provider-neutral wire layer commits only its canonical
/// digest and producer coordinates; the external runtime decodes and validates
/// the existing RyeOS object before any capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestWorkspaceOutputAuthorityInput {
    pub descriptor: u32,
    pub authority_hash: String,
    pub bytes: u64,
    pub producer_chain_root_id: String,
    pub producer_thread_id: String,
    pub admitted_launch_capsule_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestBaseSnapshotInput {
    pub descriptor: u32,
    pub snapshot_hash: String,
    pub closure_digest: String,
    pub object_count: u64,
    pub blob_count: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestMountRole {
    Product,
    Source,
    Configuration,
    PrivateScratch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestMountKind {
    Directory,
    RegularFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestMountAccess {
    ReadOnly,
    PrivateWritable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestProductManifestKind {
    Content,
    LargeContent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestMountContentAuthority {
    ProductManifest {
        manifest_kind: GuestProductManifestKind,
        manifest_hash: String,
        manifest_descriptor: u32,
        manifest_bytes: u64,
    },
    /// Existing CAS source authority, not an independently attested product.
    /// The receiver verifies these exact records and their existing relation.
    SourceClosure {
        binding_hash: String,
        binding_descriptor: u32,
        binding_bytes: u64,
        manifest_hash: String,
        manifest_descriptor: u32,
        manifest_bytes: u64,
    },
    RawFile {
        sha256: String,
    },
    PrivateScratch {
        binding_hash: String,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GuestMountSemanticContentAuthority<'a> {
    ProductManifest {
        manifest_kind: GuestProductManifestKind,
        manifest_hash: &'a str,
        manifest_bytes: u64,
    },
    SourceClosure {
        binding_hash: &'a str,
        binding_bytes: u64,
        manifest_hash: &'a str,
        manifest_bytes: u64,
    },
    RawFile {
        sha256: &'a str,
    },
    PrivateScratch {
        binding_hash: &'a str,
    },
}

impl GuestMountContentAuthority {
    /// Exact retained records in transport order. Descriptor coordinates are
    /// process-local; record hashes and lengths belong to semantic identity.
    /// Source records always precede their manifest in binding/manifest order.
    pub fn record_descriptors(&self) -> impl Iterator<Item = (u32, &str, u64)> {
        let records = match self {
            Self::ProductManifest {
                manifest_descriptor,
                manifest_hash,
                manifest_bytes,
                ..
            } => [
                Some((
                    *manifest_descriptor,
                    manifest_hash.as_str(),
                    *manifest_bytes,
                )),
                None,
            ],
            Self::SourceClosure {
                binding_descriptor,
                binding_hash,
                binding_bytes,
                manifest_descriptor,
                manifest_hash,
                manifest_bytes,
            } => [
                Some((*binding_descriptor, binding_hash.as_str(), *binding_bytes)),
                Some((
                    *manifest_descriptor,
                    manifest_hash.as_str(),
                    *manifest_bytes,
                )),
            ],
            Self::RawFile { .. } | Self::PrivateScratch { .. } => [None, None],
        };
        records.into_iter().flatten()
    }

    fn semantic(&self) -> GuestMountSemanticContentAuthority<'_> {
        match self {
            Self::ProductManifest {
                manifest_kind,
                manifest_hash,
                manifest_bytes,
                ..
            } => GuestMountSemanticContentAuthority::ProductManifest {
                manifest_kind: *manifest_kind,
                manifest_hash,
                manifest_bytes: *manifest_bytes,
            },
            Self::SourceClosure {
                binding_hash,
                binding_bytes,
                manifest_hash,
                manifest_bytes,
                ..
            } => GuestMountSemanticContentAuthority::SourceClosure {
                binding_hash,
                binding_bytes: *binding_bytes,
                manifest_hash,
                manifest_bytes: *manifest_bytes,
            },
            Self::RawFile { sha256 } => GuestMountSemanticContentAuthority::RawFile { sha256 },
            Self::PrivateScratch { binding_hash } => {
                GuestMountSemanticContentAuthority::PrivateScratch { binding_hash }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestMountInput {
    pub role: GuestMountRole,
    pub authority_id: String,
    pub descriptor: u32,
    pub destination: String,
    pub kind: GuestMountKind,
    pub access: GuestMountAccess,
    /// Portable regular-file mode retained across provider staging. Required
    /// and non-null only for regular files; directory traversal authority is
    /// represented by `kind`, not by a synthetic mode.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub normalized_mode: Option<u32>,
    pub content_authority: GuestMountContentAuthority,
    pub bytes: u64,
}

#[derive(Serialize)]
struct GuestInputSemanticIdentity<'a> {
    domain: &'static str,
    base_snapshot_hash: &'a str,
    base_snapshot_closure_digest: &'a str,
    base_snapshot_object_count: u64,
    base_snapshot_blob_count: u64,
    base_snapshot_total_bytes: u64,
    workspace_outputs: Option<GuestWorkspaceOutputSemanticIdentity<'a>>,
    inputs: Vec<GuestMountSemanticIdentity<'a>>,
    executable_search: &'a [String],
    environment: &'a BTreeMap<String, String>,
}

#[derive(Serialize)]
struct GuestWorkspaceOutputSemanticIdentity<'a> {
    authority_hash: &'a str,
    bytes: u64,
    producer_chain_root_id: &'a str,
    producer_thread_id: &'a str,
    admitted_launch_capsule_hash: &'a str,
}

#[derive(Serialize)]
struct GuestMountSemanticIdentity<'a> {
    role: GuestMountRole,
    authority_id: &'a str,
    destination: &'a str,
    kind: GuestMountKind,
    access: GuestMountAccess,
    normalized_mode: Option<u32>,
    content_authority: GuestMountSemanticContentAuthority<'a>,
    bytes: u64,
}

impl ExternalGuestInputProjection {
    /// All retained records in input order, with each source binding before
    /// its manifest. Producers and consumers share this exact inventory.
    pub fn record_descriptors(&self) -> impl Iterator<Item = (u32, &str, u64)> {
        self.inputs
            .iter()
            .flat_map(|input| input.content_authority.record_descriptors())
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_structure()?;
        ensure!(
            canonical_json(self)?.len() <= MAX_GUEST_INPUT_PROJECTION_BYTES,
            "external guest input projection exceeds its encoded bound"
        );
        Ok(())
    }

    fn validate_structure(&self) -> Result<()> {
        ensure!(
            self.schema == EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            "unsupported external guest input schema"
        );
        digest(
            &self.base_snapshot.snapshot_hash,
            "external guest base snapshot",
        )?;
        digest(
            &self.base_snapshot.closure_digest,
            "external guest base snapshot closure",
        )?;
        ensure!(
            self.base_snapshot.descriptor > 2
                && self.base_snapshot.object_count >= 3
                && self.base_snapshot.object_count <= 100_000
                && self.base_snapshot.blob_count <= 100_000
                && self.base_snapshot.total_bytes <= 1024 * 1024 * 1024,
            "external guest base snapshot transfer is invalid"
        );
        ensure!(
            !self.inputs.is_empty() && self.inputs.len() <= MAX_GUEST_INPUTS,
            "external guest input inventory is empty or exceeds its bound"
        );
        ensure!(
            self.executable_search.len() <= MAX_GUEST_EXECUTABLE_SEARCH_ENTRIES
                && self.environment.len() <= MAX_GUEST_ENVIRONMENT_ENTRIES,
            "external guest environment exceeds its entry bounds"
        );

        let mut descriptors = BTreeSet::from([self.base_snapshot.descriptor]);
        if let Some(outputs) = &self.workspace_outputs {
            ensure!(
                outputs.descriptor > 2 && descriptors.insert(outputs.descriptor),
                "external guest workspace-output descriptor is invalid or duplicated"
            );
            digest(
                &outputs.authority_hash,
                "external guest workspace-output authority",
            )?;
            digest(
                &outputs.admitted_launch_capsule_hash,
                "external guest workspace-output capsule",
            )?;
            ensure!(
                outputs.bytes > 0 && outputs.bytes <= 64 * 1024,
                "external guest workspace-output authority byte bound is invalid"
            );
            bounded_identifier(
                &outputs.producer_chain_root_id,
                2_048,
                "external guest workspace-output producer root",
            )?;
            bounded_identifier(
                &outputs.producer_thread_id,
                2_048,
                "external guest workspace-output producer thread",
            )?;
        }
        let mut destinations: Vec<&str> = Vec::new();
        let mut identities = BTreeSet::new();
        for input in &self.inputs {
            bounded_identifier(&input.authority_id, 256, "external guest authority id")?;
            ensure!(
                input.descriptor > 2 && descriptors.insert(input.descriptor),
                "external guest input descriptor is invalid or duplicated"
            );
            validate_absolute_normalized_path(
                &input.destination,
                "external guest input destination",
            )?;
            ensure!(
                input.destination != "/workspace"
                    && !Path::new("/workspace").starts_with(&input.destination),
                "external guest input replaces or contains the candidate workspace"
            );
            ensure!(
                !Path::new(&input.destination).starts_with("/workspace")
                    || (input.access == GuestMountAccess::ReadOnly
                        && matches!(input.role, GuestMountRole::Product | GuestMountRole::Source)),
                "external guest writable or configuration input overlaps the candidate workspace"
            );
            ensure!(
                destinations.iter().all(|destination| {
                    let existing = Path::new(destination);
                    let proposed = Path::new(&input.destination);
                    !existing.starts_with(proposed) && !proposed.starts_with(existing)
                }),
                "external guest input destinations overlap"
            );
            destinations.push(&input.destination);
            ensure!(
                identities.insert((input.role, input.authority_id.as_str())),
                "external guest authority identity is duplicated"
            );
            match (&input.role, &input.content_authority) {
                (
                    GuestMountRole::Product,
                    GuestMountContentAuthority::ProductManifest {
                        manifest_hash,
                        manifest_descriptor,
                        manifest_bytes,
                        ..
                    },
                ) => {
                    digest(manifest_hash, "external guest product manifest")?;
                    ensure!(
                        *manifest_descriptor > 2
                            && descriptors.insert(*manifest_descriptor)
                            && (1..=8 * 1024 * 1024).contains(manifest_bytes),
                        "external guest product manifest descriptor or byte bound is invalid"
                    );
                }
                (GuestMountRole::Source, GuestMountContentAuthority::SourceClosure { .. }) => {
                    ensure!(
                        input.kind == GuestMountKind::Directory
                            && input.access == GuestMountAccess::ReadOnly
                            && input.bytes <= MAX_GUEST_SOURCE_BYTES,
                        "external guest source closure must be a bounded read-only directory"
                    );
                    for (descriptor, hash, bytes) in input.content_authority.record_descriptors() {
                        digest(hash, "external guest source record")?;
                        ensure!(
                            (3..=i32::MAX as u32).contains(&descriptor)
                                && descriptors.insert(descriptor)
                                && (1..=MAX_GUEST_SOURCE_RECORD_BYTES).contains(&bytes),
                            "external guest source record descriptor or byte bound is invalid"
                        );
                    }
                }
                (GuestMountRole::Configuration, GuestMountContentAuthority::RawFile { sha256 }) => {
                    digest(sha256, "external guest raw file")?;
                    ensure!(
                        input.kind == GuestMountKind::RegularFile,
                        "external guest raw file authority changed kind"
                    );
                }
                (
                    GuestMountRole::PrivateScratch,
                    GuestMountContentAuthority::PrivateScratch { binding_hash },
                ) => {
                    digest(binding_hash, "external guest private scratch binding")?;
                }
                _ => anyhow::bail!("external guest input role contradicts its content authority"),
            }
            ensure!(
                input.bytes <= 1024 * 1024 * 1024 * 1024,
                "external guest input byte bound is invalid"
            );
            ensure!(
                !matches!(input.role, GuestMountRole::PrivateScratch)
                    || (input.access == GuestMountAccess::PrivateWritable
                        && input.kind == GuestMountKind::Directory),
                "external guest private scratch must be a writable directory"
            );
            ensure!(
                matches!(input.role, GuestMountRole::PrivateScratch)
                    || input.access == GuestMountAccess::ReadOnly,
                "external guest immutable input became writable"
            );
            ensure!(
                match input.kind {
                    GuestMountKind::Directory => input.normalized_mode.is_none(),
                    GuestMountKind::RegularFile => {
                        matches!(input.normalized_mode, Some(0o644 | 0o755))
                    }
                },
                "external guest input mode does not match its kind"
            );
        }
        for path in &self.executable_search {
            validate_absolute_normalized_path(path, "external guest executable search path")?;
            ensure!(
                self.inputs.iter().any(|input| {
                    matches!(input.role, GuestMountRole::Product | GuestMountRole::Source)
                        && input.kind == GuestMountKind::Directory
                        && Path::new(path).starts_with(&input.destination)
                }),
                "external guest executable search path is outside immutable admitted inputs"
            );
        }
        for (name, value) in &self.environment {
            ensure!(
                valid_environment_name(name)
                    && name.len() <= 256
                    && value.len() <= 64 * 1024
                    && !value.contains('\0'),
                "external guest environment entry is invalid"
            );
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        canonical_json(self)
    }

    pub fn identity_digest(&self) -> Result<String> {
        self.validate()?;
        let inputs = self
            .inputs
            .iter()
            .map(|input| GuestMountSemanticIdentity {
                role: input.role,
                authority_id: &input.authority_id,
                destination: &input.destination,
                kind: input.kind,
                access: input.access,
                normalized_mode: input.normalized_mode,
                content_authority: input.content_authority.semantic(),
                bytes: input.bytes,
            })
            .collect();
        let bytes = canonical_json(&GuestInputSemanticIdentity {
            domain: "ryeos.external-guest-inputs.v4",
            base_snapshot_hash: &self.base_snapshot.snapshot_hash,
            base_snapshot_closure_digest: &self.base_snapshot.closure_digest,
            base_snapshot_object_count: self.base_snapshot.object_count,
            base_snapshot_blob_count: self.base_snapshot.blob_count,
            base_snapshot_total_bytes: self.base_snapshot.total_bytes,
            workspace_outputs: self.workspace_outputs.as_ref().map(|outputs| {
                GuestWorkspaceOutputSemanticIdentity {
                    authority_hash: &outputs.authority_hash,
                    bytes: outputs.bytes,
                    producer_chain_root_id: &outputs.producer_chain_root_id,
                    producer_thread_id: &outputs.producer_thread_id,
                    admitted_launch_capsule_hash: &outputs.admitted_launch_capsule_hash,
                }
            }),
            inputs,
            executable_search: &self.executable_search,
            environment: &self.environment,
        })?;
        Ok(hex::encode(Sha256::digest(&bytes)))
    }
}

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleCapability {
    ExactAllocationReconciliation,
    /// Exact evidence that this allocation request created no occurrence.
    /// This may be provider testimony or a transport-owned proof that no
    /// request bytes were sent. A provider error after transmission, a lost
    /// response, and list absence do not establish this capability.
    AuthoritativeNoOccurrence,
    ExactActivationReconciliation,
    IdempotentTermination,
    ExactTerminalObservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleArtifactRole {
    Supervisor,
    Launcher,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleArtifactInspection {
    pub descriptor: u32,
    pub digest: String,
    pub bytes: u64,
}

/// Signed bundle declaration for the controller-side provider boundary.
///
/// Executable names are resolved only through the declaring bundle's signed
/// executor manifest. The declaration carries no host pathname, endpoint,
/// credential, or mutable provider setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalProviderDeclaration {
    pub id: String,
    pub protocol: String,
    pub targets: Vec<String>,
    pub connector: String,
    pub connector_process_group: ExternalProviderConnectorProcessGroup,
    pub configuration_adapter: String,
    pub configuration_destination: String,
}

/// Process-control behavior of the signed provider's command environment.
/// The controller must retain authority over every connector descendant;
/// this is not a request to weaken its host isolation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalProviderConnectorProcessGroup {
    Inherited,
    New,
}

/// Signed bundle declaration for one external occurrence lifecycle adapter.
/// The adapter, provider behavior specification, and two guest bootstrap
/// executables are captured from the same signed bundle generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalLifecycleAdapterDeclaration {
    pub id: String,
    pub protocol: String,
    pub targets: Vec<String>,
    pub adapter: String,
    pub supervisor: String,
    pub launcher: String,
    pub provider_spec: LifecycleProviderSpecIdentity,
    pub settings_schema_digest: String,
    pub capabilities: BTreeSet<LifecycleCapability>,
}

/// Bundle-relative identity of the closed provider behavior specification
/// consumed by a lifecycle adapter. `sha256` is part of the signed bundle
/// declaration and must match the bounded bytes captured from `path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleProviderSpecIdentity {
    pub path: String,
    pub sha256: String,
}

impl ExternalProviderDeclaration {
    pub fn validate(&self) -> Result<()> {
        bounded_identifier(&self.id, 128, "external provider declaration id")?;
        ensure!(
            self.protocol == PROVIDER_CONFIGURATION_PROTOCOL,
            "external provider declaration has an unsupported protocol"
        );
        validate_targets(&self.targets)?;
        validate_executable_name(&self.connector, "external provider connector")?;
        validate_executable_name(
            &self.configuration_adapter,
            "external provider configuration adapter",
        )?;
        validate_single_file_name(
            &self.configuration_destination,
            "external provider configuration destination",
        )?;
        Ok(())
    }
}

impl ExternalLifecycleAdapterDeclaration {
    pub fn validate(&self) -> Result<()> {
        bounded_identifier(&self.id, 128, "external lifecycle adapter id")?;
        ensure!(
            self.protocol == LIFECYCLE_ADAPTER_PROTOCOL,
            "external lifecycle adapter declaration has an unsupported protocol"
        );
        validate_targets(&self.targets)?;
        validate_executable_name(&self.adapter, "external lifecycle adapter")?;
        validate_executable_name(&self.supervisor, "external candidate supervisor")?;
        validate_executable_name(&self.launcher, "external candidate launcher")?;
        validate_bundle_relative_file_path(&self.provider_spec.path, "lifecycle provider spec")?;
        digest(&self.provider_spec.sha256, "lifecycle provider spec")?;
        digest(
            &self.settings_schema_digest,
            "external lifecycle settings schema",
        )?;
        ensure!(
            !self.capabilities.is_empty(),
            "external lifecycle adapter declares no capabilities"
        );
        Ok(())
    }
}

/// One occurrence-private request to a signed provider configuration adapter.
/// The endpoint and executable coordinate are controller-created dynamic slots,
/// not selectable commands. The capability is deliberately omitted from Debug
/// and zeroized with the request.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfigurationRequest {
    pub schema: u32,
    pub protocol: String,
    pub provider_declaration_id: String,
    pub connector_executable: ProviderConnectorExecutable,
    pub connector_endpoint: String,
    pub placement_thread_id: String,
    pub execution_binding_hash: String,
    pub connector_capability: String,
}

/// Controller-selected delivery of the already admitted connector image.
/// This coordinate conveys no authority to choose a different executable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderConnectorExecutable {
    InheritedDescriptor { descriptor: u32 },
    NamespacePath { path: String },
}

impl ProviderConfigurationRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1,
            "unsupported provider configuration schema"
        );
        ensure!(
            self.protocol == PROVIDER_CONFIGURATION_PROTOCOL,
            "unsupported provider configuration protocol"
        );
        bounded_identifier(
            &self.provider_declaration_id,
            128,
            "provider declaration id",
        )?;
        match &self.connector_executable {
            ProviderConnectorExecutable::InheritedDescriptor { descriptor } => ensure!(
                *descriptor > 2,
                "provider connector descriptor overlaps standard I/O"
            ),
            ProviderConnectorExecutable::NamespacePath { path } => {
                validate_absolute_normalized_path(path, "provider connector executable")?
            }
        }
        validate_absolute_normalized_path(&self.connector_endpoint, "provider connector endpoint")?;
        bounded_text(
            &self.placement_thread_id,
            256,
            "provider placement thread id",
        )?;
        digest(&self.execution_binding_hash, "provider execution binding")?;
        ensure!(
            !self.connector_capability.is_empty()
                && self.connector_capability.len() <= 16 * 1024
                && !self.connector_capability.chars().any(char::is_control),
            "provider connector capability is invalid"
        );
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = canonical_json(self)?;
        ensure!(
            bytes.len() <= MAX_PROVIDER_CONFIGURATION_REQUEST_BYTES,
            "provider configuration request exceeds its byte bound"
        );
        Ok(bytes)
    }
}

impl Drop for ProviderConfigurationRequest {
    fn drop(&mut self) {
        self.connector_capability.zeroize();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleAdapterInspectionRequest {
    pub schema: u32,
    pub protocol: String,
    pub adapter_id: String,
    pub adapter_artifact_hash: String,
    pub settings_schema_digest: String,
    pub target: String,
    pub declared_capabilities: BTreeSet<LifecycleCapability>,
    pub provider_spec: LifecycleArtifactInspection,
    pub artifacts: BTreeMap<LifecycleArtifactRole, LifecycleArtifactInspection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleAdapterInspectionResponse {
    pub schema: u32,
    pub protocol: String,
    pub adapter_id: String,
    pub adapter_build: String,
    pub observed_adapter_artifact_hash: String,
    pub observed_settings_schema_digest: String,
    pub target: String,
    pub effective_capabilities: BTreeSet<LifecycleCapability>,
    pub observed_provider_spec_sha256: String,
    pub artifacts: BTreeMap<LifecycleArtifactRole, LifecycleArtifactInspection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleAdapterRequest {
    Allocate {
        common: LifecycleOperationCommon,
        reservation: AllocationReservation,
    },
    ReconcileAllocation {
        common: LifecycleOperationCommon,
        reservation: AllocationReservation,
    },
    ActivateSupervisor {
        common: LifecycleOperationCommon,
        occurrence: BoundOccurrence,
        activation: SupervisorActivationIntent,
        guest_inputs: ExternalGuestInputProjection,
    },
    ReconcileSupervisorActivation {
        common: LifecycleOperationCommon,
        occurrence: BoundOccurrence,
        activation: SupervisorActivationIntent,
    },
    Terminate {
        common: LifecycleOperationCommon,
        occurrence: BoundOccurrence,
        termination: TerminationIntent,
    },
    ReconcileTermination {
        common: LifecycleOperationCommon,
        occurrence: BoundOccurrence,
        termination: TerminationIntent,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LifecycleOperationCommon {
    pub schema: u32,
    pub protocol: String,
    pub operation_id: String,
    pub binding_hash: String,
    pub settings_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllocationReservation {
    pub placement_thread_id: String,
    pub admitted_capsule_hash: String,
    pub base_snapshot_hash: String,
    pub request_digest: String,
    pub maximum_lifetime_seconds: u32,
    pub contact_deadline_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundOccurrence {
    pub request_digest: String,
    pub occurrence_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorActivationIntent {
    pub activation_request_digest: String,
    pub supervisor_runtime_hash: String,
    pub launcher_artifact_hash: String,
    pub attachment_deadline_ms: i64,
    pub execution_timeout_seconds: u32,
    pub post_execution_timeout_seconds: u32,
    pub channel_max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminationIntent {
    pub termination_request_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum LifecycleAdapterResponse {
    AllocationBound {
        operation_id: String,
        request_digest: String,
        occurrence_id: String,
        provider_observation_digest: String,
    },
    AllocationNoOccurrence {
        operation_id: String,
        request_digest: String,
        /// Digest of exact no-occurrence evidence. Despite the historical
        /// field name, the evidence may be a local pre-request transport fact
        /// rather than a response from the provider.
        provider_observation_digest: String,
    },
    AllocationPending {
        operation_id: String,
        request_digest: String,
    },
    SupervisorStarted {
        operation_id: String,
        activation_request_digest: String,
        provider_observation_digest: String,
    },
    SupervisorNotStarted {
        operation_id: String,
        activation_request_digest: String,
        provider_observation_digest: String,
    },
    SupervisorPending {
        operation_id: String,
        activation_request_digest: String,
    },
    OccurrenceTerminal {
        operation_id: String,
        termination_request_digest: String,
        provider_observation_digest: String,
    },
    TerminationPending {
        operation_id: String,
        termination_request_digest: String,
    },
}

impl LifecycleAdapterInspectionRequest {
    pub fn validate(&self) -> Result<()> {
        validate_inspection_common(
            self.schema,
            &self.protocol,
            &self.adapter_id,
            &self.adapter_artifact_hash,
            &self.settings_schema_digest,
            &self.target,
        )?;
        ensure!(
            !self.declared_capabilities.is_empty(),
            "lifecycle adapter declares no capabilities"
        );
        validate_provider_spec_inspection(&self.provider_spec)?;
        validate_artifact_inspections(&self.artifacts)?;
        ensure!(
            self.artifacts.len() == 2
                && self
                    .artifacts
                    .contains_key(&LifecycleArtifactRole::Supervisor)
                && self
                    .artifacts
                    .contains_key(&LifecycleArtifactRole::Launcher),
            "lifecycle adapter inspection requires exact supervisor and launcher artifacts"
        );
        Ok(())
    }
}

impl LifecycleAdapterInspectionResponse {
    pub fn validate_for(&self, request: &LifecycleAdapterInspectionRequest) -> Result<()> {
        request.validate()?;
        validate_inspection_common(
            self.schema,
            &self.protocol,
            &self.adapter_id,
            &self.observed_adapter_artifact_hash,
            &self.observed_settings_schema_digest,
            &self.target,
        )?;
        bounded_identifier(&self.adapter_build, 256, "lifecycle adapter build")?;
        ensure!(
            self.adapter_id == request.adapter_id
                && self.observed_adapter_artifact_hash == request.adapter_artifact_hash
                && self.observed_settings_schema_digest == request.settings_schema_digest
                && self.target == request.target
                && self.observed_provider_spec_sha256 == request.provider_spec.digest
                && self
                    .effective_capabilities
                    .is_subset(&request.declared_capabilities)
                && self.artifacts == request.artifacts,
            "lifecycle adapter inspection contradicts its declaration"
        );
        Ok(())
    }
}

fn validate_provider_spec_inspection(artifact: &LifecycleArtifactInspection) -> Result<()> {
    ensure!(
        artifact.descriptor > 2
            && (1..=MAX_LIFECYCLE_PROVIDER_SPEC_BYTES).contains(&artifact.bytes),
        "lifecycle provider spec descriptor or size is invalid"
    );
    digest(&artifact.digest, "lifecycle provider spec")
}

fn validate_artifact_inspections(
    artifacts: &BTreeMap<LifecycleArtifactRole, LifecycleArtifactInspection>,
) -> Result<()> {
    for artifact in artifacts.values() {
        ensure!(
            artifact.descriptor > 2 && (1..=1024 * 1024 * 1024).contains(&artifact.bytes),
            "lifecycle artifact descriptor or size is invalid"
        );
        digest(&artifact.digest, "lifecycle artifact")?;
    }
    Ok(())
}

impl LifecycleAdapterRequest {
    pub fn validate(&self) -> Result<()> {
        self.common().validate()?;
        match self {
            Self::Allocate { reservation, .. } | Self::ReconcileAllocation { reservation, .. } => {
                reservation.validate()
            }
            Self::ActivateSupervisor {
                occurrence,
                activation,
                guest_inputs,
                ..
            } => {
                occurrence.validate()?;
                activation.validate()?;
                guest_inputs.validate()
            }
            Self::ReconcileSupervisorActivation {
                occurrence,
                activation,
                ..
            } => {
                occurrence.validate()?;
                activation.validate()
            }
            Self::Terminate {
                occurrence,
                termination,
                ..
            }
            | Self::ReconcileTermination {
                occurrence,
                termination,
                ..
            } => {
                occurrence.validate()?;
                termination.validate()
            }
        }
    }

    pub fn common(&self) -> &LifecycleOperationCommon {
        match self {
            Self::Allocate { common, .. }
            | Self::ReconcileAllocation { common, .. }
            | Self::ActivateSupervisor { common, .. }
            | Self::ReconcileSupervisorActivation { common, .. }
            | Self::Terminate { common, .. }
            | Self::ReconcileTermination { common, .. } => common,
        }
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = canonical_json(self)?;
        ensure!(
            bytes.len() <= MAX_LIFECYCLE_REQUEST_BYTES,
            "lifecycle adapter request exceeds its byte bound"
        );
        Ok(bytes)
    }
}

impl LifecycleAdapterResponse {
    pub fn validate_for(&self, request: &LifecycleAdapterRequest) -> Result<()> {
        request.validate()?;
        bounded_identifier(self.operation_id(), 256, "lifecycle operation id")?;
        ensure!(
            self.operation_id() == request.common().operation_id,
            "lifecycle response changed its operation identity"
        );
        match (request, self) {
            (
                LifecycleAdapterRequest::Allocate { reservation, .. }
                | LifecycleAdapterRequest::ReconcileAllocation { reservation, .. },
                Self::AllocationBound {
                    request_digest,
                    occurrence_id,
                    provider_observation_digest,
                    ..
                },
            ) => {
                ensure!(
                    request_digest == &reservation.request_digest,
                    "allocation response changed its request identity"
                );
                bounded_text(occurrence_id, 512, "external occurrence id")?;
                digest(
                    provider_observation_digest,
                    "provider allocation observation",
                )
            }
            (
                LifecycleAdapterRequest::Allocate { reservation, .. }
                | LifecycleAdapterRequest::ReconcileAllocation { reservation, .. },
                Self::AllocationNoOccurrence {
                    request_digest,
                    provider_observation_digest,
                    ..
                },
            ) => {
                ensure!(
                    request_digest == &reservation.request_digest,
                    "allocation response changed its request identity"
                );
                digest(
                    provider_observation_digest,
                    "provider no-occurrence observation",
                )
            }
            (
                LifecycleAdapterRequest::Allocate { reservation, .. }
                | LifecycleAdapterRequest::ReconcileAllocation { reservation, .. },
                Self::AllocationPending { request_digest, .. },
            ) => {
                ensure!(
                    request_digest == &reservation.request_digest,
                    "allocation response changed its request identity"
                );
                Ok(())
            }
            (
                LifecycleAdapterRequest::ActivateSupervisor { activation, .. }
                | LifecycleAdapterRequest::ReconcileSupervisorActivation { activation, .. },
                Self::SupervisorStarted {
                    activation_request_digest,
                    provider_observation_digest,
                    ..
                }
                | Self::SupervisorNotStarted {
                    activation_request_digest,
                    provider_observation_digest,
                    ..
                },
            ) => {
                ensure!(
                    activation_request_digest == &activation.activation_request_digest,
                    "activation response changed its request identity"
                );
                digest(
                    provider_observation_digest,
                    "provider activation observation",
                )
            }
            (
                LifecycleAdapterRequest::ActivateSupervisor { activation, .. }
                | LifecycleAdapterRequest::ReconcileSupervisorActivation { activation, .. },
                Self::SupervisorPending {
                    activation_request_digest,
                    ..
                },
            ) => {
                ensure!(
                    activation_request_digest == &activation.activation_request_digest,
                    "activation response changed its request identity"
                );
                Ok(())
            }
            (
                LifecycleAdapterRequest::Terminate { termination, .. }
                | LifecycleAdapterRequest::ReconcileTermination { termination, .. },
                Self::OccurrenceTerminal {
                    termination_request_digest,
                    provider_observation_digest,
                    ..
                },
            ) => {
                ensure!(
                    termination_request_digest == &termination.termination_request_digest,
                    "termination response changed its request identity"
                );
                digest(provider_observation_digest, "provider terminal observation")
            }
            (
                LifecycleAdapterRequest::Terminate { termination, .. }
                | LifecycleAdapterRequest::ReconcileTermination { termination, .. },
                Self::TerminationPending {
                    termination_request_digest,
                    ..
                },
            ) => {
                ensure!(
                    termination_request_digest == &termination.termination_request_digest,
                    "termination response changed its request identity"
                );
                Ok(())
            }
            _ => bail!("lifecycle response outcome does not match its requested operation"),
        }
    }

    pub fn operation_id(&self) -> &str {
        match self {
            Self::AllocationBound { operation_id, .. }
            | Self::AllocationNoOccurrence { operation_id, .. }
            | Self::AllocationPending { operation_id, .. }
            | Self::SupervisorStarted { operation_id, .. }
            | Self::SupervisorNotStarted { operation_id, .. }
            | Self::SupervisorPending { operation_id, .. }
            | Self::OccurrenceTerminal { operation_id, .. }
            | Self::TerminationPending { operation_id, .. } => operation_id,
        }
    }
}

impl LifecycleOperationCommon {
    fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported lifecycle operation schema");
        ensure!(
            self.protocol == LIFECYCLE_ADAPTER_PROTOCOL,
            "unsupported lifecycle adapter protocol"
        );
        bounded_identifier(&self.operation_id, 256, "lifecycle operation id")?;
        digest(&self.binding_hash, "lifecycle binding hash")?;
        digest(&self.settings_digest, "lifecycle settings digest")
    }
}

impl AllocationReservation {
    fn validate(&self) -> Result<()> {
        bounded_text(&self.placement_thread_id, 256, "external placement")?;
        for (value, label) in [
            (&self.admitted_capsule_hash, "admitted capsule"),
            (&self.base_snapshot_hash, "base snapshot"),
            (&self.request_digest, "allocation request"),
        ] {
            digest(value, label)?;
        }
        ensure!(
            (1..=3_600).contains(&self.maximum_lifetime_seconds) && self.contact_deadline_ms > 0,
            "allocation reservation lifecycle bounds are invalid"
        );
        Ok(())
    }
}

impl BoundOccurrence {
    fn validate(&self) -> Result<()> {
        digest(&self.request_digest, "allocation request")?;
        bounded_text(&self.occurrence_id, 512, "external occurrence id")
    }
}

impl SupervisorActivationIntent {
    fn validate(&self) -> Result<()> {
        for (value, label) in [
            (&self.activation_request_digest, "activation request"),
            (&self.supervisor_runtime_hash, "supervisor runtime"),
            (&self.launcher_artifact_hash, "launcher artifact"),
        ] {
            digest(value, label)?;
        }
        ensure!(
            self.attachment_deadline_ms > 0
                && (1..=3_600).contains(&self.execution_timeout_seconds)
                && (1..=900).contains(&self.post_execution_timeout_seconds)
                && (1..=64 * 1024 * 1024).contains(&self.channel_max_bytes),
            "supervisor activation bounds are invalid"
        );
        Ok(())
    }
}

impl TerminationIntent {
    fn validate(&self) -> Result<()> {
        digest(&self.termination_request_digest, "termination request")
    }
}

pub fn from_json_slice_strict<T: DeserializeOwned>(input: &[u8], maximum: usize) -> Result<T> {
    ensure!(
        !input.is_empty() && input.len() <= maximum,
        "JSON message exceeds its byte bound"
    );
    let mut deserializer = serde_json::Deserializer::from_slice(input);
    let value = StrictJsonValue { depth: 0 }
        .deserialize(&mut deserializer)
        .context("decode strict lifecycle JSON")?;
    deserializer
        .end()
        .context("lifecycle JSON has trailing bytes")?;
    serde_json::from_value(value).context("decode lifecycle message")
}

pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(value)?)
}

fn validate_inspection_common(
    schema: u32,
    protocol: &str,
    adapter_id: &str,
    adapter_hash: &str,
    settings_schema_digest: &str,
    target: &str,
) -> Result<()> {
    ensure!(schema == 1, "unsupported lifecycle inspection schema");
    ensure!(
        protocol == LIFECYCLE_ADAPTER_PROTOCOL,
        "unsupported lifecycle adapter protocol"
    );
    bounded_identifier(adapter_id, 128, "lifecycle adapter id")?;
    digest(adapter_hash, "lifecycle adapter artifact")?;
    digest(settings_schema_digest, "lifecycle settings schema")?;
    bounded_identifier(target, 128, "lifecycle adapter target")
}

fn bounded_identifier(value: &str, maximum: usize, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= maximum
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "{label} is invalid"
    );
    Ok(())
}

fn validate_targets(targets: &[String]) -> Result<()> {
    ensure!(
        !targets.is_empty() && targets.len() <= 16,
        "external execution declaration has an invalid target set"
    );
    let mut unique = BTreeSet::new();
    for target in targets {
        bounded_identifier(target, 128, "external execution target")?;
        ensure!(
            unique.insert(target.as_str()),
            "external execution declaration repeats target `{target}`"
        );
    }
    Ok(())
}

fn validate_executable_name(value: &str, label: &str) -> Result<()> {
    bounded_identifier(value, 128, label)?;
    ensure!(
        value != "." && value != ".." && !value.contains('/'),
        "{label} must be a bare signed-bundle executable name"
    );
    Ok(())
}

fn validate_absolute_normalized_path(value: &str, label: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(
        path.is_absolute()
            && value.len() <= 4096
            && !value.as_bytes().contains(&0)
            && path
                .components()
                .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
            && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str(),
        "{label} is not an absolute normalized path"
    );
    Ok(())
}

fn validate_single_file_name(value: &str, label: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(
        !value.is_empty()
            && value.len() <= 255
            && !value.as_bytes().contains(&0)
            && !path.is_absolute()
            && path.components().count() == 1
            && matches!(path.components().next(), Some(Component::Normal(_))),
        "{label} is not a canonical single file name"
    );
    Ok(())
}

fn validate_bundle_relative_file_path(value: &str, label: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(
        !value.is_empty()
            && value.len() <= 512
            && !value.as_bytes().contains(&0)
            && !value.chars().any(char::is_control)
            && !value.contains('\\')
            && !path.is_absolute()
            && path
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str(),
        "{label} is not a normalized bundle-relative file path"
    );
    Ok(())
}

fn bounded_text(value: &str, maximum: usize, label: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control),
        "{label} is invalid"
    );
    Ok(())
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        && !name.contains('=')
        && !name.contains('\0')
}

fn digest(value: &str, label: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} is invalid"
    );
    Ok(())
}

struct StrictJsonValue {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for StrictJsonValue {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonValueVisitor { depth: self.depth })
    }
}

struct StrictJsonValueVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for StrictJsonValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("bounded JSON without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }
    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }
    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(A::Error::custom("lifecycle JSON nesting exceeds its bound"));
        }
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0));
        while let Some(value) = sequence.next_element_seed(StrictJsonValue {
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A>(self, mut mapping: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(A::Error::custom("lifecycle JSON nesting exceeds its bound"));
        }
        let mut values = serde_json::Map::new();
        while let Some(key) = mapping.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(A::Error::custom(format!(
                    "duplicate JSON object key `{key}`"
                )));
            }
            let value = mapping.next_value_seed(StrictJsonValue {
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct_mode() -> ExternalExecutionMode {
        ExternalExecutionMode::DirectCommand {
            stdout_max_bytes: 8,
            stderr_max_bytes: 4,
        }
    }

    fn command_termination() -> ExternalCommandTermination {
        let empty = ExternalCommandOutputCommitment {
            bytes: 0,
            sha256: hex::encode(Sha256::digest([])),
            truncated: false,
        };
        ExternalCommandTermination {
            target_exit: ExternalTargetExit::Code(0),
            reason: ExternalCommandTerminationReason::TargetExited,
            stdout: empty.clone(),
            stderr: empty,
        }
    }

    #[test]
    fn direct_command_contract_is_closed_and_bounded() {
        direct_mode().validate().unwrap();
        for mode in [
            ExternalExecutionMode::DirectCommand {
                stdout_max_bytes: 0,
                stderr_max_bytes: 1,
            },
            ExternalExecutionMode::DirectCommand {
                stdout_max_bytes: u64::MAX,
                stderr_max_bytes: 1,
            },
            ExternalExecutionMode::DirectCommand {
                stdout_max_bytes: 64 * 1024 * 1024,
                stderr_max_bytes: 1,
            },
        ] {
            assert!(mode.validate().is_err());
        }
        for value in [
            serde_json::json!({"kind":"direct_command","stdout_max_bytes":8}),
            serde_json::json!({"kind":"direct_command","stdout_max_bytes":8,"stderr_max_bytes":4,"fallback":true}),
            serde_json::json!({"kind":"structured_session","stdout_max_bytes":8}),
            serde_json::json!({"kind":"unknown"}),
        ] {
            assert!(serde_json::from_value::<ExternalExecutionMode>(value).is_err());
        }
    }

    #[test]
    fn command_termination_cannot_substitute_eof_or_cleanup() {
        let original = command_termination();
        original.validate(direct_mode()).unwrap();
        assert!(
            original
                .validate(ExternalExecutionMode::StructuredSession {})
                .is_err()
        );
        for exit in [
            ExternalTargetExit::Code(-1),
            ExternalTargetExit::Code(256),
            ExternalTargetExit::Signal(0),
            ExternalTargetExit::Signal(65),
        ] {
            let mut changed = original.clone();
            changed.target_exit = exit;
            assert!(changed.validate(direct_mode()).is_err());
        }
        for exit in [ExternalTargetExit::Code(1), ExternalTargetExit::Signal(9)] {
            let mut changed = original.clone();
            changed.target_exit = exit;
            changed.validate(direct_mode()).unwrap();
        }
        let mut value = serde_json::to_value(&original).unwrap();
        value.as_object_mut().unwrap().remove("target_exit");
        assert!(serde_json::from_value::<ExternalCommandTermination>(value).is_err());
        let mut value = serde_json::to_value(&original).unwrap();
        value["namespace_exit"] = serde_json::json!({"kind":"code","value":0});
        assert!(serde_json::from_value::<ExternalCommandTermination>(value).is_err());
    }

    #[test]
    fn command_termination_commits_bounded_outputs_without_false_success() {
        let mut value = command_termination();
        value.stdout.sha256 = "a".repeat(64);
        assert!(value.validate(direct_mode()).is_err());
        value.stdout.bytes = 9;
        assert!(value.validate(direct_mode()).is_err());
        value.stdout.bytes = 8;
        value.stdout.truncated = true;
        assert!(value.validate(direct_mode()).is_err());
        value.reason = ExternalCommandTerminationReason::OutputLimit;
        value.validate(direct_mode()).unwrap();
        value.stdout.bytes = 7;
        assert!(value.validate(direct_mode()).is_err());
        value.stdout.bytes = 8;
        value.stdout.truncated = false;
        assert!(value.validate(direct_mode()).is_err());
        for reason in [
            ExternalCommandTerminationReason::Cancelled,
            ExternalCommandTerminationReason::Deadline,
            ExternalCommandTerminationReason::Fault,
        ] {
            value.reason = reason;
            // A code-zero target does not erase the non-successful reason.
            value.validate(direct_mode()).unwrap();
            assert_eq!(value.target_exit, ExternalTargetExit::Code(0));
        }
    }

    fn guest_projection() -> ExternalGuestInputProjection {
        ExternalGuestInputProjection {
            schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
            base_snapshot: GuestBaseSnapshotInput {
                descriptor: 10,
                snapshot_hash: "1".repeat(64),
                closure_digest: "2".repeat(64),
                object_count: 3,
                blob_count: 1,
                total_bytes: 1,
            },
            workspace_outputs: None,
            inputs: vec![GuestMountInput {
                role: GuestMountRole::Product,
                authority_id: "runtime".into(),
                descriptor: 11,
                destination: "/runtime/bin/python".into(),
                kind: GuestMountKind::RegularFile,
                access: GuestMountAccess::ReadOnly,
                normalized_mode: Some(0o755),
                content_authority: GuestMountContentAuthority::ProductManifest {
                    manifest_kind: GuestProductManifestKind::Content,
                    manifest_hash: "3".repeat(64),
                    manifest_descriptor: 12,
                    manifest_bytes: 256,
                },
                bytes: 1,
            }],
            executable_search: Vec::new(),
            environment: BTreeMap::new(),
        }
    }

    fn source_projection() -> ExternalGuestInputProjection {
        let mut projection = guest_projection();
        projection.inputs.push(GuestMountInput {
            role: GuestMountRole::Source,
            authority_id: "evaluator-source".into(),
            descriptor: 13,
            destination: "/source/evaluator".into(),
            kind: GuestMountKind::Directory,
            access: GuestMountAccess::ReadOnly,
            normalized_mode: None,
            content_authority: GuestMountContentAuthority::SourceClosure {
                binding_hash: "4".repeat(64),
                binding_descriptor: 14,
                binding_bytes: 256,
                manifest_hash: "5".repeat(64),
                manifest_descriptor: 15,
                manifest_bytes: 512,
            },
            bytes: 1024,
        });
        projection
    }

    #[test]
    fn source_closure_has_exact_ordered_records_and_no_product_alias() {
        let projection = source_projection();
        projection.validate().unwrap();
        let expected = vec![
            (12, "3".repeat(64), 256),
            (14, "4".repeat(64), 256),
            (15, "5".repeat(64), 512),
        ];
        assert_eq!(
            projection
                .record_descriptors()
                .map(|(fd, hash, bytes)| (fd, hash.to_owned(), bytes))
                .collect::<Vec<_>>(),
            expected
        );
        let bytes = projection.canonical_bytes().unwrap();
        assert_eq!(
            from_json_slice_strict::<ExternalGuestInputProjection>(
                &bytes,
                MAX_GUEST_INPUT_PROJECTION_BYTES
            )
            .unwrap(),
            projection
        );

        let mut old = projection.clone();
        old.schema = 3;
        assert!(old.validate().is_err());
        let mut fake_source = projection.clone();
        fake_source.inputs[1].content_authority = projection.inputs[0].content_authority.clone();
        // Give the fake product a distinct descriptor: refusal must be role-based.
        if let GuestMountContentAuthority::ProductManifest {
            manifest_descriptor,
            ..
        } = &mut fake_source.inputs[1].content_authority
        {
            *manifest_descriptor = 16;
        }
        assert!(
            fake_source
                .validate()
                .unwrap_err()
                .to_string()
                .contains("role contradicts")
        );
        for role in [
            GuestMountRole::Product,
            GuestMountRole::Configuration,
            GuestMountRole::PrivateScratch,
        ] {
            let mut invalid = projection.clone();
            invalid.inputs[1].role = role;
            assert!(
                invalid
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains("role contradicts")
            );
        }
    }

    #[test]
    fn source_closure_refuses_wrong_kind_access_mode_and_bounds() {
        let projection = source_projection();
        let mut invalid = projection.clone();
        invalid.inputs[1].kind = GuestMountKind::RegularFile;
        invalid.inputs[1].normalized_mode = Some(0o644);
        assert!(invalid.validate().is_err());
        let mut invalid = projection.clone();
        invalid.inputs[1].access = GuestMountAccess::PrivateWritable;
        assert!(invalid.validate().is_err());
        let mut invalid = projection.clone();
        invalid.inputs[1].normalized_mode = Some(0o755);
        assert!(invalid.validate().is_err());
        let mut boundary = projection.clone();
        boundary.inputs[1].bytes = MAX_GUEST_SOURCE_BYTES;
        boundary.validate().unwrap();
        boundary.inputs[1].bytes += 1;
        assert!(boundary.validate().is_err());
        for bound in [0, MAX_GUEST_SOURCE_RECORD_BYTES + 1] {
            for binding in [false, true] {
                let mut invalid = projection.clone();
                if let GuestMountContentAuthority::SourceClosure {
                    binding_bytes,
                    manifest_bytes,
                    ..
                } = &mut invalid.inputs[1].content_authority
                {
                    if binding {
                        *binding_bytes = bound;
                    } else {
                        *manifest_bytes = bound;
                    }
                }
                assert!(invalid.validate().is_err());
            }
        }
        let mut boundary = projection;
        if let GuestMountContentAuthority::SourceClosure {
            binding_bytes,
            manifest_bytes,
            ..
        } = &mut boundary.inputs[1].content_authority
        {
            *binding_bytes = MAX_GUEST_SOURCE_RECORD_BYTES;
            *manifest_bytes = MAX_GUEST_SOURCE_RECORD_BYTES;
        }
        boundary.validate().unwrap();
    }

    #[test]
    fn source_record_descriptors_are_unique_and_hashes_are_required() {
        let projection = source_projection();
        for fd in [0, 1, 2, 10, 11, 12, 13, i32::MAX as u32 + 1, u32::MAX] {
            for binding in [false, true] {
                let mut invalid = projection.clone();
                if let GuestMountContentAuthority::SourceClosure {
                    binding_descriptor,
                    manifest_descriptor,
                    ..
                } = &mut invalid.inputs[1].content_authority
                {
                    if binding {
                        *binding_descriptor = fd;
                    } else {
                        *manifest_descriptor = fd;
                    }
                }
                assert!(invalid.validate().is_err());
            }
        }
        let mut duplicate = projection.clone();
        if let GuestMountContentAuthority::SourceClosure {
            binding_descriptor,
            manifest_descriptor,
            ..
        } = &mut duplicate.inputs[1].content_authority
        {
            *binding_descriptor = *manifest_descriptor;
        }
        assert!(duplicate.validate().is_err());
        for binding in [false, true] {
            let mut invalid = projection.clone();
            if let GuestMountContentAuthority::SourceClosure {
                binding_hash,
                manifest_hash,
                ..
            } = &mut invalid.inputs[1].content_authority
            {
                if binding {
                    *binding_hash = "invalid".into();
                } else {
                    *manifest_hash = "invalid".into();
                }
            }
            assert!(invalid.validate().is_err());
        }
    }

    #[test]
    fn source_identity_ignores_fd_relocation_but_commits_each_record() {
        let projection = source_projection();
        let identity = projection.identity_digest().unwrap();
        let mut relocated = projection.clone();
        relocated.base_snapshot.descriptor = 20;
        relocated.inputs[1].descriptor = 21;
        if let GuestMountContentAuthority::SourceClosure {
            binding_descriptor,
            manifest_descriptor,
            ..
        } = &mut relocated.inputs[1].content_authority
        {
            *binding_descriptor = 22;
            *manifest_descriptor = 23;
        }
        assert_eq!(relocated.identity_digest().unwrap(), identity);
        for field in [
            "binding_hash",
            "manifest_hash",
            "binding_bytes",
            "manifest_bytes",
        ] {
            let mut value = serde_json::to_value(&projection).unwrap();
            value["inputs"][1]["content_authority"][field] = if field.ends_with("hash") {
                serde_json::json!("6".repeat(64))
            } else {
                serde_json::json!(1024)
            };
            let changed: ExternalGuestInputProjection = serde_json::from_value(value).unwrap();
            assert_ne!(changed.identity_digest().unwrap(), identity, "{field}");
        }
    }

    fn common() -> LifecycleOperationCommon {
        LifecycleOperationCommon {
            schema: 1,
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
            operation_id: "allocation-1".into(),
            binding_hash: "a".repeat(64),
            settings_digest: "b".repeat(64),
        }
    }

    fn reservation() -> AllocationReservation {
        AllocationReservation {
            placement_thread_id: "T-placement".into(),
            admitted_capsule_hash: "c".repeat(64),
            base_snapshot_hash: "d".repeat(64),
            request_digest: "e".repeat(64),
            maximum_lifetime_seconds: 60,
            contact_deadline_ms: 1,
        }
    }

    #[test]
    fn signed_declarations_bind_a_closed_bundle_relative_provider_spec() {
        let provider = ExternalProviderDeclaration {
            id: "codex-hosted".into(),
            protocol: PROVIDER_CONFIGURATION_PROTOCOL.into(),
            targets: vec!["x86_64-unknown-linux-gnu".into()],
            connector: "ryeos-external-candidate-connector".into(),
            connector_process_group: ExternalProviderConnectorProcessGroup::New,
            configuration_adapter: "ryeos-codex-external-configuration".into(),
            configuration_destination: "environments.toml".into(),
        };
        provider.validate().unwrap();

        let lifecycle = ExternalLifecycleAdapterDeclaration {
            id: "synthetic".into(),
            protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
            targets: vec!["x86_64-unknown-linux-gnu".into()],
            adapter: "ryeos-external-lifecycle-synthetic".into(),
            supervisor: "ryeos-external-candidate-supervisor".into(),
            launcher: "ryeos-external-candidate-launcher".into(),
            provider_spec: LifecycleProviderSpecIdentity {
                path: "lifecycle/provider.json".into(),
                sha256: "f".repeat(64),
            },
            settings_schema_digest: "a".repeat(64),
            capabilities: BTreeSet::from([
                LifecycleCapability::ExactAllocationReconciliation,
                LifecycleCapability::AuthoritativeNoOccurrence,
            ]),
        };
        lifecycle.validate().unwrap();

        let mut invalid = provider;
        invalid.connector = "../ambient".into();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn allocation_wire_is_closed_and_exactly_correlated() {
        let request = LifecycleAdapterRequest::Allocate {
            common: common(),
            reservation: reservation(),
        };
        let bytes = request.canonical_bytes().unwrap();
        let decoded: LifecycleAdapterRequest =
            from_json_slice_strict(&bytes, MAX_LIFECYCLE_REQUEST_BYTES).unwrap();
        assert_eq!(decoded, request);
        let response = LifecycleAdapterResponse::AllocationBound {
            operation_id: "allocation-1".into(),
            request_digest: "e".repeat(64),
            occurrence_id: "sbx-exact".into(),
            provider_observation_digest: "f".repeat(64),
        };
        response.validate_for(&request).unwrap();
        let mut wrong = response.clone();
        if let LifecycleAdapterResponse::AllocationBound { request_digest, .. } = &mut wrong {
            *request_digest = "0".repeat(64);
        }
        assert!(wrong.validate_for(&request).is_err());
    }

    #[test]
    fn operation_mismatch_and_unknown_or_duplicate_fields_refuse() {
        let request = LifecycleAdapterRequest::Terminate {
            common: common(),
            occurrence: BoundOccurrence {
                request_digest: "e".repeat(64),
                occurrence_id: "occurrence".into(),
            },
            termination: TerminationIntent {
                termination_request_digest: "f".repeat(64),
            },
        };
        let wrong = LifecycleAdapterResponse::AllocationPending {
            operation_id: "allocation-1".into(),
            request_digest: "e".repeat(64),
        };
        assert!(wrong.validate_for(&request).is_err());
        let unknown = br#"{"operation":"allocate","common":{},"reservation":{},"extra":true}"#;
        assert!(from_json_slice_strict::<LifecycleAdapterRequest>(unknown, 1024).is_err());
        let duplicate = br#"{"operation":"allocate","operation":"terminate"}"#;
        assert!(from_json_slice_strict::<Value>(duplicate, 1024).is_err());
    }

    #[test]
    fn contract_carries_no_ambient_path_command_url_or_secret_field() {
        let request = LifecycleAdapterRequest::Allocate {
            common: common(),
            reservation: reservation(),
        };
        let encoded = String::from_utf8(request.canonical_bytes().unwrap()).unwrap();
        for forbidden in ["path", "command", "url", "credential", "secret", "token"] {
            assert!(!encoded.contains(forbidden), "wire contains {forbidden}");
        }
    }

    #[test]
    fn guest_regular_file_mode_is_required_validated_and_identity_bearing() {
        let executable = guest_projection();
        executable.validate().unwrap();
        let executable_identity = executable.identity_digest().unwrap();

        let mut data = executable.clone();
        data.inputs[0].normalized_mode = Some(0o644);
        data.validate().unwrap();
        assert_ne!(data.identity_digest().unwrap(), executable_identity);

        for mode in [None, Some(0o600), Some(0o777)] {
            let mut invalid = executable.clone();
            invalid.inputs[0].normalized_mode = mode;
            assert!(invalid.validate().is_err());
        }

        let mut directory = executable;
        directory.inputs[0].kind = GuestMountKind::Directory;
        directory.inputs[0].destination = "/runtime".into();
        assert!(directory.validate().is_err());
        directory.inputs[0].normalized_mode = None;
        directory.validate().unwrap();
    }

    #[test]
    fn guest_executable_search_never_uses_private_scratch() {
        let mut projection = guest_projection();
        projection.inputs[0].kind = GuestMountKind::Directory;
        projection.inputs[0].destination = "/runtime".into();
        projection.inputs[0].normalized_mode = None;
        projection.executable_search = vec!["/runtime/bin".into()];
        projection.validate().unwrap();

        let mut writable = projection.clone();
        writable.inputs[0].role = GuestMountRole::PrivateScratch;
        writable.inputs[0].access = GuestMountAccess::PrivateWritable;
        writable.inputs[0].content_authority = GuestMountContentAuthority::PrivateScratch {
            binding_hash: "4".repeat(64),
        };
        assert!(writable.validate().is_err());

        let mut configuration = projection;
        configuration.inputs[0].role = GuestMountRole::Configuration;
        configuration.inputs[0].content_authority = GuestMountContentAuthority::RawFile {
            sha256: "4".repeat(64),
        };
        assert!(configuration.validate().is_err());
    }

    #[test]
    fn separate_command_product_is_part_of_guest_identity() {
        let mut projection = guest_projection();
        projection.inputs.push(GuestMountInput {
            role: GuestMountRole::Product,
            authority_id: "command-tools".into(),
            descriptor: 13,
            destination: "/tools".into(),
            kind: GuestMountKind::Directory,
            access: GuestMountAccess::ReadOnly,
            normalized_mode: None,
            content_authority: GuestMountContentAuthority::ProductManifest {
                manifest_kind: GuestProductManifestKind::Content,
                manifest_hash: "4".repeat(64),
                manifest_descriptor: 14,
                manifest_bytes: 256,
            },
            bytes: 1,
        });
        projection.executable_search = vec!["/tools/bin".into()];
        projection
            .environment
            .insert("PATH".into(), "/tools/bin".into());
        projection.validate().unwrap();
        let identity = projection.identity_digest().unwrap();

        projection.inputs[1].content_authority = GuestMountContentAuthority::ProductManifest {
            manifest_kind: GuestProductManifestKind::Content,
            manifest_hash: "5".repeat(64),
            manifest_descriptor: 14,
            manifest_bytes: 256,
        };
        assert_ne!(projection.identity_digest().unwrap(), identity);
    }

    #[test]
    fn product_manifest_tier_is_identity_bearing_but_descriptor_coordinate_is_not() {
        let input = guest_projection();
        let identity = input.identity_digest().unwrap();
        let mut remapped = input.clone();
        if let GuestMountContentAuthority::ProductManifest {
            manifest_descriptor,
            ..
        } = &mut remapped.inputs[0].content_authority
        {
            *manifest_descriptor = 63;
        }
        assert_eq!(remapped.identity_digest().unwrap(), identity);

        let mut large = input.clone();
        if let GuestMountContentAuthority::ProductManifest { manifest_kind, .. } =
            &mut large.inputs[0].content_authority
        {
            *manifest_kind = GuestProductManifestKind::LargeContent;
        }
        assert_ne!(large.identity_digest().unwrap(), identity);

        let mut duplicate = input.clone();
        let input_descriptor = duplicate.inputs[0].descriptor;
        if let GuestMountContentAuthority::ProductManifest {
            manifest_descriptor,
            ..
        } = &mut duplicate.inputs[0].content_authority
        {
            *manifest_descriptor = input_descriptor;
        }
        assert!(duplicate.validate().is_err());

        let mut untyped = input;
        untyped.inputs[0].content_authority = GuestMountContentAuthority::RawFile {
            sha256: "3".repeat(64),
        };
        assert!(untyped.validate().is_err());
    }

    #[test]
    fn guest_input_mode_is_required_even_when_null() {
        let encoded = serde_json::to_value(guest_projection()).unwrap();
        let mut object = encoded.as_object().unwrap().clone();
        let inputs = object.get_mut("inputs").unwrap().as_array_mut().unwrap();
        inputs[0].as_object_mut().unwrap().remove("normalized_mode");
        let bytes = serde_json::to_vec(&object).unwrap();
        assert!(
            from_json_slice_strict::<ExternalGuestInputProjection>(
                &bytes,
                MAX_GUEST_INPUT_PROJECTION_BYTES,
            )
            .is_err()
        );
    }
}
