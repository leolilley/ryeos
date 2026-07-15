//! Node-owned subprocess sandbox policy and its immutable runtime form.
//!
//! The policy has one fixed source: `<app-root>/.ai/node/sandbox.yaml`.
//! [`SandboxRuntime::load`] reads, strictly parses, and resolves that policy
//! once. Launch paths then share the resolved runtime and call [`SandboxRuntime::apply`]
//! without reopening node configuration at the process boundary.

use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

use crate::canonical_ref::CanonicalRef;
use crate::error::EngineError;

pub const SANDBOX_POLICY_VERSION: u32 = 1;
pub const SANDBOX_POLICY_RELATIVE_PATH: &str = "node/sandbox.yaml";
const VERIFIED_CODE_SANDBOX_ROOT: &str = "/run/ryeos/verified-code";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxPolicy {
    pub version: u32,
    pub mode: SandboxMode,
    pub backend: SandboxBackendPolicy,
    pub filesystem: SandboxFilesystemPolicy,
    pub network: SandboxNetworkPolicy,
    pub environment: SandboxEnvironmentPolicy,
    pub limits: SandboxLimitsPolicy,
}

impl SandboxPolicy {
    /// Canonical first-init and in-memory-fixture policy. Keeping this typed
    /// value in the engine gives node init and `SandboxRuntime::default()` one
    /// source of truth while leaving the on-disk file create-once.
    pub fn default_disabled() -> Self {
        Self {
            version: SANDBOX_POLICY_VERSION,
            mode: SandboxMode::Disabled,
            backend: SandboxBackendPolicy {
                kind: SandboxBackendKind::Bubblewrap,
                executable: PathBuf::from("/usr/bin/bwrap"),
            },
            filesystem: SandboxFilesystemPolicy {
                readable: vec![
                    "{node_public_identity}".to_string(),
                    "{daemon_socket}".to_string(),
                    "{bundle_roots}".to_string(),
                    "{node_trusted_keys}".to_string(),
                    "{verified_code}".to_string(),
                ],
                writable: vec!["{project}".to_string(), "{checkpoint_dir}".to_string()],
            },
            network: SandboxNetworkPolicy {
                mode: SandboxNetworkMode::Host,
            },
            environment: SandboxEnvironmentPolicy {
                allow: vec!["*".to_string()],
            },
            limits: SandboxLimitsPolicy {
                open_files: Some(1024),
                stdout_bytes: 8_388_608,
                stderr_bytes: 8_388_608,
                verified_artifact_file_bytes: 67_108_864,
                verified_artifact_total_bytes: 268_435_456,
                verified_artifact_files: 4_096,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    Disabled,
    Enforce,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxBackendPolicy {
    pub kind: SandboxBackendKind,
    pub executable: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackendKind {
    Bubblewrap,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxFilesystemPolicy {
    pub readable: Vec<String>,
    pub writable: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxNetworkPolicy {
    pub mode: SandboxNetworkMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxNetworkMode {
    Host,
    Isolated,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxEnvironmentPolicy {
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SandboxLimitsPolicy {
    pub open_files: Option<u64>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub verified_artifact_file_bytes: u64,
    pub verified_artifact_total_bytes: u64,
    pub verified_artifact_files: u64,
}

/// Backend resolution facts and the exact policy snapshot used by a runtime.
///
/// Doctor and status surfaces consume this value rather than reparsing the
/// source file with a second implementation. Enforced policy loading captures
/// the configured backend immediately. A disabled snapshot captures it during
/// successful runtime admission only when the registry requires a mandatory
/// private launch-preparer profile.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SandboxInspection {
    pub source: Option<PathBuf>,
    pub version: u32,
    pub mode: SandboxMode,
    pub digest: Option<String>,
    pub backend: SandboxBackendInspection,
    pub filesystem: SandboxFilesystemPolicy,
    pub network: SandboxNetworkPolicy,
    pub environment: SandboxEnvironmentPolicy,
    pub limits: SandboxLimitsPolicy,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SandboxBackendInspection {
    pub kind: SandboxBackendKind,
    pub configured_executable: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_executable: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxRuntimeState {
    Disabled,
    Enforced,
}

/// A strictly parsed, immutable sandbox snapshot shared by process launches.
#[derive(Debug, Clone)]
pub struct SandboxRuntime {
    inspection: SandboxInspection,
    state: SandboxRuntimeState,
    /// Canonical host path used for authority and overlap checks.
    app_root: Option<PathBuf>,
    /// Exact app-root directory inode captured while the policy was loaded.
    app_root_authority: Option<Arc<lillux::PinnedDirectory>>,
    /// Exact daemon-owned parent of runtime workspace project directories.
    runtime_workspaces: Option<Arc<lillux::PinnedDirectory>>,
    /// Node-configured spelling recreated inside the sandbox namespace.
    app_root_destination: Option<PathBuf>,
    daemon_socket: Option<PinnedDaemonSocket>,
    verified_artifacts: Option<Arc<VerifiedArtifactStore>>,
    /// Exact daemon-lifetime backend capture used by every enforced apply and
    /// every mandatory private profile, irrespective of the general mode.
    /// Disabled snapshots publish this once when an admitted runtime registry
    /// first requires a private profile, so every existing clone and later
    /// engine rebuild reuses the same bytes.
    backend_capture: Arc<OnceLock<CapturedSandboxBackend>>,
}

#[derive(Debug, Clone)]
struct CapturedSandboxBackend {
    resolved_executable: PathBuf,
    handle: Arc<std::fs::File>,
    digest: String,
    version: String,
}

#[derive(Debug, Clone)]
struct PinnedDaemonSocket {
    source: PathBuf,
    destination: PathBuf,
    parent: Arc<lillux::PinnedDirectory>,
    name: std::ffi::OsString,
    entry: Arc<std::fs::File>,
}

#[derive(Debug)]
struct VerifiedArtifactStore {
    root: lillux::PinnedDirectory,
    stores_root: lillux::PinnedDirectory,
    generation: std::ffi::OsString,
    max_file_bytes: u64,
    max_total_bytes: u64,
    max_files: u64,
    usage: std::sync::Mutex<VerifiedArtifactUsage>,
    #[cfg(unix)]
    _lifetime_lock: std::fs::File,
}

#[derive(Debug)]
struct MaterializedArtifact {
    path: PathBuf,
    handle: Arc<std::fs::File>,
}

#[derive(Debug, Default)]
struct VerifiedArtifactUsage {
    total_bytes: u64,
    files: u64,
    entries: std::collections::HashMap<String, (String, u64)>,
}

impl Drop for VerifiedArtifactStore {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;

            if let Ok(Some(cleanup_lock)) = self
                .stores_root
                .open_regular(".cleanup.lock".as_ref(), true)
            {
                if unsafe { libc::flock(cleanup_lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
                    let _ = remove_flat_artifact_generation(
                        &self.stores_root,
                        &self.generation,
                        &self.root,
                    );
                }
            }
            return;
        }
    }
}

impl VerifiedArtifactStore {
    fn create(
        app_root: &lillux::PinnedDirectory,
        limits: &SandboxLimitsPolicy,
    ) -> Result<Self, EngineError> {
        #[cfg(not(unix))]
        {
            let _ = (app_root, limits);
            return Err(refused(
                "verified-code artifact stores require Unix file locking".to_string(),
            ));
        }

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            use std::os::unix::fs::PermissionsExt as _;
            use std::sync::atomic::{AtomicU64, Ordering};

            static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(0);

            let stores_root = open_or_create_relative_directory(
                app_root,
                &[crate::AI_DIR, "state", "cache", "verified-code"],
                0o700,
                "verified-code store root",
            )?;
            stores_root.set_mode(0o700).map_err(|error| {
                refused(format!(
                    "verified-code store root {} cannot be protected: {error}",
                    stores_root.path().display()
                ))
            })?;

            // Serialize generation creation with stale-generation cleanup so
            // another process never observes a new directory before its
            // lifetime lock is held.
            let cleanup_lock = stores_root
                .open_regular_create(".cleanup.lock".as_ref(), true, false, 0o600)
                .map_err(|error| {
                    refused(format!(
                        "verified-code cleanup lock {} cannot be opened: {error}",
                        stores_root.path().join(".cleanup.lock").display()
                    ))
                })?;
            cleanup_lock
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| {
                    refused(format!(
                        "verified-code cleanup lock cannot be protected: {error}"
                    ))
                })?;
            if unsafe { libc::flock(cleanup_lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(refused(format!(
                    "verified-code cleanup lock {} cannot be acquired: {}",
                    stores_root.path().join(".cleanup.lock").display(),
                    std::io::Error::last_os_error()
                )));
            }

            let generation = format!(
                "{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed)
            );
            let generation = std::ffi::OsString::from(generation);
            let root = stores_root
                .create_child(&generation, 0o700)
                .map_err(|error| {
                    refused(format!(
                        "verified-code generation cannot be created: {error}"
                    ))
                })?;
            root.set_mode(0o700).map_err(|error| {
                refused(format!(
                    "verified-code generation {} cannot be protected: {error}",
                    root.path().display()
                ))
            })?;
            let lifetime_lock = root
                .open_regular_create(".lifetime.lock".as_ref(), true, true, 0o600)
                .map_err(|error| {
                    refused(format!(
                        "verified-code lifetime lock {} cannot be opened: {error}",
                        root.path().join(".lifetime.lock").display()
                    ))
                })?;
            lifetime_lock
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| {
                    refused(format!(
                        "verified-code lifetime lock cannot be protected: {error}"
                    ))
                })?;
            if unsafe { libc::flock(lifetime_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
            {
                return Err(refused(format!(
                    "verified-code lifetime lock {} cannot be acquired: {}",
                    root.path().join(".lifetime.lock").display(),
                    std::io::Error::last_os_error()
                )));
            }

            for name in stores_root.entry_names().map_err(|error| {
                refused(format!(
                    "verified-code store root {} cannot be read: {error}",
                    stores_root.path().display()
                ))
            })? {
                if name == generation || name == ".cleanup.lock" {
                    continue;
                }
                let Some(stale_root) =
                    stores_root.open_child_directory(&name).map_err(|error| {
                        refused(format!(
                            "verified-code store entry cannot be inspected: {error}"
                        ))
                    })?
                else {
                    return Err(refused(format!(
                        "verified-code store contains unsupported entry {}",
                        stores_root.path().join(&name).display()
                    )));
                };
                let stale_guard = match stale_root.open_regular(".lifetime.lock".as_ref(), true) {
                    Ok(Some(stale_lock))
                        if unsafe {
                            libc::flock(stale_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0
                        } =>
                    {
                        Some(Some(stale_lock))
                    }
                    Ok(Some(_)) => None,
                    Ok(None) => Some(None),
                    Err(error) => {
                        return Err(refused(format!(
                            "stale verified-code lifetime lock cannot be opened: {error}"
                        )))
                    }
                };
                if let Some(_stale_guard) = stale_guard {
                    remove_flat_artifact_generation(&stores_root, &name, &stale_root).map_err(
                        |error| {
                            refused(format!(
                                "stale verified-code generation {} cannot be removed: {error}",
                                stale_root.path().display()
                            ))
                        },
                    )?;
                }
            }

            Ok(Self {
                root,
                stores_root,
                generation,
                max_file_bytes: limits.verified_artifact_file_bytes,
                max_total_bytes: limits.verified_artifact_total_bytes,
                max_files: limits.verified_artifact_files,
                usage: std::sync::Mutex::new(VerifiedArtifactUsage::default()),
                _lifetime_lock: lifetime_lock,
            })
        }
    }

    fn read_source(
        &self,
        kind: &str,
        path: &Path,
    ) -> Result<(Vec<u8>, std::fs::Metadata), EngineError> {
        read_regular_file_bytes_limited(kind, path, self.max_file_bytes)
    }

    fn materialize(
        &self,
        name: &str,
        expected_hash: &str,
        content: &[u8],
    ) -> Result<MaterializedArtifact, EngineError> {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(std::path::Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(refused(format!(
                "verified artifact name `{name}` is not one safe path component"
            )));
        }
        let content_len = u64::try_from(content.len()).map_err(|_| {
            refused(format!(
                "verified artifact `{name}` size cannot be represented"
            ))
        })?;
        if content_len > self.max_file_bytes {
            return Err(refused(format!(
                "verified artifact `{name}` is {content_len} bytes, exceeding per-file limit {}",
                self.max_file_bytes
            )));
        }
        let actual_hash = lillux::cas::sha256_hex(content);
        if actual_hash != expected_hash {
            return Err(refused(format!(
                "verified artifact `{name}` content hash mismatch (expected {expected_hash}, got {actual_hash})"
            )));
        }

        let mut usage = self
            .usage
            .lock()
            .map_err(|_| refused("verified artifact accounting lock is poisoned".to_string()))?;
        let artifact = self.root.path().join(name);
        if let Some((recorded_hash, recorded_len)) = usage.entries.get(name) {
            if recorded_hash != expected_hash || *recorded_len != content_len {
                return Err(refused(format!(
                    "verified artifact name `{name}` was reused for different content"
                )));
            }
            let file = self
                .root
                .open_regular(name.as_ref(), false)
                .map_err(|error| refused(format!("verified artifact cannot be opened: {error}")))?
                .ok_or_else(|| {
                    refused(format!(
                        "verified artifact {} disappeared",
                        artifact.display()
                    ))
                })?;
            protect_verified_artifact(&file, &artifact)?;
            let (existing, _) = read_regular_file_handle_limited(
                "verified artifact",
                &artifact,
                file.try_clone()
                    .map_err(|error| refused(error.to_string()))?,
                self.max_file_bytes,
            )?;
            if lillux::cas::sha256_hex(&existing) != expected_hash {
                return Err(refused(format!(
                    "verified artifact {} failed its content-address check",
                    artifact.display()
                )));
            }
            return Ok(MaterializedArtifact {
                path: artifact,
                handle: Arc::new(file),
            });
        }

        let next_files = usage
            .files
            .checked_add(1)
            .ok_or_else(|| refused("verified artifact file accounting overflowed".to_string()))?;
        let next_total = usage
            .total_bytes
            .checked_add(content_len)
            .ok_or_else(|| refused("verified artifact byte accounting overflowed".to_string()))?;
        if next_files > self.max_files {
            return Err(refused(format!(
                "verified artifact store would exceed file limit {}",
                self.max_files
            )));
        }
        if next_total > self.max_total_bytes {
            return Err(refused(format!(
                "verified artifact store would exceed total-byte limit {}",
                self.max_total_bytes
            )));
        }

        let file = if let Some(file) = self
            .root
            .open_regular(name.as_ref(), false)
            .map_err(|error| refused(format!("verified artifact cannot be opened: {error}")))?
        {
            let (existing, _) = read_regular_file_handle_limited(
                "verified artifact",
                &artifact,
                file.try_clone()
                    .map_err(|error| refused(error.to_string()))?,
                self.max_file_bytes,
            )?;
            if existing != content || lillux::cas::sha256_hex(&existing) != expected_hash {
                return Err(refused(format!(
                    "verified artifact {} exists with unexpected content",
                    artifact.display()
                )));
            }
            file
        } else {
            match self
                .root
                .atomic_create_regular(name.as_ref(), content, 0o500)
                .map_err(|error| {
                    refused(format!(
                        "verified artifact {} cannot be written: {error}",
                        artifact.display()
                    ))
                })? {
                Some(file) => file,
                None => {
                    let file = self
                        .root
                        .open_regular(name.as_ref(), false)
                        .map_err(|error| {
                            refused(format!("verified artifact cannot be opened: {error}"))
                        })?
                        .ok_or_else(|| {
                            refused(format!(
                                "verified artifact {} disappeared",
                                artifact.display()
                            ))
                        })?;
                    let (existing, _) = read_regular_file_handle_limited(
                        "verified artifact",
                        &artifact,
                        file.try_clone()
                            .map_err(|error| refused(error.to_string()))?,
                        self.max_file_bytes,
                    )?;
                    if existing != content || lillux::cas::sha256_hex(&existing) != expected_hash {
                        return Err(refused(format!(
                            "verified artifact {} exists with unexpected content",
                            artifact.display()
                        )));
                    }
                    file
                }
            }
        };
        protect_verified_artifact(&file, &artifact)?;
        let (captured, _) = read_regular_file_handle_limited(
            "verified artifact",
            &artifact,
            file.try_clone()
                .map_err(|error| refused(error.to_string()))?,
            self.max_file_bytes,
        )?;
        if lillux::cas::sha256_hex(&captured) != expected_hash {
            return Err(refused(format!(
                "verified artifact {} failed its content-address check",
                artifact.display()
            )));
        }
        usage.files = next_files;
        usage.total_bytes = next_total;
        usage
            .entries
            .insert(name.to_string(), (expected_hash.to_string(), content_len));
        Ok(MaterializedArtifact {
            path: artifact,
            handle: Arc::new(file),
        })
    }
}

fn protect_verified_artifact(file: &std::fs::File, path: &Path) -> Result<(), EngineError> {
    #[cfg(not(unix))]
    {
        let _ = (file, path);
        Err(refused(
            "verified artifacts require Unix file permissions".to_string(),
        ))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o500))
            .map_err(|error| {
                refused(format!(
                    "verified artifact {} cannot be protected: {error}",
                    path.display()
                ))
            })
    }
}

fn open_or_create_relative_directory(
    root: &lillux::PinnedDirectory,
    components: &[&str],
    mode: u32,
    kind: &str,
) -> Result<lillux::PinnedDirectory, EngineError> {
    let mut current = root
        .try_clone()
        .map_err(|error| refused(format!("{kind} authority cannot be cloned: {error}")))?;
    for component in components {
        current = current
            .open_or_create_child(component.as_ref(), mode)
            .map_err(|error| refused(format!("{kind} cannot open `{component}`: {error}")))?;
    }
    Ok(current)
}

fn open_relative_directory(
    root: &lillux::PinnedDirectory,
    components: &[&str],
    kind: &str,
) -> Result<lillux::PinnedDirectory, EngineError> {
    let mut current = root
        .try_clone()
        .map_err(|error| refused(format!("{kind} authority cannot be cloned: {error}")))?;
    for component in components {
        current = current
            .open_child_directory(component.as_ref())
            .map_err(|error| refused(format!("{kind} cannot open `{component}`: {error}")))?
            .ok_or_else(|| refused(format!("{kind} component `{component}` is missing")))?;
    }
    Ok(current)
}

fn remove_flat_artifact_generation(
    stores_root: &lillux::PinnedDirectory,
    generation: &std::ffi::OsStr,
    root: &lillux::PinnedDirectory,
) -> anyhow::Result<()> {
    for entry in root.regular_files()? {
        root.remove_if_same(&entry.name, &entry.file)?;
    }
    if !stores_root.remove_empty_child_if_same(generation, root)? {
        anyhow::bail!(
            "verified-code generation is not a flat regular-file namespace: {}",
            root.path().display()
        );
    }
    Ok(())
}

/// Provenance of the writable project root presented to one launch.
///
/// `RuntimeWorkspace` is set only by daemon-owned execution provenance. It
/// permits that exact workspace beneath the otherwise protected runtime cache;
/// caller-selected live paths always use `External`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxProjectAuthority {
    External,
    RuntimeWorkspace,
    /// Pure node handler launch. The project path supplies a read-only cwd;
    /// no configured host writable mount is granted for this launch.
    ReadOnly,
}

/// Verified file identity for executable code used by one launch.
///
/// The source path records the host-side provenance. Enforced apply re-reads
/// it, requires the already-verified whole-file digest to match, materializes
/// those exact bytes into node-owned content-addressed storage, and executes
/// the artifact from a synthetic read-only code namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxVerifiedCode {
    pub source_path: PathBuf,
    pub content_hash: String,
}

/// Per-launch facts used to resolve policy placeholders and record provenance.
#[derive(Debug, Clone, Copy)]
pub struct SandboxLaunchContext<'a> {
    pub project_path: &'a Path,
    pub project_authority: SandboxProjectAuthority,
    pub state_root: Option<&'a Path>,
    pub checkpoint_dir: Option<&'a Path>,
    /// Daemon-owned callback socket this launch is authorized to reach.
    ///
    /// This is a typed launch fact rather than an inference from child
    /// environment names. Enforced mode validates it against the socket
    /// identity pinned when the daemon loaded the sandbox policy.
    pub daemon_socket_path: Option<&'a Path>,
    pub bundle_roots: &'a [PathBuf],
    pub node_trusted_keys_dir: Option<&'a Path>,
    pub verified_code: &'a [SandboxVerifiedCode],
    pub item_ref: &'a str,
    pub thread_id: &'a str,
}

#[derive(Debug, Clone)]
struct ReadableMount {
    source: PathBuf,
    destination: PathBuf,
    source_handle: Arc<std::fs::File>,
}

impl PartialEq for ReadableMount {
    fn eq(&self, other: &Self) -> bool {
        self.source == other.source && self.destination == other.destination
    }
}

impl Eq for ReadableMount {}

#[derive(Debug, Clone)]
struct PreparedVerifiedCode {
    original: PathBuf,
    canonical_source: PathBuf,
    mirror: Option<ReadableMount>,
    artifact: ReadableMount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WritableMountAuthority {
    Policy,
    DaemonCheckpoint,
}

#[derive(Debug, Clone)]
struct WritableMount {
    source: PathBuf,
    destination: PathBuf,
    authority: WritableMountAuthority,
    source_handle: Arc<std::fs::File>,
}

impl SandboxRuntime {
    /// Load the node-owned policy from its fixed path and resolve its runtime.
    ///
    /// Missing files, malformed YAML, unknown fields, and unsupported versions
    /// are errors in both modes. Enforced mode validates and captures the
    /// configured backend before this value is returned. Disabled mode defers
    /// backend admission unless an engine's runtime registry requires a
    /// mandatory launch-preparer profile.
    pub fn load(app_root: &Path) -> Result<Self, EngineError> {
        Self::load_inner(app_root, None)
    }

    /// Load the daemon snapshot and retain the exact configured callback
    /// socket inode for every launch that is allowed callback IPC.
    pub fn load_for_daemon(app_root: &Path, daemon_socket: &Path) -> Result<Self, EngineError> {
        validate_namespace_destination("daemon socket", daemon_socket)?;
        let socket_parent = daemon_socket.parent().ok_or_else(|| {
            refused(format!(
                "daemon socket path has no parent: {}",
                daemon_socket.display()
            ))
        })?;
        let canonical_parent = canonicalize_launch_path("daemon socket parent", socket_parent)?;
        let socket_name = daemon_socket.file_name().ok_or_else(|| {
            refused(format!(
                "daemon socket path has no file name: {}",
                daemon_socket.display()
            ))
        })?;
        let parent = lillux::PinnedDirectory::open(&canonical_parent)
            .map_err(|error| refused(format!("daemon socket parent cannot be pinned: {error}")))?
            .ok_or_else(|| refused("daemon socket parent disappeared".to_string()))?;
        let entry = parent
            .open_mount_entry(socket_name)
            .map_err(|error| refused(format!("daemon socket cannot be pinned: {error}")))?
            .ok_or_else(|| refused("daemon socket disappeared before sandbox load".to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt as _;
            if !entry
                .metadata()
                .map_err(|error| refused(format!("daemon socket cannot be inspected: {error}")))?
                .file_type()
                .is_socket()
            {
                return Err(refused(format!(
                    "daemon socket {} is not a Unix socket",
                    daemon_socket.display()
                )));
            }
        }
        let socket = PinnedDaemonSocket {
            source: canonical_parent.join(socket_name),
            destination: daemon_socket.to_path_buf(),
            parent: Arc::new(parent),
            name: socket_name.to_os_string(),
            entry: Arc::new(entry),
        };
        Self::load_inner(app_root, Some(socket))
    }

    fn load_inner(
        app_root: &Path,
        daemon_socket: Option<PinnedDaemonSocket>,
    ) -> Result<Self, EngineError> {
        validate_namespace_destination("app root", app_root)?;
        // Bind the policy bytes and all later authority checks to one canonical
        // app-root identity. The supplied spelling remains the namespace
        // destination, but it is never used as the host-side authority root.
        let runtime_app_root = canonicalize_context_mount("app root", app_root)?;
        let app_root_authority = lillux::PinnedDirectory::open(&runtime_app_root)
            .map_err(|error| refused(format!("app root cannot be pinned: {error}")))?
            .ok_or_else(|| {
                refused("app root disappeared while loading sandbox policy".to_string())
            })?;
        let policy_parent = open_relative_directory(
            &app_root_authority,
            &[crate::AI_DIR, "node"],
            "sandbox policy parent",
        )?;
        let source = runtime_app_root
            .join(crate::AI_DIR)
            .join(SANDBOX_POLICY_RELATIVE_PATH);
        let mut file = policy_parent
            .open_regular("sandbox.yaml".as_ref(), false)
            .map_err(|error| refused(format!("node sandbox policy cannot be opened: {error}")))?
            .ok_or_else(|| {
                refused(format!(
                    "node sandbox policy is required at {}",
                    source.display()
                ))
            })?;
        let mut raw = String::new();
        file.read_to_string(&mut raw)
            .map_err(|error| EngineError::SandboxPolicyRefused {
                reason: format!(
                    "node sandbox policy is required at {}: {error}",
                    source.display()
                ),
            })?;
        let policy: SandboxPolicy =
            serde_yaml::from_str(&raw).map_err(|error| EngineError::SandboxPolicyRefused {
                reason: format!("invalid node sandbox policy {}: {error}", source.display()),
            })?;
        let digest = format!("sha256:{}", lillux::sha256_hex(raw.as_bytes()));
        let observed_app_root = lillux::PinnedDirectory::open(app_root)
            .map_err(|error| refused(format!("app root cannot be rechecked: {error}")))?
            .ok_or_else(|| {
                refused("app root disappeared while loading sandbox policy".to_string())
            })?;
        if !app_root_authority
            .is_same_directory(&observed_app_root)
            .map_err(|error| refused(format!("app-root identity cannot be compared: {error}")))?
        {
            return Err(refused(format!(
                "app root {} changed while its sandbox policy was being loaded",
                app_root.display()
            )));
        }
        Self::resolve(
            policy,
            Some(source),
            Some(digest),
            Some(runtime_app_root),
            Some(Arc::new(app_root_authority)),
            Some(app_root.to_path_buf()),
            daemon_socket,
        )
    }

    pub fn source(&self) -> Option<&Path> {
        self.inspection.source.as_deref()
    }

    pub fn version(&self) -> u32 {
        self.inspection.version
    }

    pub fn mode(&self) -> SandboxMode {
        self.inspection.mode
    }

    pub fn digest(&self) -> Option<&str> {
        self.inspection.digest.as_deref()
    }

    pub fn inspection(&self) -> &SandboxInspection {
        &self.inspection
    }

    /// Load and resolve a policy for an inspection-only caller such as doctor.
    /// This shares the production parser and validator rather than maintaining
    /// a second diagnostic interpretation of the policy.
    pub fn inspect(app_root: &Path) -> Result<SandboxInspection, EngineError> {
        Self::load(app_root).map(|runtime| runtime.inspection)
    }

    pub fn is_enforced(&self) -> bool {
        self.state == SandboxRuntimeState::Enforced
    }

    /// Capture the configured Bubblewrap backend into a detached tentative
    /// snapshot. The daemon-wide capture is not changed until the caller has
    /// validated every runtime/preparer edge against this exact handle.
    pub fn tentative_mandatory_bubblewrap_backend(&self) -> Result<Self, EngineError> {
        if let Some(backend) = self.backend_capture.get() {
            return Ok(self.with_backend_inspection(backend));
        }
        let artifacts = self.verified_artifacts.as_deref().ok_or_else(|| {
            refused(
                "mandatory Bubblewrap profile requires a production sandbox snapshot".to_string(),
            )
        })?;
        let backend = SandboxBackendPolicy {
            kind: self.inspection.backend.kind,
            executable: self.inspection.backend.configured_executable.clone(),
        };
        let (resolved_executable, handle, digest, version) =
            resolve_backend(&backend, artifacts)?;
        let candidate = CapturedSandboxBackend {
            resolved_executable,
            handle,
            digest,
            version,
        };
        let backend_capture = Arc::new(OnceLock::new());
        backend_capture
            .set(candidate)
            .expect("a fresh tentative backend cell accepts its capture");
        let mut captured = self.clone();
        captured.backend_capture = backend_capture;
        let backend = captured
            .backend_capture
            .get()
            .expect("tentative backend capture is initialized");
        captured.inspection.backend.resolved_executable =
            Some(backend.resolved_executable.clone());
        captured.inspection.backend.captured_digest = Some(backend.digest.clone());
        captured.inspection.backend.captured_version = Some(backend.version.clone());
        Ok(captured)
    }

    /// Publish a fully validated tentative backend into the daemon snapshot.
    /// Returns the exact published snapshot and whether another concurrent
    /// admission won the race with a different handle. A reconciled caller must
    /// validate its runtime/preparer graph again against the returned winner.
    pub fn publish_mandatory_bubblewrap_backend(
        &self,
        tentative: &Self,
    ) -> Result<(Self, bool), EngineError> {
        let same_artifact_store = match (
            self.verified_artifacts.as_ref(),
            tentative.verified_artifacts.as_ref(),
        ) {
            (Some(current), Some(candidate)) => Arc::ptr_eq(current, candidate),
            (None, None) => true,
            _ => false,
        };
        if self.inspection.source != tentative.inspection.source
            || self.inspection.digest != tentative.inspection.digest
            || self.inspection.backend.kind != tentative.inspection.backend.kind
            || self.inspection.backend.configured_executable
                != tentative.inspection.backend.configured_executable
            || !same_artifact_store
        {
            return Err(refused(
                "tentative mandatory backend belongs to a different sandbox policy snapshot"
                    .to_string(),
            ));
        }
        let candidate = tentative.backend_capture.get().ok_or_else(|| {
            refused("tentative mandatory backend has no captured executable".to_string())
        })?;
        let _ = self.backend_capture.set(candidate.clone());
        let published = self
            .backend_capture
            .get()
            .expect("mandatory backend publication selects a winner");
        let reconciled = !Arc::ptr_eq(&candidate.handle, &published.handle);
        Ok((self.with_backend_inspection(published), reconciled))
    }

    fn with_backend_inspection(&self, backend: &CapturedSandboxBackend) -> Self {
        let mut captured = self.clone();
        captured.inspection.backend.resolved_executable =
            Some(backend.resolved_executable.clone());
        captured.inspection.backend.captured_digest = Some(backend.digest.clone());
        captured.inspection.backend.captured_version = Some(backend.version.clone());
        captured
    }

    /// Return a content-captured Bubblewrap executable for a mandatory private
    /// profile such as launch preparation. This consumes the immutable policy
    /// snapshot instead of reopening `sandbox.yaml`. General sandbox mode does
    /// not disable mandatory profiles; runtime admission validates a tentative
    /// capture before atomically publishing it with the bound preparer registry.
    /// This accessor never reopens node policy or executable paths.
    pub fn capture_mandatory_bubblewrap_backend(
        &self,
    ) -> Result<Arc<std::fs::File>, EngineError> {
        self.backend_capture
            .get()
            .map(|backend| backend.handle.clone())
            .ok_or_else(|| {
                refused(
                    "mandatory Bubblewrap profile requires a production sandbox snapshot with a daemon-captured backend"
                        .to_string(),
                )
            })
    }

    /// Apply this immutable policy snapshot to one executable request.
    pub fn apply(
        &self,
        request: lillux::SubprocessRequest,
        context: SandboxLaunchContext<'_>,
    ) -> Result<lillux::SubprocessRequest, EngineError> {
        if !request.timeout.is_finite() || request.timeout < 0.0 {
            return Err(refused(format!(
                "invalid subprocess timeout {}",
                request.timeout
            )));
        }
        if self.state == SandboxRuntimeState::Disabled {
            // Opt-out disables Bubblewrap and OS confinement, not daemon-memory
            // safety. Retained output remains bounded by the immutable node
            // policy, with any lower caller limit preserved.
            let mut request = request;
            let requested = request.limits.unwrap_or_default();
            request.limits = Some(lillux::SubprocessLimits {
                // `mode: disabled` is an OS-confinement opt-out. Preserve a
                // tighter limit already owned by the caller, but do not install
                // the sandbox policy's RLIMIT_NOFILE until enforcement is on.
                max_open_files: requested.max_open_files,
                max_stdout_bytes: Some(
                    requested
                        .max_stdout_bytes
                        .map_or(self.inspection.limits.stdout_bytes, |limit| {
                            limit.min(self.inspection.limits.stdout_bytes)
                        }),
                ),
                max_stderr_bytes: Some(
                    requested
                        .max_stderr_bytes
                        .map_or(self.inspection.limits.stderr_bytes, |limit| {
                            limit.min(self.inspection.limits.stderr_bytes)
                        }),
                ),
            });
            return Ok(request);
        }
        if !request.inherited_fds.is_empty() {
            return Err(refused(
                "enforced sandbox launches cannot inherit caller-supplied file descriptors"
                    .to_string(),
            ));
        }
        if request.supervised_status.is_some() {
            return Err(refused(
                "enforced sandbox launches cannot inherit caller-supplied process supervision"
                    .to_string(),
            ));
        }

        let _item_ref = CanonicalRef::parse(context.item_ref).map_err(|error| {
            refused(format!(
                "invalid sandbox item reference `{}`: {error}",
                context.item_ref
            ))
        })?;
        // Retained in the launch context even though Bubblewrap does not need it
        // in argv. Audit surfaces can attach it without expanding policy input.
        let _thread_id = context.thread_id;
        validate_thread_path_component(context.thread_id)?;

        let lillux::SubprocessRequest {
            cmd,
            mut args,
            cwd,
            mut envs,
            stdin_data,
            timeout,
            limits,
            mut inherited_fds,
            supervised_status,
        } = request;
        debug_assert!(supervised_status.is_none());

        let project_destination = context.project_path.to_path_buf();
        let cwd_destination = cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| project_destination.clone());
        validate_namespace_destination("project", &project_destination)?;
        validate_namespace_destination("working directory", &cwd_destination)?;
        if let Some(path) = context.state_root {
            validate_namespace_destination("state root", path)?;
        }
        if let Some(path) = context.checkpoint_dir {
            validate_namespace_destination("checkpoint directory", path)?;
        }
        for root in context.bundle_roots {
            validate_namespace_destination("bundle root", root)?;
        }
        if let Some(path) = context.node_trusted_keys_dir {
            validate_namespace_destination("node trusted-keys directory", path)?;
        }
        if let Some(path) = self.app_root_destination.as_deref() {
            validate_namespace_destination("app root", path)?;
        }
        let canonical_project = canonicalize_context_mount("project", &project_destination)?;
        let canonical_cwd = canonicalize_context_mount("working directory", &cwd_destination)?;
        let (runtime_workspace_authorized, project_source_handle) = if context.project_authority
            == SandboxProjectAuthority::RuntimeWorkspace
        {
            let workspaces = self
                .runtime_workspaces
                .as_deref()
                .expect("enforced sandbox runtime has pinned workspace authority");
            if canonical_project.parent() != Some(workspaces.path()) {
                return Err(refused(format!(
                    "runtime workspace {} is not a direct child of {}",
                    canonical_project.display(),
                    workspaces.path().display()
                )));
            }
            let workspace_name = canonical_project.file_name().ok_or_else(|| {
                refused(format!(
                    "runtime workspace {} has no child name",
                    canonical_project.display()
                ))
            })?;
            let expected = workspaces
                .open_child_directory(workspace_name)
                .map_err(|error| refused(format!("runtime workspace cannot be opened: {error}")))?
                .ok_or_else(|| refused("runtime workspace disappeared".to_string()))?;
            let requested = lillux::PinnedDirectory::open(&canonical_project)
                .map_err(|error| {
                    refused(format!(
                        "requested runtime workspace cannot be pinned: {error}"
                    ))
                })?
                .ok_or_else(|| refused("requested runtime workspace disappeared".to_string()))?;
            if !expected.is_same_directory(&requested).map_err(|error| {
                refused(format!(
                    "runtime workspace identity cannot be compared: {error}"
                ))
            })? {
                return Err(refused(
                    "runtime workspace does not match its pinned named-child authority".to_string(),
                ));
            }
            let handle = Arc::new(expected.try_clone_descriptor().map_err(|error| {
                refused(format!(
                    "runtime workspace authority cannot be cloned: {error}"
                ))
            })?);
            (true, Some(handle))
        } else {
            (false, None)
        };

        let mut environment_names = std::collections::HashSet::new();
        if let Some((duplicate, _)) = envs
            .iter()
            .find(|(name, _)| !environment_names.insert(name.as_str()))
        {
            return Err(refused(format!(
                "environment variable `{duplicate}` is present more than once"
            )));
        }
        if let Some((name, _)) = envs.iter().find(|(name, value)| {
            name.is_empty() || name.contains('=') || name.contains('\0') || value.contains('\0')
        }) {
            return Err(refused(format!(
                "environment variable `{name}` has an invalid name or value"
            )));
        }
        if let Some((name, _)) = envs.iter().find(|(name, _)| {
            name != "TMPDIR" && !environment_name_allowed(&self.inspection.environment, name)
        }) {
            return Err(refused(format!(
                "environment variable `{name}` is not allowed by node policy"
            )));
        }

        let backend = self
            .backend_capture
            .get()
            .expect("enforced sandbox runtime has a captured backend");
        let backend_path = &backend.resolved_executable;
        let backend_handle = &backend.handle;
        let canonical_state_root = context
            .state_root
            .map(|path| canonicalize_context_mount("state root", path))
            .transpose()?;
        let canonical_checkpoint_dir = context
            .checkpoint_dir
            .map(|path| canonicalize_context_mount("checkpoint directory", path))
            .transpose()?;
        let mut checkpoint_source_handle = None;
        if let Some(checkpoint_dir) = &canonical_checkpoint_dir {
            let expected = self
                .app_root
                .as_deref()
                .expect("enforced sandbox runtime has an app root")
                .join("threads")
                .join(context.thread_id)
                .join("checkpoints");
            let app_root_authority = self
                .app_root_authority
                .as_deref()
                .expect("enforced sandbox runtime has pinned app-root authority");
            let expected_authority = open_relative_directory(
                app_root_authority,
                &["threads", context.thread_id, "checkpoints"],
                "daemon checkpoint directory",
            )?;
            let requested_authority = lillux::PinnedDirectory::open(checkpoint_dir)
                .map_err(|error| {
                    refused(format!("checkpoint directory cannot be pinned: {error}"))
                })?
                .ok_or_else(|| refused("checkpoint directory disappeared".to_string()))?;
            if !expected_authority
                .is_same_directory(&requested_authority)
                .map_err(|error| {
                    refused(format!("checkpoint identity cannot be compared: {error}"))
                })?
            {
                return Err(refused(
                    "checkpoint directory is not the daemon-owned directory".to_string(),
                ));
            }
            if checkpoint_dir != &expected {
                return Err(refused(format!(
                    "checkpoint directory {} does not match daemon-owned path {}",
                    checkpoint_dir.display(),
                    expected.display()
                )));
            }
            checkpoint_source_handle = Some(Arc::new(
                expected_authority.try_clone_descriptor().map_err(|error| {
                    refused(format!("checkpoint authority cannot be cloned: {error}"))
                })?,
            ));
        }
        let resolved_writable_mounts =
            if context.project_authority == SandboxProjectAuthority::ReadOnly {
                Vec::new()
            } else {
                self.inspection
                    .filesystem
                    .writable
                    .iter()
                    .map(|configured| {
                        resolve_writable_mount(
                            configured,
                            &project_destination,
                            &canonical_project,
                            &cwd_destination,
                            &canonical_cwd,
                            context.checkpoint_dir,
                            canonical_checkpoint_dir.as_deref(),
                            checkpoint_source_handle.as_ref(),
                            project_source_handle.as_ref(),
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
        let mut writable_mounts = Vec::<WritableMount>::new();
        for mount in resolved_writable_mounts.into_iter().flatten() {
            if let Some(existing) = writable_mounts.iter_mut().find(|existing| {
                existing.source == mount.source && existing.destination == mount.destination
            }) {
                if mount.authority == WritableMountAuthority::DaemonCheckpoint {
                    existing.authority = mount.authority;
                }
            } else {
                writable_mounts.push(mount);
            }
        }
        writable_mounts.sort_by(|left, right| {
            left.destination
                .cmp(&right.destination)
                .then_with(|| left.source.cmp(&right.source))
        });
        for mount in &writable_mounts {
            validate_writable_mount(
                &mount.source,
                self.app_root.as_deref(),
                backend_path,
                self.daemon_socket
                    .as_ref()
                    .map(|socket| socket.source.as_path()),
                &canonical_project,
                context.project_authority,
                runtime_workspace_authorized,
                mount.authority,
                canonical_checkpoint_dir.as_deref(),
            )?;
        }
        let mut prepared_code = Vec::with_capacity(context.verified_code.len() + 1);
        for verified in context.verified_code {
            let prepared = self.prepare_verified_code(
                verified,
                &project_destination,
                &canonical_project,
                context.bundle_roots,
            )?;
            rewrite_verified_code_references(
                &mut args,
                &mut envs,
                &verified.source_path,
                &prepared.canonical_source,
                &prepared.artifact.destination,
            )?;
            prepared_code.push(prepared);
        }

        let lexical_command = PathBuf::from(&cmd);
        if !lexical_command.is_absolute() {
            return Err(refused(format!(
                "command path must be absolute: {}",
                lexical_command.display()
            )));
        }
        let canonical_command = canonicalize_context_mount("command", &lexical_command)?;
        // Preserve argv[0] only when the requested command spelling traversed
        // a symlink. This retains virtual-environment launcher semantics while
        // the executable itself comes from the already-resolved target.
        let command_argv0 = (lexical_command != canonical_command).then(|| cmd.clone());
        let verified_command = prepared_code.iter().find(|prepared| {
            lexical_command == prepared.original || canonical_command == prepared.canonical_source
        });
        let (command_path, command_is_on_system_surface) = if let Some(prepared) = verified_command
        {
            (prepared.artifact.destination.clone(), false)
        } else {
            let on_system_surface = is_lexically_on_system_runtime_surface(&lexical_command)
                && is_on_system_runtime_surface(&canonical_command);
            if on_system_surface {
                (canonical_command, true)
            } else {
                let is_runtime_workspace_project_code = context.project_authority
                    == SandboxProjectAuthority::RuntimeWorkspace
                    && lexical_command.starts_with(&project_destination);
                let is_node_code = self.app_root.as_deref().is_some_and(|root| {
                    canonical_command.starts_with(root)
                        && !(is_runtime_workspace_project_code
                            && canonical_command.starts_with(&canonical_project))
                }) || self.app_root_destination.as_deref().is_some_and(|root| {
                    lexical_command.starts_with(root) && !is_runtime_workspace_project_code
                });
                let is_bundle_code = context.bundle_roots.iter().any(|root| {
                    lexical_command.starts_with(root)
                        || std::fs::canonicalize(root)
                            .ok()
                            .is_some_and(|root| canonical_command.starts_with(root))
                });
                if is_node_code || is_bundle_code {
                    return Err(refused(format!(
                        "node or bundle executable {} lacks a verified content identity",
                        canonical_command.display()
                    )));
                }
                // Operator/project executables such as a virtual-environment
                // interpreter are not signed bundle material. Capture the
                // exact opened bytes now and execute only the immutable copy.
                let prepared = self.prepare_current_command(
                    &canonical_command,
                    &project_destination,
                    &canonical_project,
                    context.bundle_roots,
                )?;
                let command_path = prepared.artifact.destination.clone();
                prepared_code.push(prepared);
                (command_path, false)
            }
        };
        let mut verified_code_mounts = prepared_code
            .iter()
            .filter_map(|prepared| prepared.mirror.clone())
            .chain(
                prepared_code
                    .iter()
                    .map(|prepared| prepared.artifact.clone()),
            )
            .collect::<Vec<_>>();
        verified_code_mounts.sort_by(|left, right| {
            left.destination
                .cmp(&right.destination)
                .then_with(|| left.source.cmp(&right.source))
        });
        verified_code_mounts.dedup();
        let requested_daemon_socket = context.daemon_socket_path;
        if let Some(path) = requested_daemon_socket {
            validate_namespace_destination("requested daemon socket", path)?;
        }
        let canonical_requested_daemon_socket = requested_daemon_socket
            .map(|requested| {
                let configured = self.daemon_socket.as_ref().ok_or_else(|| {
                    refused(
                        "sandbox launch requested daemon IPC without a daemon-pinned socket path"
                            .to_string(),
                    )
                })?;
                if requested != configured.destination {
                    return Err(refused(format!(
                        "sandbox launch requested daemon socket {}, expected {}",
                        requested.display(),
                        configured.destination.display()
                    )));
                }
                let current = configured
                    .parent
                    .open_mount_entry(&configured.name)
                    .map_err(|error| {
                        refused(format!(
                            "daemon socket authority cannot be checked: {error}"
                        ))
                    })?
                    .ok_or_else(|| refused("daemon socket entry disappeared".to_string()))?;
                if !same_file_identity(&current, &configured.entry)? {
                    return Err(refused(format!(
                        "daemon socket {} changed after sandbox load",
                        configured.destination.display()
                    )));
                }
                Ok(configured.source.clone())
            })
            .transpose()?;
        let mut readable_mounts = self
            .inspection
            .filesystem
            .readable
            .iter()
            .map(|configured| {
                resolve_readable_mounts(
                    configured,
                    &project_destination,
                    &canonical_project,
                    &cwd_destination,
                    &canonical_cwd,
                    self.app_root.as_deref(),
                    self.app_root_authority.as_deref(),
                    self.app_root_destination.as_deref(),
                    self.daemon_socket.as_ref(),
                    requested_daemon_socket,
                    context.bundle_roots,
                    context.node_trusted_keys_dir,
                    &verified_code_mounts,
                )
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        readable_mounts.sort_by(|left, right| {
            left.destination
                .cmp(&right.destination)
                .then_with(|| left.source.cmp(&right.source))
        });
        readable_mounts.dedup();
        for mount in &writable_mounts {
            validate_namespace_destination("writable mount", &mount.destination)?;
        }
        for mount in &readable_mounts {
            validate_namespace_destination("readable mount", &mount.destination)?;
        }
        for required in &verified_code_mounts {
            if !readable_mounts.iter().any(|mount| mount == required) {
                return Err(refused(format!(
                    "verified code {} is not pinned read-only by the node sandbox policy",
                    required.destination.display()
                )));
            }
        }
        for (kind, required_destination, required_source) in [
            (
                "state root",
                context.state_root,
                canonical_state_root.as_deref(),
            ),
            (
                "checkpoint directory",
                context.checkpoint_dir,
                canonical_checkpoint_dir.as_deref(),
            ),
        ] {
            if let (Some(required_destination), Some(required_source)) =
                (required_destination, required_source)
            {
                if !writable_mounts.iter().any(|mount| {
                    required_source.starts_with(&mount.source)
                        && required_destination.starts_with(&mount.destination)
                }) {
                    return Err(refused(format!(
                        "{kind} {} is not writable under the node sandbox policy",
                        required_destination.display()
                    )));
                }
            }
        }
        if let (Some(requested), Some(canonical_requested)) = (
            requested_daemon_socket,
            canonical_requested_daemon_socket.as_deref(),
        ) {
            let socket_is_visible = readable_mounts
                .iter()
                .any(|mount| mount.source == canonical_requested && mount.destination == requested);
            if !socket_is_visible {
                return Err(refused(format!(
                    "daemon socket {} is not readable under the node sandbox policy",
                    requested.display()
                )));
            }
        }
        let cwd_is_visible = writable_mounts.iter().any(|mount| {
            canonical_cwd.starts_with(&mount.source)
                && cwd_destination.starts_with(&mount.destination)
        }) || readable_mounts.iter().any(|mount| {
            canonical_cwd.starts_with(&mount.source)
                && cwd_destination.starts_with(&mount.destination)
        });
        if !cwd_is_visible {
            return Err(refused(format!(
                "working directory {} is not visible through a readable or writable node-policy mount",
                cwd_destination.display()
            )));
        }
        validate_namespace_destination("command", &command_path)?;
        let status = lillux::bubblewrap_status_pipe().map_err(|reason| {
            refused(format!(
                "Bubblewrap process supervision cannot be initialized: {reason}"
            ))
        })?;
        let status_fd = status.writer_fd().to_string();
        inherited_fds.push(status.writer);
        let mut sandbox_args = vec![
            "--json-status-fd".to_string(),
            status_fd,
            "--clearenv".to_string(),
            "--unshare-user".to_string(),
            "--unshare-ipc".to_string(),
            "--unshare-uts".to_string(),
        ];
        // Deliberately retain the host PID namespace: managed runtimes attach
        // their own PID over the pinned UDS, and the daemon must be able to use
        // that value for host-side PGID cancellation and reconciliation.
        if self.inspection.network.mode == SandboxNetworkMode::Isolated {
            sandbox_args.push("--unshare-net".to_string());
        }
        sandbox_args.extend(["--tmpfs".to_string(), "/".to_string()]);

        // Only the OS runtime surface and exact verified executable are visible.
        // System sources are fd-pinned as well: even privileged host updates
        // cannot redirect a validated mount pathname between apply and spawn.
        let mut system_readable_mounts = Vec::new();
        for path in ["/usr", "/bin", "/lib", "/lib64"] {
            let destination = PathBuf::from(path);
            if destination.exists() {
                let source = canonicalize_launch_path("system runtime mount", &destination)?;
                let source_handle = pin_mount_source("system runtime mount", &source)?;
                sandbox_args.extend([
                    "--ro-bind-fd".to_string(),
                    mount_fd_arg(&source_handle),
                    destination.to_string_lossy().into_owned(),
                ]);
                system_readable_mounts.push(ReadableMount {
                    source,
                    destination,
                    source_handle,
                });
            }
        }
        sandbox_args.extend(["--dir".to_string(), "/etc".to_string()]);
        // Retaining host PIDs is required by the current cancellation
        // protocol, but exposing the host procfs would let a same-UID workload
        // escape the selected filesystem surface through another process's
        // `root`, `cwd`, or `fd` links. Present an empty `/proc` directory;
        // RyeOS does not claim PID/signal isolation.
        sandbox_args.extend(["--dir".to_string(), "/proc".to_string()]);
        for path in [
            "/etc/hosts",
            "/etc/nsswitch.conf",
            "/etc/resolv.conf",
            "/etc/ssl",
        ] {
            let destination = PathBuf::from(path);
            if destination.exists() {
                let source = canonicalize_launch_path("system configuration mount", &destination)?;
                let source_handle = pin_mount_source("system configuration mount", &source)?;
                sandbox_args.extend([
                    "--ro-bind-fd".to_string(),
                    mount_fd_arg(&source_handle),
                    destination.to_string_lossy().into_owned(),
                ]);
                system_readable_mounts.push(ReadableMount {
                    source,
                    destination,
                    source_handle,
                });
            }
        }

        if !command_is_on_system_surface {
            append_parent_directories(&mut sandbox_args, &command_path);
        }

        sandbox_args.extend(["--dev".to_string(), "/dev".to_string()]);
        sandbox_args.extend(["--tmpfs".to_string(), "/tmp".to_string()]);
        for mount in &readable_mounts {
            append_parent_directories(&mut sandbox_args, &mount.destination);
        }
        for mount in &writable_mounts {
            append_parent_directories(&mut sandbox_args, &mount.destination);
        }
        inherited_fds.extend(
            writable_mounts
                .iter()
                .map(|mount| mount.source_handle.clone()),
        );
        inherited_fds.extend(
            readable_mounts
                .iter()
                .map(|mount| mount.source_handle.clone()),
        );
        inherited_fds.extend(
            system_readable_mounts
                .iter()
                .map(|mount| mount.source_handle.clone()),
        );
        inherited_fds.push(backend_handle.clone());
        for mount in &writable_mounts {
            sandbox_args.extend([
                "--bind-fd".to_string(),
                mount_fd_arg(&mount.source_handle),
                mount.destination.to_string_lossy().into_owned(),
            ]);
        }
        // Read-only policy always wins over an overlapping writable ancestor.
        // Verified code is held back for a dedicated final overlay below.
        for mount in readable_mounts
            .iter()
            .filter(|mount| !verified_code_mounts.contains(mount))
        {
            sandbox_args.extend([
                "--ro-bind-fd".to_string(),
                mount_fd_arg(&mount.source_handle),
                mount.destination.to_string_lossy().into_owned(),
            ]);
        }
        // Install verified root files, verified executables, and captured
        // project/operator executables last. No ordinary policy mount can
        // override these synthetic destinations.
        for mount in &verified_code_mounts {
            sandbox_args.extend([
                "--ro-bind-fd".to_string(),
                mount_fd_arg(&mount.source_handle),
                mount.destination.to_string_lossy().into_owned(),
            ]);
        }

        for (name, value) in &envs {
            if name == "TMPDIR" {
                continue;
            }
            sandbox_args.extend(["--setenv".to_string(), name.clone(), value.clone()]);
        }
        // `/tmp` is a private tmpfs in every enforced launch. Pin the standard
        // Unix temporary-directory contract to that namespace-local path so an
        // inherited host TMPDIR cannot make a runtime cache silently disappear.
        sandbox_args.extend([
            "--setenv".to_string(),
            "TMPDIR".to_string(),
            "/tmp".to_string(),
        ]);

        sandbox_args.push("--chdir".to_string());
        sandbox_args.push(cwd_destination.to_string_lossy().into_owned());
        if let Some(argv0) = command_argv0 {
            sandbox_args.extend(["--argv0".to_string(), argv0]);
        }
        sandbox_args.push("--".to_string());
        sandbox_args.push(command_path.to_string_lossy().into_owned());
        sandbox_args.append(&mut args);
        let bubblewrap_args_handle = seal_bubblewrap_args(&sandbox_args)?;
        let bubblewrap_args_fd = mount_fd_arg(&bubblewrap_args_handle);
        inherited_fds.push(bubblewrap_args_handle);

        let requested_open_files = limits.as_ref().and_then(|limits| limits.max_open_files);
        let effective_open_files = match (self.inspection.limits.open_files, requested_open_files) {
            (Some(policy), Some(requested)) => Some(policy.min(requested)),
            (Some(policy), None) => Some(policy),
            (None, Some(requested)) => Some(requested),
            (None, None) => None,
        };
        let effective_stdout_bytes = limits
            .as_ref()
            .and_then(|limits| limits.max_stdout_bytes)
            .map_or(self.inspection.limits.stdout_bytes, |requested| {
                requested.min(self.inspection.limits.stdout_bytes)
            });
        let effective_stderr_bytes = limits
            .as_ref()
            .and_then(|limits| limits.max_stderr_bytes)
            .map_or(self.inspection.limits.stderr_bytes, |requested| {
                requested.min(self.inspection.limits.stderr_bytes)
            });
        let limits = Some(lillux::SubprocessLimits {
            max_open_files: effective_open_files,
            max_stdout_bytes: Some(effective_stdout_bytes),
            max_stderr_bytes: Some(effective_stderr_bytes),
        });

        Ok(lillux::SubprocessRequest {
            cmd: format!("/proc/self/fd/{}", mount_fd_arg(backend_handle)),
            args: vec!["--args".to_string(), bubblewrap_args_fd],
            cwd: Some(canonical_cwd.to_string_lossy().into_owned()),
            // Never expose the target environment to Bubblewrap's own dynamic
            // loader. `--clearenv`/`--setenv` constructs it inside the sandbox.
            envs: Vec::new(),
            stdin_data,
            timeout,
            limits,
            inherited_fds,
            supervised_status: Some(status.reader),
        })
    }

    fn resolve(
        policy: SandboxPolicy,
        source: Option<PathBuf>,
        digest: Option<String>,
        app_root: Option<PathBuf>,
        app_root_authority: Option<Arc<lillux::PinnedDirectory>>,
        app_root_destination: Option<PathBuf>,
        daemon_socket: Option<PinnedDaemonSocket>,
    ) -> Result<Self, EngineError> {
        if policy.version != SANDBOX_POLICY_VERSION {
            return Err(refused(format!(
                "unsupported node sandbox policy version {} (expected {})",
                policy.version, SANDBOX_POLICY_VERSION
            )));
        }
        validate_policy_semantics(&policy)?;

        let state = match policy.mode {
            SandboxMode::Disabled => SandboxRuntimeState::Disabled,
            SandboxMode::Enforce => SandboxRuntimeState::Enforced,
        };
        if state == SandboxRuntimeState::Enforced {
            validate_enforced_limits(&policy.limits)?;
            if app_root.is_none() {
                return Err(refused(
                    "enforced sandbox runtime requires an app root".to_string(),
                ));
            }
        }
        // Production snapshots capture Bubblewrap immediately when enforcement
        // is enabled. Disabled snapshots defer that work until runtime admission
        // knows whether a mandatory private profile exists. The `None` branch is
        // retained solely for the in-memory `Default` fixture, which has no app
        // root.
        let verified_artifacts = match app_root_authority.as_deref() {
            Some(app_root) => Some(Arc::new(VerifiedArtifactStore::create(
                app_root,
                &policy.limits,
            )?)),
            None => None,
        };
        let captured_backend = if state == SandboxRuntimeState::Enforced {
            let artifacts = verified_artifacts.as_deref().ok_or_else(|| {
                refused("enforced sandbox runtime requires an artifact store".to_string())
            })?;
            let (resolved_executable, handle, digest, version) =
                resolve_backend(&policy.backend, artifacts)?;
            Some(CapturedSandboxBackend {
                resolved_executable,
                handle,
                digest,
                version,
            })
        } else {
            None
        };
        let runtime_workspaces = if state == SandboxRuntimeState::Enforced {
            let app_root = app_root_authority.as_deref().ok_or_else(|| {
                refused("enforced sandbox runtime requires pinned app-root authority".to_string())
            })?;
            let workspaces = open_or_create_relative_directory(
                app_root,
                &[crate::AI_DIR, "state", "cache", "executions"],
                0o700,
                "runtime workspace root",
            )?;
            workspaces.set_mode(0o700).map_err(|error| {
                refused(format!(
                    "runtime workspace root cannot be protected: {error}"
                ))
            })?;
            Some(Arc::new(workspaces))
        } else {
            None
        };
        let resolved_executable = captured_backend
            .as_ref()
            .map(|backend| backend.resolved_executable.clone());
        let captured_digest = captured_backend
            .as_ref()
            .map(|backend| backend.digest.clone());
        let captured_version = captured_backend
            .as_ref()
            .map(|backend| backend.version.clone());
        let backend_capture = Arc::new(OnceLock::new());
        if let Some(captured_backend) = captured_backend {
            backend_capture
                .set(captured_backend)
                .expect("a fresh backend capture cell accepts its initial value");
        }
        Ok(Self {
            inspection: SandboxInspection {
                source,
                version: policy.version,
                mode: policy.mode,
                digest,
                backend: SandboxBackendInspection {
                    kind: policy.backend.kind,
                    configured_executable: policy.backend.executable,
                    resolved_executable,
                    captured_digest,
                    captured_version,
                },
                filesystem: policy.filesystem,
                network: policy.network,
                environment: policy.environment,
                limits: policy.limits,
            },
            state,
            app_root,
            app_root_authority,
            runtime_workspaces,
            app_root_destination,
            daemon_socket,
            verified_artifacts,
            backend_capture,
        })
    }

    fn prepare_verified_code(
        &self,
        verified: &SandboxVerifiedCode,
        project_destination: &Path,
        canonical_project: &Path,
        bundle_roots: &[PathBuf],
    ) -> Result<PreparedVerifiedCode, EngineError> {
        if !verified.source_path.is_absolute() {
            return Err(refused(format!(
                "verified code path must be absolute: {}",
                verified.source_path.display()
            )));
        }
        if verified.content_hash.len() != 64
            || !verified
                .content_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(refused(format!(
                "verified code has invalid SHA-256 digest `{}`",
                verified.content_hash
            )));
        }
        let artifacts = self
            .verified_artifacts
            .as_deref()
            .expect("enforced sandbox runtime has a verified artifact store");
        let canonical_source = canonicalize_context_mount("code source", &verified.source_path)?;
        let (content, _) = artifacts.read_source("verified code", &verified.source_path)?;
        let actual_hash = lillux::cas::sha256_hex(&content);
        if actual_hash != verified.content_hash {
            return Err(refused(format!(
                "verified code {} changed after verification (expected {}, got {})",
                verified.source_path.display(),
                verified.content_hash,
                actual_hash
            )));
        }
        let observed_source = canonicalize_context_mount("code source", &verified.source_path)?;
        if observed_source != canonical_source {
            return Err(refused(format!(
                "verified code {} changed filesystem identity while its bytes were captured",
                verified.source_path.display()
            )));
        }
        self.prepare_code_bytes(
            &verified.source_path,
            Some(&canonical_source),
            &verified.content_hash,
            &content,
            project_destination,
            canonical_project,
            bundle_roots,
        )
    }

    fn prepare_current_command(
        &self,
        command: &Path,
        project_destination: &Path,
        canonical_project: &Path,
        bundle_roots: &[PathBuf],
    ) -> Result<PreparedVerifiedCode, EngineError> {
        let artifacts = self
            .verified_artifacts
            .as_deref()
            .expect("enforced sandbox runtime has a verified artifact store");
        let (content, metadata) = artifacts.read_source("command", command)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(refused(format!(
                    "command {} is not executable",
                    command.display()
                )));
            }
        }
        let content_hash = lillux::cas::sha256_hex(&content);
        self.prepare_code_bytes(
            command,
            Some(command),
            &content_hash,
            &content,
            project_destination,
            canonical_project,
            bundle_roots,
        )
    }

    fn prepare_code_bytes(
        &self,
        original: &Path,
        expected_canonical_source: Option<&Path>,
        content_hash: &str,
        content: &[u8],
        project_destination: &Path,
        canonical_project: &Path,
        bundle_roots: &[PathBuf],
    ) -> Result<PreparedVerifiedCode, EngineError> {
        let canonical_source = canonicalize_context_mount("code source", original)?;
        if expected_canonical_source.is_some_and(|expected| expected != canonical_source) {
            return Err(refused(format!(
                "code source {} changed filesystem identity before materialization",
                original.display()
            )));
        }
        let (mirror, destination) = code_namespace_layout(
            original,
            &canonical_source,
            project_destination,
            canonical_project,
            bundle_roots,
        )?;
        let artifact_root = self
            .verified_artifacts
            .as_deref()
            .expect("enforced sandbox runtime has a verified artifact store");
        let artifact = artifact_root.materialize(content_hash, content_hash, content)?;
        let artifact = ReadableMount {
            source_handle: artifact.handle,
            source: artifact.path,
            destination,
        };
        Ok(PreparedVerifiedCode {
            original: original.to_path_buf(),
            canonical_source,
            mirror,
            artifact,
        })
    }
}

/// Disabled runtime used by in-memory fixtures that have no node filesystem.
/// Production composition must call [`SandboxRuntime::load`].
impl Default for SandboxRuntime {
    fn default() -> Self {
        Self::resolve(
            SandboxPolicy::default_disabled(),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("compiled disabled sandbox fixture policy is valid")
    }
}

fn resolve_backend(
    policy: &SandboxBackendPolicy,
    artifacts: &VerifiedArtifactStore,
) -> Result<(PathBuf, Arc<std::fs::File>, String, String), EngineError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (policy, artifacts);
        return Err(refused(
            "Bubblewrap sandbox enforcement is supported only on Linux".to_string(),
        ));
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt as _;

        if !policy.executable.is_absolute() {
            return Err(refused(format!(
                "sandbox backend executable must be absolute: {}",
                policy.executable.display()
            )));
        }
        let executable = std::fs::canonicalize(&policy.executable).map_err(|error| {
            refused(format!(
                "sandbox backend {} cannot be resolved: {error}",
                policy.executable.display()
            ))
        })?;
        let (content, metadata) = artifacts.read_source("sandbox backend", &executable)?;
        if !metadata.is_file() {
            return Err(refused(format!(
                "sandbox backend {} is not a file",
                executable.display()
            )));
        }
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(refused(format!(
                "sandbox backend {} is not executable",
                executable.display()
            )));
        }
        if metadata.permissions().mode() & 0o6000 != 0 {
            return Err(refused(format!(
                "sandbox backend {} must not be setuid or setgid",
                executable.display()
            )));
        }
        reject_file_capabilities("sandbox backend", &executable)?;
        let content_hash = lillux::cas::sha256_hex(&content);
        let artifact =
            artifacts.materialize(&format!("backend-{content_hash}"), &content_hash, &content)?;
        let handle = artifact.handle;
        let version = probe_bubblewrap_backend(&handle)?;
        Ok((
            executable,
            handle,
            format!("sha256:{content_hash}"),
            version,
        ))
    }
}

fn probe_bubblewrap_backend(handle: &Arc<std::fs::File>) -> Result<String, EngineError> {
    let command = format!("/proc/self/fd/{}", mount_fd_arg(handle));
    let run = |argument: &str| {
        lillux::run(lillux::SubprocessRequest {
            cmd: command.clone(),
            args: vec![argument.to_string()],
            cwd: Some("/".to_string()),
            envs: Vec::new(),
            stdin_data: None,
            timeout: 5.0,
            limits: None,
            inherited_fds: vec![handle.clone()],
            supervised_status: None,
        })
    };

    let version_result = run("--version");
    if !version_result.success {
        return Err(refused(format!(
            "captured sandbox backend failed its version probe: {}",
            version_result.stderr.trim()
        )));
    }
    let version = version_result
        .stdout
        .trim()
        .strip_prefix("bubblewrap ")
        .filter(|version| !version.is_empty() && !version.chars().any(char::is_whitespace))
        .ok_or_else(|| {
            refused(format!(
                "captured sandbox backend returned an invalid version: `{}`",
                version_result.stdout.trim()
            ))
        })?;
    require_bubblewrap_version(version)?;

    let help_result = run("--help");
    if !help_result.success {
        return Err(refused(format!(
            "captured sandbox backend failed its feature probe: {}",
            help_result.stderr.trim()
        )));
    }
    let help = format!("{}\n{}", help_result.stdout, help_result.stderr);
    let help_tokens = help
        .split_whitespace()
        .collect::<std::collections::HashSet<_>>();
    for required in [
        "--args",
        "--bind-fd",
        "--cap-drop",
        "--chdir",
        "--clearenv",
        "--dev-bind",
        "--die-with-parent",
        "--json-status-fd",
        "--new-session",
        "--ro-bind-fd",
        "--tmpfs",
        "--unshare-all",
        "--argv0",
    ] {
        if !help_tokens.contains(required) {
            return Err(refused(format!(
                "captured sandbox backend does not support required option {required}"
            )));
        }
    }
    Ok(version.to_string())
}

fn require_bubblewrap_version(version: &str) -> Result<(), EngineError> {
    let mut parts = version.split('.');
    let parse_part = |part: Option<&str>, name: &str| {
        let part = part.unwrap_or_default();
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(refused(format!(
                "captured sandbox backend has invalid {name} version component `{version}`"
            )));
        }
        part.parse::<u64>().map_err(|_| {
            refused(format!(
                "captured sandbox backend has invalid {name} version component `{version}`"
            ))
        })
    };
    let parsed = (
        parse_part(parts.next(), "major")?,
        parse_part(parts.next(), "minor")?,
        parse_part(parts.next(), "patch")?,
    );
    if parts.next().is_some() {
        return Err(refused(format!(
            "captured sandbox backend version `{version}` must use major.minor.patch"
        )));
    }
    if parsed < (0, 11, 0) {
        return Err(refused(format!(
            "captured sandbox backend version {version} is unsupported; Bubblewrap 0.11.0 or newer is required"
        )));
    }
    Ok(())
}

fn reject_file_capabilities(kind: &str, path: &Path) -> Result<(), EngineError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (kind, path);
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;

        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            refused(format!(
                "{kind} path contains an interior NUL: {}",
                path.display()
            ))
        })?;
        let attribute = c"security.capability";
        let result =
            unsafe { libc::getxattr(c_path.as_ptr(), attribute.as_ptr(), std::ptr::null_mut(), 0) };
        if result >= 0 {
            return Err(refused(format!(
                "{kind} {} must not carry Linux file capabilities",
                path.display()
            )));
        }
        let error = std::io::Error::last_os_error();
        let code = error.raw_os_error();
        if code != Some(libc::ENODATA)
            && code != Some(libc::ENOTSUP)
            && code != Some(libc::EOPNOTSUPP)
        {
            return Err(refused(format!(
                "{kind} {} capabilities cannot be inspected: {error}",
                path.display()
            )));
        }
        Ok(())
    }
}

fn validate_policy_semantics(policy: &SandboxPolicy) -> Result<(), EngineError> {
    if !policy.backend.executable.is_absolute() {
        return Err(refused(format!(
            "sandbox backend executable must be absolute: {}",
            policy.backend.executable.display()
        )));
    }
    for configured in &policy.filesystem.readable {
        let is_placeholder = matches!(
            configured.as_str(),
            "{project}"
                | "{cwd}"
                | "{node_public_identity}"
                | "{daemon_socket}"
                | "{bundle_roots}"
                | "{node_trusted_keys}"
                | "{verified_code}"
        );
        if !is_placeholder {
            validate_namespace_destination("readable policy path", Path::new(configured))?;
        }
    }
    for configured in &policy.filesystem.writable {
        if !matches!(
            configured.as_str(),
            "{project}" | "{cwd}" | "{checkpoint_dir}"
        ) {
            validate_namespace_destination("writable policy path", Path::new(configured))?;
        }
    }
    for allowed in &policy.environment.allow {
        let wildcard_count = allowed.bytes().filter(|byte| *byte == b'*').count();
        let valid = !allowed.is_empty()
            && (wildcard_count == 0
                || allowed == "*"
                || (wildcard_count == 1 && allowed.ends_with('*')));
        if !valid {
            return Err(refused(format!(
                "sandbox environment allow pattern `{allowed}` must be exact, `*`, or a prefix ending in one `*`"
            )));
        }
    }
    validate_artifact_limits(&policy.limits)?;
    Ok(())
}

fn validate_artifact_limits(policy: &SandboxLimitsPolicy) -> Result<(), EngineError> {
    if policy.stdout_bytes == 0 {
        return Err(refused(
            "sandbox stdout byte limit must be greater than zero".to_string(),
        ));
    }
    if policy.stderr_bytes == 0 {
        return Err(refused(
            "sandbox stderr byte limit must be greater than zero".to_string(),
        ));
    }
    if policy.verified_artifact_file_bytes == 0 {
        return Err(refused(
            "sandbox verified-artifact per-file byte limit must be greater than zero".to_string(),
        ));
    }
    if policy.verified_artifact_total_bytes < policy.verified_artifact_file_bytes {
        return Err(refused(format!(
            "sandbox verified-artifact total-byte limit {} is below per-file limit {}",
            policy.verified_artifact_total_bytes, policy.verified_artifact_file_bytes
        )));
    }
    if policy.verified_artifact_files == 0 {
        return Err(refused(
            "sandbox verified-artifact file limit must be greater than zero".to_string(),
        ));
    }
    Ok(())
}

fn validate_enforced_limits(policy: &SandboxLimitsPolicy) -> Result<(), EngineError> {
    let limits = lillux::SubprocessLimits {
        max_open_files: policy.open_files,
        max_stdout_bytes: Some(policy.stdout_bytes),
        max_stderr_bytes: Some(policy.stderr_bytes),
    };
    lillux::validate_subprocess_limits(Some(&limits)).map_err(|reason| {
        refused(format!(
            "sandbox subprocess limits are not enforceable: {reason}"
        ))
    })
}

fn environment_name_allowed(policy: &SandboxEnvironmentPolicy, name: &str) -> bool {
    policy.allow.iter().any(|allowed| {
        allowed == "*"
            || allowed == name
            || allowed
                .strip_suffix('*')
                .is_some_and(|prefix| name.starts_with(prefix))
    })
}

fn rewrite_verified_code_references(
    args: &mut [String],
    envs: &mut [(String, String)],
    source: &Path,
    canonical_source: &Path,
    destination: &Path,
) -> Result<(), EngineError> {
    let source = source.to_str().ok_or_else(|| {
        refused(format!(
            "verified code path is not valid UTF-8: {}",
            source.display()
        ))
    })?;
    let destination = destination.to_str().ok_or_else(|| {
        refused(format!(
            "verified code namespace path is not valid UTF-8: {}",
            destination.display()
        ))
    })?;
    for value in args
        .iter_mut()
        .chain(envs.iter_mut().map(|(_, value)| value))
    {
        if let Some(rewritten) = replace_delimited_path(value, source, destination) {
            *value = rewritten;
        } else {
            let exact_path = Path::new(value);
            if exact_path.is_absolute()
                && std::fs::canonicalize(exact_path)
                    .ok()
                    .is_some_and(|path| path == canonical_source)
            {
                *value = destination.to_string();
            } else if let Some((prefix, path)) = value.split_once('=') {
                let path = Path::new(path);
                if path.is_absolute()
                    && std::fs::canonicalize(path)
                        .ok()
                        .is_some_and(|path| path == canonical_source)
                {
                    *value = format!("{prefix}={destination}");
                }
            }
        }

        if value.contains(source)
            || canonical_source
                .to_str()
                .is_some_and(|canonical| value.contains(canonical))
            || contains_canonical_path_reference(value, canonical_source)
        {
            return Err(refused(format!(
                "verified code reference in argument or environment value cannot be rewritten safely: {value:?}"
            )));
        }
    }
    Ok(())
}

/// Detect absolute path spellings (including symlink aliases) embedded in a
/// structured value. Any alias that still resolves to verified code after the
/// supported rewrite forms is refused rather than allowed to reopen live bytes.
fn contains_canonical_path_reference(value: &str, canonical_source: &Path) -> bool {
    fn terminates_path(character: char) -> bool {
        character.is_whitespace()
            || matches!(
                character,
                '=' | ':'
                    | ';'
                    | ','
                    | '"'
                    | '\''
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '('
                    | ')'
                    | '?'
                    | '&'
                    | '|'
            )
    }

    value
        .char_indices()
        .filter(|(_, character)| *character == '/')
        .any(|(start, _)| {
            let suffix = &value[start..];
            suffix
                .char_indices()
                .skip(1)
                .filter_map(|(index, character)| terminates_path(character).then_some(index))
                .chain(std::iter::once(suffix.len()))
                .any(|end| {
                    let candidate = Path::new(&suffix[..end]);
                    std::fs::canonicalize(candidate)
                        .ok()
                        .is_some_and(|path| path == canonical_source)
                })
        })
}

/// Replace a verified file path only when it occupies a complete token inside
/// an argument or structured environment value. A raw substring replacement
/// would also rewrite unrelated paths such as `entry.py.backup`.
fn replace_delimited_path(value: &str, source: &str, destination: &str) -> Option<String> {
    if source.is_empty() {
        return None;
    }

    fn boundary(character: Option<char>) -> bool {
        character.is_none_or(|character| {
            character.is_whitespace()
                || matches!(
                    character,
                    '=' | ':' | ';' | ',' | '"' | '\'' | '[' | ']' | '{' | '}' | '(' | ')'
                )
        })
    }

    let mut cursor = 0;
    let mut rewritten = String::with_capacity(value.len());
    let mut changed = false;
    while let Some(relative_start) = value[cursor..].find(source) {
        let start = cursor + relative_start;
        let end = start + source.len();
        if boundary(value[..start].chars().next_back()) && boundary(value[end..].chars().next()) {
            rewritten.push_str(&value[cursor..start]);
            rewritten.push_str(destination);
            cursor = end;
            changed = true;
        } else {
            let first_len = value[start..]
                .chars()
                .next()
                .expect("a non-empty source match has one character")
                .len_utf8();
            rewritten.push_str(&value[cursor..start + first_len]);
            cursor = start + first_len;
        }
    }
    rewritten.push_str(&value[cursor..]);
    changed.then_some(rewritten)
}

fn is_on_system_runtime_surface(path: &Path) -> bool {
    ["/usr", "/bin", "/lib", "/lib64"]
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .any(|root| path.starts_with(root))
}

fn is_lexically_on_system_runtime_surface(path: &Path) -> bool {
    ["/usr", "/bin", "/sbin", "/lib", "/lib64"]
        .iter()
        .any(|root| path.starts_with(root))
}

fn code_namespace_layout(
    original: &Path,
    canonical_source: &Path,
    project_destination: &Path,
    canonical_project: &Path,
    bundle_roots: &[PathBuf],
) -> Result<(Option<ReadableMount>, PathBuf), EngineError> {
    let mut candidates = vec![(
        canonical_project.to_path_buf(),
        project_destination.to_path_buf(),
    )];
    for bundle_root in bundle_roots {
        candidates.push((
            canonicalize_context_mount("bundle root", bundle_root)?,
            bundle_root.clone(),
        ));
    }
    let selected = candidates
        .into_iter()
        .filter(|(canonical, _)| canonical_source.starts_with(canonical))
        .max_by_key(|(canonical, _)| canonical.components().count());

    let Some((authority_source, authority_destination)) = selected else {
        let file_name = canonical_source.file_name().ok_or_else(|| {
            refused(format!(
                "code source {} has no file name",
                canonical_source.display()
            ))
        })?;
        let authority_id = lillux::cas::sha256_hex(canonical_source.as_os_str().as_encoded_bytes());
        let namespace_root = PathBuf::from(VERIFIED_CODE_SANDBOX_ROOT).join(authority_id);
        return Ok((None, namespace_root.join(file_name)));
    };
    let relative = original
        .strip_prefix(&authority_destination)
        .ok()
        .or_else(|| canonical_source.strip_prefix(&authority_source).ok())
        .ok_or_else(|| {
            refused(format!(
                "code source {} is not beneath namespace authority {}",
                original.display(),
                authority_destination.display()
            ))
        })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(refused(format!(
            "code source {} has an unsafe namespace-relative path",
            original.display()
        )));
    }
    let authority_id = lillux::cas::sha256_hex(authority_source.as_os_str().as_encoded_bytes());
    let namespace_root = PathBuf::from(VERIFIED_CODE_SANDBOX_ROOT).join(authority_id);
    Ok((
        Some(ReadableMount {
            source_handle: pin_mount_source("code authority", &authority_source)?,
            source: authority_source,
            destination: namespace_root.clone(),
        }),
        namespace_root.join(relative),
    ))
}

fn read_regular_file_bytes_limited(
    kind: &str,
    path: &Path,
    max_bytes: u64,
) -> Result<(Vec<u8>, std::fs::Metadata), EngineError> {
    if !path.is_absolute() {
        return Err(refused(format!(
            "{kind} path must be absolute: {}",
            path.display()
        )));
    }

    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
    }
    .map_err(|error| {
        refused(format!(
            "{kind} {} cannot be opened: {error}",
            path.display()
        ))
    })?;

    #[cfg(not(unix))]
    let file = std::fs::File::open(path).map_err(|error| {
        refused(format!(
            "{kind} {} cannot be opened: {error}",
            path.display()
        ))
    })?;

    read_regular_file_handle_limited(kind, path, file, max_bytes)
}

fn read_regular_file_handle_limited(
    kind: &str,
    path: &Path,
    mut file: std::fs::File,
    max_bytes: u64,
) -> Result<(Vec<u8>, std::fs::Metadata), EngineError> {
    file.seek(std::io::SeekFrom::Start(0)).map_err(|error| {
        refused(format!(
            "{kind} {} cannot be rewound: {error}",
            path.display()
        ))
    })?;
    let metadata = file.metadata().map_err(|error| {
        refused(format!(
            "{kind} {} cannot be inspected: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(refused(format!(
            "{kind} {} must be a regular non-symlink file",
            path.display()
        )));
    }
    if metadata.len() > max_bytes {
        return Err(refused(format!(
            "{kind} {} is {} bytes, exceeding configured per-file limit {max_bytes}",
            path.display(),
            metadata.len()
        )));
    }
    let mut content = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut content)
        .map_err(|error| refused(format!("{kind} {} cannot be read: {error}", path.display())))?;
    if u64::try_from(content.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(refused(format!(
            "{kind} {} grew beyond configured per-file limit {max_bytes} while being read",
            path.display()
        )));
    }
    Ok((content, metadata))
}

fn canonicalize_launch_path(kind: &str, path: &Path) -> Result<PathBuf, EngineError> {
    std::fs::canonicalize(path).map_err(|error| {
        refused(format!(
            "{kind} path {} cannot be resolved: {error}",
            path.display()
        ))
    })
}

/// Open one already-canonical mount source and retain an inheritable O_PATH
/// descriptor for Bubblewrap. Validation and mount execution therefore refer
/// to the same kernel object; a pathname swap after this point cannot redirect
/// the bind to a protected node path.
fn pin_mount_source(kind: &str, path: &Path) -> Result<Arc<std::fs::File>, EngineError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (kind, path);
        return Err(refused(
            "fd-pinned sandbox mounts are supported only on Linux".to_string(),
        ));
    }

    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd as _, FromRawFd as _};
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::fs::FileTypeExt as _;

        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            refused(format!(
                "{kind} path contains an interior NUL: {}",
                path.display()
            ))
        })?;
        let mut fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(refused(format!(
                "{kind} {} cannot be pinned: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        if fd <= libc::STDERR_FILENO {
            let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            let duplicate_error = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            if duplicated < 0 {
                return Err(refused(format!(
                    "{kind} {} descriptor cannot be moved above stdio: {duplicate_error}",
                    path.display()
                )));
            }
            fd = duplicated;
        }
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|error| {
            refused(format!(
                "pinned {kind} {} cannot be inspected: {error}",
                path.display()
            ))
        })?;
        let file_type = metadata.file_type();
        if !(file_type.is_file() || file_type.is_dir() || file_type.is_socket()) {
            return Err(refused(format!(
                "{kind} {} must be a regular file, directory, or Unix socket",
                path.display()
            )));
        }

        let fd_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
        let observed = std::fs::read_link(&fd_path).map_err(|error| {
            refused(format!(
                "pinned {kind} {} cannot be resolved through {}: {error}",
                path.display(),
                fd_path.display()
            ))
        })?;
        if observed != path {
            return Err(refused(format!(
                "{kind} {} changed while it was being pinned (opened {})",
                path.display(),
                observed.display()
            )));
        }

        Ok(Arc::new(file))
    }
}

fn mount_fd_arg(handle: &Arc<std::fs::File>) -> String {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        return handle.as_raw_fd().to_string();
    }
    #[cfg(not(unix))]
    {
        let _ = handle;
        unreachable!("enforced sandbox mounts are Linux-only")
    }
}

fn same_file_identity(left: &std::fs::File, right: &std::fs::File) -> Result<bool, EngineError> {
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        Err(refused(
            "sandbox file-identity comparison is unavailable on this platform".to_string(),
        ))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let left = left
            .metadata()
            .map_err(|error| refused(format!("sandbox authority cannot be inspected: {error}")))?;
        let right = right
            .metadata()
            .map_err(|error| refused(format!("sandbox authority cannot be inspected: {error}")))?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
}

/// Serialize Bubblewrap's complete option vector into an immutable anonymous
/// file. The host process command line then contains only `--args <fd>`;
/// target arguments and `--setenv` values never appear in `/proc/*/cmdline`.
fn seal_bubblewrap_args(args: &[String]) -> Result<Arc<std::fs::File>, EngineError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        return Err(refused(
            "fd-backed Bubblewrap arguments are supported only on Linux".to_string(),
        ));
    }

    #[cfg(target_os = "linux")]
    {
        use std::io::{Seek as _, Write as _};
        use std::os::fd::{AsRawFd as _, FromRawFd as _};

        let mut fd = unsafe {
            libc::memfd_create(
                c"ryeos-bwrap-args".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(refused(format!(
                "Bubblewrap argument memfd cannot be created: {}",
                std::io::Error::last_os_error()
            )));
        }
        if fd <= libc::STDERR_FILENO {
            let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            let duplicate_error = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            if duplicated < 0 {
                return Err(refused(format!(
                    "Bubblewrap argument descriptor cannot be moved above stdio: {duplicate_error}"
                )));
            }
            fd = duplicated;
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        for (index, argument) in args.iter().enumerate() {
            if argument.as_bytes().contains(&0) {
                return Err(refused(format!(
                    "Bubblewrap argument {index} contains an interior NUL"
                )));
            }
            file.write_all(argument.as_bytes()).map_err(|error| {
                refused(format!(
                    "Bubblewrap argument {index} cannot be written to its private descriptor: {error}"
                ))
            })?;
            file.write_all(&[0]).map_err(|error| {
                refused(format!(
                    "Bubblewrap argument {index} terminator cannot be written to its private descriptor: {error}"
                ))
            })?;
        }
        file.seek(std::io::SeekFrom::Start(0)).map_err(|error| {
            refused(format!(
                "Bubblewrap argument descriptor cannot be rewound: {error}"
            ))
        })?;
        let required_seals =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, required_seals) } < 0 {
            return Err(refused(format!(
                "Bubblewrap argument descriptor cannot be sealed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let observed_seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        if observed_seals < 0 {
            return Err(refused(format!(
                "Bubblewrap argument descriptor seals cannot be verified: {}",
                std::io::Error::last_os_error()
            )));
        }
        if observed_seals & required_seals != required_seals {
            return Err(refused(format!(
                "Bubblewrap argument descriptor is missing required seals (observed {observed_seals:#x})"
            )));
        }
        Ok(Arc::new(file))
    }
}

fn resolve_writable_mount(
    configured: &str,
    project_destination: &Path,
    canonical_project: &Path,
    cwd_destination: &Path,
    canonical_cwd: &Path,
    checkpoint_destination: Option<&Path>,
    canonical_checkpoint_dir: Option<&Path>,
    checkpoint_source_handle: Option<&Arc<std::fs::File>>,
    project_source_handle: Option<&Arc<std::fs::File>>,
) -> Result<Option<WritableMount>, EngineError> {
    let authority = if configured == "{checkpoint_dir}" {
        WritableMountAuthority::DaemonCheckpoint
    } else {
        WritableMountAuthority::Policy
    };
    let (source, destination) = match configured {
        "{project}" => (
            canonical_project.to_path_buf(),
            project_destination.to_path_buf(),
        ),
        "{cwd}" => (canonical_cwd.to_path_buf(), cwd_destination.to_path_buf()),
        "{checkpoint_dir}" => match (canonical_checkpoint_dir, checkpoint_destination) {
            (Some(source), Some(destination)) => (source.to_path_buf(), destination.to_path_buf()),
            (None, None) => return Ok(None),
            _ => {
                return Err(refused(
                    "sandbox checkpoint source/destination mismatch".to_string(),
                ))
            }
        },
        other => {
            let destination = PathBuf::from(other);
            let source = std::fs::canonicalize(&destination).map_err(|error| {
                refused(format!(
                    "sandbox path {} cannot be resolved: {error}",
                    destination.display()
                ))
            })?;
            (source, destination)
        }
    };
    if !destination.is_absolute() {
        return Err(refused(format!(
            "sandbox path `{}` is not absolute",
            destination.display()
        )));
    }
    let source_handle = if authority == WritableMountAuthority::DaemonCheckpoint {
        checkpoint_source_handle
            .cloned()
            .ok_or_else(|| refused("daemon checkpoint mount has no pinned authority".to_string()))?
    } else if configured == "{project}" && project_source_handle.is_some() {
        project_source_handle
            .cloned()
            .expect("checked project source handle")
    } else {
        pin_mount_source("writable mount", &source)?
    };
    Ok(Some(WritableMount {
        source,
        destination,
        authority,
        source_handle,
    }))
}

fn resolve_readable_mounts(
    configured: &str,
    project_destination: &Path,
    canonical_project: &Path,
    cwd_destination: &Path,
    canonical_cwd: &Path,
    app_root: Option<&Path>,
    app_root_authority: Option<&lillux::PinnedDirectory>,
    app_root_destination: Option<&Path>,
    daemon_socket: Option<&PinnedDaemonSocket>,
    requested_daemon_socket: Option<&Path>,
    bundle_roots: &[PathBuf],
    node_trusted_keys_dir: Option<&Path>,
    verified_code_mounts: &[ReadableMount],
) -> Result<Vec<ReadableMount>, EngineError> {
    if configured == "{node_public_identity}" {
        let source_path = app_root
            .ok_or_else(|| {
                refused(
                    "sandbox runtime cannot resolve {node_public_identity} without an app root"
                        .to_string(),
                )
            })?
            .join(crate::AI_DIR)
            .join("node/identity/public-identity.json");
        let authority = app_root_authority.ok_or_else(|| {
            refused(
                "sandbox runtime cannot resolve {node_public_identity} without pinned app-root authority"
                    .to_string(),
            )
        })?;
        let destination = app_root_destination
            .ok_or_else(|| {
                refused(
                    "sandbox runtime cannot resolve {node_public_identity} destination without an app root"
                        .to_string(),
                )
            })?
            .join(crate::AI_DIR)
            .join("node/identity/public-identity.json");
        let identity_parent = open_relative_directory(
            authority,
            &[crate::AI_DIR, "node", "identity"],
            "node public identity parent",
        )?;
        let source_handle = identity_parent
            .open_regular("public-identity.json".as_ref(), false)
            .map_err(|error| refused(format!("node public identity cannot be opened: {error}")))?
            .ok_or_else(|| {
                refused(format!(
                    "node public identity {} is missing",
                    source_path.display()
                ))
            })?;
        return Ok(vec![ReadableMount {
            source: source_path,
            destination,
            source_handle: Arc::new(source_handle),
        }]);
    }

    if configured == "{daemon_socket}" {
        let Some(requested) = requested_daemon_socket else {
            return Ok(Vec::new());
        };
        let configured = daemon_socket.ok_or_else(|| {
            refused(
                "sandbox launch requested daemon IPC without a daemon-pinned socket path"
                    .to_string(),
            )
        })?;
        if !requested.is_absolute() {
            return Err(refused(format!(
                "requested daemon socket path must be absolute: {}",
                requested.display()
            )));
        }
        if requested != configured.destination {
            return Err(refused(format!(
                "sandbox launch requested daemon socket {}, expected {}",
                requested.display(),
                configured.destination.display()
            )));
        }
        return Ok(vec![ReadableMount {
            source: configured.source.clone(),
            destination: requested.to_path_buf(),
            source_handle: configured.entry.clone(),
        }]);
    }

    if configured == "{bundle_roots}" {
        return bundle_roots
            .iter()
            .map(|destination| {
                let source = canonicalize_context_mount("bundle root", destination)?;
                let source_handle = pin_mount_source("bundle root", &source)?;
                Ok(ReadableMount {
                    source,
                    destination: destination.to_path_buf(),
                    source_handle,
                })
            })
            .collect();
    }

    if configured == "{node_trusted_keys}" {
        let Some(destination) = node_trusted_keys_dir else {
            return Ok(Vec::new());
        };
        let source = canonicalize_context_mount("node trusted-keys directory", destination)?;
        let source_handle = pin_mount_source("node trusted-keys directory", &source)?;
        return Ok(vec![ReadableMount {
            source,
            destination: destination.to_path_buf(),
            source_handle,
        }]);
    }

    if configured == "{verified_code}" {
        return Ok(verified_code_mounts.to_vec());
    }

    let (source, destination) = match configured {
        "{project}" => (
            canonical_project.to_path_buf(),
            project_destination.to_path_buf(),
        ),
        "{cwd}" => (canonical_cwd.to_path_buf(), cwd_destination.to_path_buf()),
        other => {
            let destination = PathBuf::from(other);
            let source = std::fs::canonicalize(&destination).map_err(|error| {
                refused(format!(
                    "sandbox readable path {} cannot be resolved: {error}",
                    destination.display()
                ))
            })?;
            (source, destination)
        }
    };
    if !destination.is_absolute() && !matches!(configured, "{project}" | "{cwd}") {
        return Err(refused(format!(
            "sandbox path `{}` is not absolute",
            destination.display()
        )));
    }
    let source_handle = pin_mount_source("readable mount", &source)?;
    Ok(vec![ReadableMount {
        destination,
        source,
        source_handle,
    }])
}

fn canonicalize_context_mount(kind: &str, path: &Path) -> Result<PathBuf, EngineError> {
    if !path.is_absolute() {
        return Err(refused(format!(
            "sandbox {kind} path must be absolute: {}",
            path.display()
        )));
    }
    canonicalize_launch_path(kind, path)
}

/// Require a path spelling that Bubblewrap cannot reinterpret through `.` or
/// `..` while constructing its new root. Host-side authority checks use
/// canonical sources, but namespace destinations deliberately retain their
/// configured spelling, so every destination must cross this lexical gate.
fn validate_namespace_destination(kind: &str, path: &Path) -> Result<(), EngineError> {
    use std::path::Component;

    let path_text = path.to_str().ok_or_else(|| {
        refused(format!(
            "sandbox {kind} namespace destination is not valid UTF-8: {}",
            path.display()
        ))
    })?;
    if path_text.contains('\0') {
        return Err(refused(format!(
            "sandbox {kind} namespace destination contains an interior NUL"
        )));
    }
    let mut components = path.components();
    #[cfg(windows)]
    let root_is_valid = matches!(components.next(), Some(Component::Prefix(_)))
        && matches!(components.next(), Some(Component::RootDir));
    #[cfg(not(windows))]
    let root_is_valid = matches!(components.next(), Some(Component::RootDir));
    let remainder_is_normal = components.all(|component| matches!(component, Component::Normal(_)));
    if !root_is_valid || !remainder_is_normal {
        return Err(refused(format!(
            "sandbox {kind} namespace destination must contain only an absolute root followed by normal path components: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_thread_path_component(thread_id: &str) -> Result<(), EngineError> {
    use std::path::Component;

    let mut components = Path::new(thread_id).components();
    let is_single_normal_component =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if thread_id.is_empty() || !is_single_normal_component {
        return Err(refused(format!(
            "sandbox thread id must be one normal path component: `{thread_id}`"
        )));
    }
    Ok(())
}

fn validate_writable_mount(
    path: &Path,
    app_root: Option<&Path>,
    backend: &Path,
    daemon_socket: Option<&Path>,
    canonical_project: &Path,
    project_authority: SandboxProjectAuthority,
    runtime_workspace_authorized: bool,
    mount_authority: WritableMountAuthority,
    canonical_checkpoint_dir: Option<&Path>,
) -> Result<(), EngineError> {
    if path == Path::new("/") {
        return Err(refused(
            "sandbox writable path `/` would expose the entire host".to_string(),
        ));
    }

    if let Some(app_root) = app_root {
        let execution_root = app_root.join(crate::AI_DIR).join("state/cache/executions");
        let is_exact_runtime_workspace = project_authority
            == SandboxProjectAuthority::RuntimeWorkspace
            && runtime_workspace_authorized
            && canonical_project.parent() == Some(execution_root.as_path())
            && path.starts_with(canonical_project);
        let is_exact_daemon_checkpoint = mount_authority
            == WritableMountAuthority::DaemonCheckpoint
            && canonical_checkpoint_dir == Some(path);
        if paths_overlap(path, app_root)
            && !is_exact_runtime_workspace
            && !is_exact_daemon_checkpoint
        {
            return Err(refused(format!(
                "sandbox writable path {} overlaps protected app root {}",
                path.display(),
                app_root.display()
            )));
        }
    }
    if paths_overlap(path, backend) {
        return Err(refused(format!(
            "sandbox writable path {} overlaps sandbox backend {}",
            path.display(),
            backend.display()
        )));
    }
    if let Some(socket) = daemon_socket {
        if paths_overlap(path, socket) {
            return Err(refused(format!(
                "sandbox writable path {} overlaps protected daemon socket {}",
                path.display(),
                socket.display()
            )));
        }
    }

    for protected in [
        "/boot", "/dev", "/etc", "/proc", "/run", "/sys", "/usr", "/bin", "/sbin", "/lib", "/lib64",
    ] {
        let protected = Path::new(protected);
        if protected.exists() {
            let protected = std::fs::canonicalize(protected).unwrap_or_else(|_| protected.into());
            if paths_overlap(path, &protected) {
                return Err(refused(format!(
                    "sandbox writable path {} overlaps protected system root {}",
                    path.display(),
                    protected.display()
                )));
            }
        }
    }

    if let Some(home) = std::env::var_os("HOME") {
        if let Ok(home) = std::fs::canonicalize(home) {
            // Projects beneath HOME are normal; HOME itself or an ancestor is
            // too broad because it would expose unrelated credentials/config.
            if home.starts_with(path) {
                return Err(refused(format!(
                    "sandbox writable path {} contains protected home directory {}",
                    path.display(),
                    home.display()
                )));
            }
        }
    }

    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn append_parent_directories(args: &mut Vec<String>, path: &Path) {
    let mut parents = path
        .ancestors()
        .skip(1)
        .filter(|parent| *parent != Path::new("/"))
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    parents.reverse();
    for parent in parents {
        args.extend(["--dir".to_string(), parent.to_string_lossy().into_owned()]);
    }
}

fn refused(reason: String) -> EngineError {
    EngineError::SandboxPolicyRefused { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_yaml(mode: &str, executable: &Path) -> String {
        format!(
            "version: 1\nmode: {mode}\nbackend:\n  kind: bubblewrap\n  executable: {}\nfilesystem:\n  readable: []\n  writable: [\"{{project}}\"]\nnetwork:\n  mode: isolated\nenvironment:\n  allow: [\"PATH\"]\nlimits:\n  open_files: 128\n  stdout_bytes: 8388608\n  stderr_bytes: 8388608\n  verified_artifact_file_bytes: 67108864\n  verified_artifact_total_bytes: 268435456\n  verified_artifact_files: 4096\n",
            executable.display()
        )
    }

    fn write_policy(app_root: &Path, body: &str) {
        std::fs::create_dir_all(app_root.join(".ai/node")).unwrap();
        std::fs::write(app_root.join(".ai/node/sandbox.yaml"), body).unwrap();
    }

    fn request(project: &Path) -> lillux::SubprocessRequest {
        lillux::SubprocessRequest {
            cmd: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "true".to_string()],
            cwd: Some(project.to_string_lossy().into_owned()),
            envs: vec![("PATH".to_string(), "/usr/bin".to_string())],
            stdin_data: None,
            timeout: 1.0,
            limits: None,
            inherited_fds: Vec::new(),
            supervised_status: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn bubblewrap_args(request: &lillux::SubprocessRequest) -> Vec<String> {
        use std::io::{Read as _, Seek as _};
        use std::os::fd::AsRawFd as _;

        assert_eq!(request.args.first().map(String::as_str), Some("--args"));
        let args_fd = request
            .args
            .get(1)
            .expect("Bubblewrap --args descriptor")
            .parse::<i32>()
            .expect("numeric Bubblewrap --args descriptor");
        let args_file = request
            .inherited_fds
            .iter()
            .find(|file| file.as_raw_fd() == args_fd)
            .expect("inherited Bubblewrap --args descriptor");
        let mut args_file = args_file.try_clone().unwrap();
        args_file.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut encoded = Vec::new();
        args_file.read_to_end(&mut encoded).unwrap();
        assert_eq!(encoded.last(), Some(&0));
        let mut encoded_args = encoded
            .split(|byte| *byte == 0)
            .map(|argument| String::from_utf8(argument.to_vec()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(encoded_args.pop().as_deref(), Some(""));
        encoded_args
    }

    #[test]
    fn verified_path_rewrite_requires_token_boundaries() {
        assert_eq!(
            replace_delimited_path(
                "--entry=/project/entry.py",
                "/project/entry.py",
                "/run/verified/entry.py"
            )
            .as_deref(),
            Some("--entry=/run/verified/entry.py")
        );
        assert_eq!(
            replace_delimited_path(
                "{\"entry\":\"/project/entry.py\"}",
                "/project/entry.py",
                "/run/verified/entry.py"
            )
            .as_deref(),
            Some("{\"entry\":\"/run/verified/entry.py\"}")
        );
        assert_eq!(
            replace_delimited_path(
                "/project/entry.py.backup",
                "/project/entry.py",
                "/run/verified/entry.py"
            ),
            None
        );
    }

    #[test]
    fn verified_path_rewrite_refuses_ambiguous_live_reference() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("entry.py");
        std::fs::write(&source, b"print('verified')\n").unwrap();
        let canonical_source = std::fs::canonicalize(&source).unwrap();
        let destination = Path::new("/run/ryeos/verified-code/entry.py");
        let mut args = vec![format!("{}.backup", source.display())];
        let mut envs = Vec::new();

        let error = rewrite_verified_code_references(
            &mut args,
            &mut envs,
            &source,
            &canonical_source,
            destination,
        )
        .unwrap_err();

        assert!(error.to_string().contains("cannot be rewritten safely"));
    }

    #[cfg(unix)]
    #[test]
    fn verified_path_rewrite_refuses_embedded_symlink_alias() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("entry.py");
        std::fs::write(&source, b"print('verified')\n").unwrap();
        let alias = root.path().join("entry:alias.py");
        symlink(&source, &alias).unwrap();
        let canonical_source = std::fs::canonicalize(&source).unwrap();
        let destination = Path::new("/run/ryeos/verified-code/entry.py");
        let mut args = vec![format!("file://{},next", alias.display())];
        let mut envs = Vec::new();

        let error = rewrite_verified_code_references(
            &mut args,
            &mut envs,
            &source,
            &canonical_source,
            destination,
        )
        .unwrap_err();

        assert!(error.to_string().contains("cannot be rewritten safely"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn disabled_runtime_captures_backend_only_for_mandatory_profile() {
        let app_root = tempfile::tempdir().unwrap();
        write_policy(
            app_root.path(),
            &policy_yaml("disabled", Path::new("/usr/bin/bwrap")),
        );

        let runtime = SandboxRuntime::load(app_root.path()).unwrap();

        assert_eq!(runtime.mode(), SandboxMode::Disabled);
        assert!(!runtime.is_enforced());
        let expected_source = app_root.path().join(".ai/node/sandbox.yaml");
        assert_eq!(runtime.source(), Some(expected_source.as_path()));
        assert!(runtime.digest().unwrap().starts_with("sha256:"));
        assert!(runtime.inspection().backend.resolved_executable.is_none());
        assert!(runtime.inspection().backend.captured_digest.is_none());
        assert!(runtime.inspection().backend.captured_version.is_none());
        assert!(runtime.capture_mandatory_bubblewrap_backend().is_err());

        let tentative = runtime.tentative_mandatory_bubblewrap_backend().unwrap();
        assert!(tentative.inspection().backend.resolved_executable.is_some());
        assert!(tentative.inspection().backend.captured_digest.is_some());
        assert!(tentative.inspection().backend.captured_version.is_some());
        assert!(runtime.capture_mandatory_bubblewrap_backend().is_err());

        let contender = runtime.tentative_mandatory_bubblewrap_backend().unwrap();
        let contender_handle = contender.capture_mandatory_bubblewrap_backend().unwrap();
        let tentative_handle = tentative.capture_mandatory_bubblewrap_backend().unwrap();
        let (captured, reconciled) = runtime
            .publish_mandatory_bubblewrap_backend(&tentative)
            .unwrap();
        assert!(!reconciled);
        let captured_handle = captured.capture_mandatory_bubblewrap_backend().unwrap();
        let published_handle = runtime.capture_mandatory_bubblewrap_backend().unwrap();
        assert!(Arc::ptr_eq(&tentative_handle, &captured_handle));
        assert!(Arc::ptr_eq(&captured_handle, &published_handle));

        let (race_winner, reconciled) = runtime
            .publish_mandatory_bubblewrap_backend(&contender)
            .unwrap();
        assert!(reconciled);
        assert!(!Arc::ptr_eq(&contender_handle, &published_handle));
        assert!(Arc::ptr_eq(
            &published_handle,
            &race_winner.capture_mandatory_bubblewrap_backend().unwrap()
        ));

        let rebuilt = runtime.tentative_mandatory_bubblewrap_backend().unwrap();
        assert_eq!(rebuilt.inspection().backend, captured.inspection().backend);
        assert!(Arc::ptr_eq(
            &captured_handle,
            &rebuilt.capture_mandatory_bubblewrap_backend().unwrap()
        ));
    }

    #[test]
    fn disabled_apply_skips_confinement_but_applies_node_resource_bounds() {
        let runtime = SandboxRuntime::default();
        let project = tempfile::tempdir().unwrap();
        let original = request(project.path());
        let applied = runtime
            .apply(
                original,
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .unwrap();

        assert_eq!(applied.cmd, "/bin/sh");
        assert_eq!(applied.args, ["-c", "true"]);
        let limits = applied.limits.expect("disabled mode still bounds output");
        assert_eq!(
            limits.max_open_files,
            runtime.inspection().limits.open_files
        );
        assert_eq!(
            limits.max_stdout_bytes,
            Some(runtime.inspection().limits.stdout_bytes)
        );
        assert_eq!(
            limits.max_stderr_bytes,
            Some(runtime.inspection().limits.stderr_bytes)
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn enforced_runtime_resolves_backend_and_applies_limits() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_policy(
            app_root.path(),
            &policy_yaml("enforce", Path::new("/usr/bin/bwrap")),
        );
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();

        let applied = runtime
            .apply(
                request(project.path()),
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .unwrap();

        assert!(runtime.is_enforced());
        assert_eq!(
            runtime.inspection().backend.resolved_executable,
            Some(std::fs::canonicalize("/usr/bin/bwrap").unwrap())
        );
        assert!(runtime
            .inspection()
            .backend
            .captured_digest
            .as_deref()
            .is_some_and(|digest| digest.starts_with("sha256:")));
        assert!(runtime
            .inspection()
            .backend
            .captured_version
            .as_deref()
            .is_some_and(|version| !version.is_empty()));
        let bubblewrap_args = bubblewrap_args(&applied);
        assert!(bubblewrap_args.iter().any(|arg| arg == "--unshare-user"));
        assert!(bubblewrap_args.iter().any(|arg| arg == "--unshare-ipc"));
        assert!(bubblewrap_args.iter().any(|arg| arg == "--unshare-uts"));
        assert!(bubblewrap_args.iter().any(|arg| arg == "--unshare-net"));
        // Lillux already creates a new session for the retained Bubblewrap
        // wrapper. The target must inherit that stable wrapper-led process
        // group so descendants remain killable after the target PID exits.
        assert!(!bubblewrap_args.iter().any(|arg| arg == "--new-session"));
        assert!(bubblewrap_args.iter().any(|arg| arg == "--clearenv"));
        assert!(bubblewrap_args
            .windows(2)
            .any(|args| args[0] == "--json-status-fd" && args[1].parse::<i32>().is_ok()));
        assert!(bubblewrap_args
            .windows(3)
            .any(|args| args == ["--setenv", "PATH", "/usr/bin"]));
        assert!(applied.envs.is_empty());
        assert!(applied.supervised_status.is_some());
        assert_eq!(
            applied.limits,
            Some(lillux::SubprocessLimits {
                max_open_files: Some(128),
                max_stdout_bytes: Some(8_388_608),
                max_stderr_bytes: Some(8_388_608),
            })
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn enforced_runtime_keeps_target_secrets_out_of_the_host_command_line() {
        use std::os::fd::AsRawFd as _;

        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let policy = policy_yaml("enforce", Path::new("/usr/bin/bwrap"))
            .replace("allow: [\"PATH\"]", "allow: [\"PATH\", \"SECRET_TOKEN\"]");
        write_policy(app_root.path(), &policy);
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();
        let mut subprocess = request(project.path());
        subprocess.args.push("argument-secret".to_string());
        subprocess
            .envs
            .push(("SECRET_TOKEN".to_string(), "environment-secret".to_string()));

        let applied = runtime
            .apply(
                subprocess,
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .unwrap();

        assert_eq!(applied.args.first().map(String::as_str), Some("--args"));
        assert_eq!(applied.args.len(), 2);
        assert!(!applied.args.iter().any(|arg| arg.contains("secret")));
        let hidden_args = bubblewrap_args(&applied);
        assert!(hidden_args.iter().any(|arg| arg == "argument-secret"));
        assert!(hidden_args.iter().any(|arg| arg == "environment-secret"));

        let args_fd = applied.args[1].parse::<i32>().unwrap();
        let args_file = applied
            .inherited_fds
            .iter()
            .find(|file| file.as_raw_fd() == args_fd)
            .unwrap();
        let seals = unsafe { libc::fcntl(args_file.as_raw_fd(), libc::F_GET_SEALS) };
        let required =
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
        assert_eq!(seals & required, required);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn enforced_runtime_rejects_nul_in_a_target_argument() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_policy(
            app_root.path(),
            &policy_yaml("enforce", Path::new("/usr/bin/bwrap")),
        );
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();
        let mut subprocess = request(project.path());
        subprocess.args.push("bad\0argument".to_string());

        let error = runtime
            .apply(
                subprocess,
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .err()
            .expect("sandbox request with an interior NUL should be rejected");

        assert!(error.to_string().contains("interior NUL"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn enforced_runtime_keeps_stricter_existing_subprocess_limits() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_policy(
            app_root.path(),
            &policy_yaml("enforce", Path::new("/usr/bin/bwrap")),
        );
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();
        let mut request = request(project.path());
        request.limits = Some(lillux::SubprocessLimits {
            max_open_files: Some(64),
            max_stdout_bytes: Some(1_024),
            max_stderr_bytes: Some(2_048),
        });

        let applied = runtime
            .apply(
                request,
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .unwrap();

        assert_eq!(
            applied.limits,
            Some(lillux::SubprocessLimits {
                max_open_files: Some(64),
                max_stdout_bytes: Some(1_024),
                max_stderr_bytes: Some(2_048),
            })
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn enforced_runtime_pins_tmpdir_to_its_private_tmpfs() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let policy = policy_yaml("enforce", Path::new("/usr/bin/bwrap"))
            .replace("allow: [\"PATH\"]", "allow: [\"PATH\", \"TMPDIR\"]");
        write_policy(app_root.path(), &policy);
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();
        let mut subprocess = request(project.path());
        subprocess
            .envs
            .push(("TMPDIR".to_string(), "/host/tmp".to_string()));

        let applied = runtime
            .apply(
                subprocess,
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .unwrap();

        let bubblewrap_args = bubblewrap_args(&applied);
        let tmpdir_bindings = bubblewrap_args
            .windows(3)
            .filter(|args| args[0] == "--setenv" && args[1] == "TMPDIR")
            .collect::<Vec<_>>();
        assert_eq!(tmpdir_bindings.len(), 1);
        assert_eq!(tmpdir_bindings[0][2], "/tmp");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn readable_project_is_sufficient_to_make_the_working_directory_visible() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let policy = policy_yaml("enforce", Path::new("/usr/bin/bwrap"))
            .replace("readable: []", "readable: [\"{project}\"]")
            .replace("writable: [\"{project}\"]", "writable: []");
        write_policy(app_root.path(), &policy);
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();

        let applied = runtime
            .apply(
                request(project.path()),
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .unwrap();

        let project = std::fs::canonicalize(project.path()).unwrap();
        let bubblewrap_args = bubblewrap_args(&applied);
        assert!(bubblewrap_args.windows(3).any(|args| {
            args[0] == "--ro-bind-fd" && args[2] == project.to_string_lossy().as_ref()
        }));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn only_an_exact_daemon_runtime_workspace_may_overlap_the_app_root() {
        let app_root = tempfile::tempdir().unwrap();
        let execution_root = app_root.path().join(".ai/state/cache/executions");
        let workspace = execution_root.join("no-project-test");
        std::fs::create_dir_all(workspace.join(".ai")).unwrap();
        write_policy(
            app_root.path(),
            &policy_yaml("enforce", Path::new("/usr/bin/bwrap")),
        );
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();

        runtime
            .apply(
                request(&workspace),
                SandboxLaunchContext {
                    project_path: &workspace,
                    project_authority: SandboxProjectAuthority::RuntimeWorkspace,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .unwrap();

        let sibling = app_root.path().join(".ai/state/cache/not-an-execution");
        std::fs::create_dir_all(&sibling).unwrap();
        let error = runtime
            .apply(
                request(&sibling),
                SandboxLaunchContext {
                    project_path: &sibling,
                    project_authority: SandboxProjectAuthority::RuntimeWorkspace,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .err()
            .expect("runtime workspace outside the execution root should be rejected");
        assert!(error.to_string().contains("protected app root"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn node_public_identity_placeholder_mounts_only_the_public_document() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let public_identity = app_root
            .path()
            .join(".ai/node/identity/public-identity.json");
        std::fs::create_dir_all(public_identity.parent().unwrap()).unwrap();
        std::fs::write(&public_identity, b"{}").unwrap();
        let policy = policy_yaml("enforce", Path::new("/usr/bin/bwrap"))
            .replace("readable: []", "readable: [\"{node_public_identity}\"]");
        write_policy(app_root.path(), &policy);
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();

        let applied = runtime
            .apply(
                request(project.path()),
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .unwrap();

        let public_identity = std::fs::canonicalize(public_identity).unwrap();
        let public_identity = public_identity.to_string_lossy();
        let bubblewrap_args = bubblewrap_args(&applied);
        assert!(bubblewrap_args
            .windows(3)
            .any(|args| { args[0] == "--ro-bind-fd" && args[2] == public_identity.as_ref() }));
    }

    #[test]
    #[cfg(all(target_os = "linux", unix))]
    fn node_public_identity_placeholder_rejects_a_symlink() {
        use std::os::unix::fs::symlink;

        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let identity_dir = app_root.path().join(".ai/node/identity");
        std::fs::create_dir_all(&identity_dir).unwrap();
        let private_key = identity_dir.join("private_key.pem");
        std::fs::write(&private_key, b"private").unwrap();
        symlink(&private_key, identity_dir.join("public-identity.json")).unwrap();
        let policy = policy_yaml("enforce", Path::new("/usr/bin/bwrap"))
            .replace("readable: []", "readable: [\"{node_public_identity}\"]");
        write_policy(app_root.path(), &policy);
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();

        let error = match runtime.apply(
            request(project.path()),
            SandboxLaunchContext {
                project_path: project.path(),
                project_authority: SandboxProjectAuthority::External,
                state_root: None,
                checkpoint_dir: None,
                daemon_socket_path: None,
                bundle_roots: &[],
                node_trusted_keys_dir: None,
                verified_code: &[],
                item_ref: "tool:test/probe",
                thread_id: "T-test",
            },
        ) {
            Ok(_) => panic!("symlinked public identity must be refused"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("regular non-symlink file"));
    }

    #[test]
    #[cfg(all(target_os = "linux", unix))]
    fn daemon_socket_placeholder_mounts_only_the_typed_pinned_socket() {
        use std::os::unix::net::UnixListener;

        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let socket_root = tempfile::tempdir().unwrap();
        let socket = socket_root.path().join("ryeosd.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let policy = policy_yaml("enforce", Path::new("/usr/bin/bwrap"))
            .replace("readable: []", "readable: [\"{daemon_socket}\"]");
        write_policy(app_root.path(), &policy);
        let runtime = SandboxRuntime::load_for_daemon(app_root.path(), &socket).unwrap();

        let applied = runtime
            .apply(
                request(project.path()),
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: Some(&socket),
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .unwrap();

        let bubblewrap_args = bubblewrap_args(&applied);
        assert!(bubblewrap_args.windows(3).any(|args| {
            args[0] == "--ro-bind-fd" && args[2] == socket.to_string_lossy().as_ref()
        }));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn enforced_runtime_rejects_the_app_root_as_a_writable_project() {
        let app_root = tempfile::tempdir().unwrap();
        write_policy(
            app_root.path(),
            &policy_yaml("enforce", Path::new("/usr/bin/bwrap")),
        );
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();

        let error = runtime
            .apply(
                request(app_root.path()),
                SandboxLaunchContext {
                    project_path: app_root.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .err()
            .expect("writable app-root project should be rejected");

        assert!(error.to_string().contains("protected app root"));
    }

    #[test]
    #[cfg(unix)]
    fn policy_source_must_not_be_a_symlink() {
        use std::os::unix::fs::symlink;

        let app_root = tempfile::tempdir().unwrap();
        let policy_dir = app_root.path().join(".ai/node");
        std::fs::create_dir_all(&policy_dir).unwrap();
        let real_policy = policy_dir.join("real-sandbox.yaml");
        std::fs::write(
            &real_policy,
            policy_yaml("disabled", Path::new("/definitely/missing-bwrap")),
        )
        .unwrap();
        symlink(&real_policy, policy_dir.join("sandbox.yaml")).unwrap();

        let error = SandboxRuntime::load(app_root.path()).unwrap_err();

        assert!(error.to_string().contains("regular non-symlink file"));
    }

    #[test]
    fn strict_schema_rejects_unsupported_unknown_and_zero_limits() {
        let app_root = tempfile::tempdir().unwrap();
        let unsupported =
            policy_yaml("disabled", Path::new("/missing")).replacen("version: 1", "version: 99", 1);
        write_policy(app_root.path(), &unsupported);
        let error = SandboxRuntime::load(app_root.path()).unwrap_err();
        assert!(error.to_string().contains("expected 1"));

        let unknown = policy_yaml("disabled", Path::new("/missing"))
            .replace("  open_files: 128", "  open_files: 128\n  max_processes: 4");
        write_policy(app_root.path(), &unknown);
        let error = SandboxRuntime::load(app_root.path()).unwrap_err();
        assert!(error.to_string().contains("unknown field"));

        let zero_output = policy_yaml("disabled", Path::new("/missing"))
            .replace("  stdout_bytes: 8388608", "  stdout_bytes: 0");
        write_policy(app_root.path(), &zero_output);
        let error = SandboxRuntime::load(app_root.path()).unwrap_err();
        assert!(error.to_string().contains("stdout byte limit"));
    }

    #[test]
    fn namespace_destinations_reject_relative_and_parent_components() {
        validate_namespace_destination("test", Path::new("/safe/normal/path")).unwrap();

        for unsafe_path in ["relative/path", "/safe/../usr"] {
            let error = validate_namespace_destination("test", Path::new(unsafe_path)).unwrap_err();
            assert!(error.to_string().contains("normal path components"));
        }
    }

    #[test]
    fn policy_literal_with_parent_traversal_is_refused_while_disabled() {
        let app_root = tempfile::tempdir().unwrap();
        let policy = policy_yaml("disabled", Path::new("/definitely/missing-bwrap"))
            .replace("writable: [\"{project}\"]", "writable: [\"/safe/../usr\"]");
        write_policy(app_root.path(), &policy);

        let error = SandboxRuntime::load(app_root.path()).unwrap_err();

        assert!(error.to_string().contains("normal path components"));
    }

    #[test]
    #[cfg(all(target_os = "linux", unix))]
    fn project_destination_with_symlink_and_parent_traversal_is_refused() {
        use std::os::unix::fs::symlink;

        let app_root = tempfile::tempdir().unwrap();
        let paths = tempfile::tempdir().unwrap();
        let deep = paths.path().join("host/a/b");
        let canonical_project = paths.path().join("host/target");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(&canonical_project).unwrap();
        let alias = paths.path().join("alias");
        symlink(&deep, &alias).unwrap();
        let lexical_project = alias.join("../../target");
        assert_eq!(
            std::fs::canonicalize(&lexical_project).unwrap(),
            std::fs::canonicalize(&canonical_project).unwrap()
        );
        write_policy(
            app_root.path(),
            &policy_yaml("enforce", Path::new("/usr/bin/bwrap")),
        );
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();

        let error = runtime
            .apply(
                request(&lexical_project),
                SandboxLaunchContext {
                    project_path: &lexical_project,
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .err()
            .expect("non-normal namespace destination should be rejected");

        assert!(error.to_string().contains("normal path components"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn host_network_mode_does_not_create_a_network_namespace() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let policy = policy_yaml("enforce", Path::new("/usr/bin/bwrap"))
            .replace("mode: isolated", "mode: host");
        write_policy(app_root.path(), &policy);
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();

        let applied = runtime
            .apply(
                request(project.path()),
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .unwrap();

        assert!(!bubblewrap_args(&applied)
            .iter()
            .any(|arg| arg == "--unshare-net"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn verified_code_uses_an_exact_final_synthetic_overlay() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let source = project.path().join("entry.py");
        let content = b"print('verified')\n";
        std::fs::write(&source, content).unwrap();
        let policy = policy_yaml("enforce", Path::new("/usr/bin/bwrap"))
            .replace("readable: []", "readable: [\"{verified_code}\"]");
        write_policy(app_root.path(), &policy);
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();
        let verified = [SandboxVerifiedCode {
            source_path: source.clone(),
            content_hash: lillux::cas::sha256_hex(content),
        }];
        let mut subprocess = request(project.path());
        subprocess.args = vec![source.to_string_lossy().into_owned()];

        let applied = runtime
            .apply(
                subprocess,
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &verified,
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .unwrap();

        let canonical_project = std::fs::canonicalize(project.path()).unwrap();
        let authority_id =
            lillux::cas::sha256_hex(canonical_project.as_os_str().as_encoded_bytes());
        let namespace_root = PathBuf::from(VERIFIED_CODE_SANDBOX_ROOT).join(authority_id);
        let destination = namespace_root.join("entry.py");
        let destination_text = destination.to_string_lossy();
        let bubblewrap_args = bubblewrap_args(&applied);
        assert!(bubblewrap_args
            .iter()
            .any(|arg| arg == destination_text.as_ref()));

        let writable_index = bubblewrap_args
            .windows(3)
            .position(|args| {
                args[0] == "--bind-fd" && args[2] == project.path().to_string_lossy().as_ref()
            })
            .unwrap();
        let mirror_index = bubblewrap_args
            .windows(3)
            .position(|args| {
                args[0] == "--ro-bind-fd" && args[2] == namespace_root.to_string_lossy().as_ref()
            })
            .unwrap();
        let artifact_index = bubblewrap_args
            .windows(3)
            .position(|args| args[0] == "--ro-bind-fd" && args[2] == destination_text.as_ref())
            .unwrap();
        assert!(writable_index < mirror_index);
        assert!(mirror_index < artifact_index);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn verified_code_change_after_verification_is_refused() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let source = project.path().join("entry.py");
        std::fs::write(&source, b"before\n").unwrap();
        let verified = [SandboxVerifiedCode {
            source_path: source.clone(),
            content_hash: lillux::cas::sha256_hex(b"before\n"),
        }];
        std::fs::write(&source, b"after\n").unwrap();
        let policy = policy_yaml("enforce", Path::new("/usr/bin/bwrap"))
            .replace("readable: []", "readable: [\"{verified_code}\"]");
        write_policy(app_root.path(), &policy);
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();

        let error = runtime
            .apply(
                request(project.path()),
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &verified,
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .err()
            .expect("verified code changed after hashing should be rejected");

        assert!(error.to_string().contains("changed after verification"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn verified_code_requires_the_policy_surface() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let source = project.path().join("entry.py");
        let content = b"print('verified')\n";
        std::fs::write(&source, content).unwrap();
        write_policy(
            app_root.path(),
            &policy_yaml("enforce", Path::new("/usr/bin/bwrap")),
        );
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();
        let verified = [SandboxVerifiedCode {
            source_path: source,
            content_hash: lillux::cas::sha256_hex(content),
        }];

        let error = runtime
            .apply(
                request(project.path()),
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &verified,
                    item_ref: "tool:test/probe",
                    thread_id: "T-test",
                },
            )
            .err()
            .expect("verified code absent from readable policy should be rejected");

        assert!(error.to_string().contains("not pinned read-only"));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn thread_id_path_traversal_is_refused() {
        let app_root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_policy(
            app_root.path(),
            &policy_yaml("enforce", Path::new("/usr/bin/bwrap")),
        );
        let runtime = SandboxRuntime::load(app_root.path()).unwrap();

        let error = runtime
            .apply(
                request(project.path()),
                SandboxLaunchContext {
                    project_path: project.path(),
                    project_authority: SandboxProjectAuthority::External,
                    state_root: None,
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &[],
                    node_trusted_keys_dir: None,
                    verified_code: &[],
                    item_ref: "tool:test/probe",
                    thread_id: "../T-test",
                },
            )
            .err()
            .expect("invalid sandbox thread id should be rejected");

        assert!(error.to_string().contains("one normal path component"));
    }
}
