//! Node-owned subprocess isolation policy and its immutable runtime form.
//!
//! The policy has one fixed source: `<app-root>/.ai/node/isolation.yaml`.
//! [`IsolationRuntime::load`] reads, strictly parses, and resolves that policy
//! once. Launch paths then share the resolved runtime and call [`IsolationRuntime::apply`]
//! without reopening node configuration at the process boundary.

use std::collections::BTreeMap;
use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::canonical_ref::CanonicalRef;
use crate::error::EngineError;
use crate::trust::TrustStore;
use ryeos_isolation_protocol::{
    AdapterLaunchRequest, IsolationAdapterProtocolVersion, IsolationAuthority,
    IsolationAuthorityId, IsolationAuthorityPurpose, IsolationDeviceSurface, IsolationEnvironment,
    IsolationMount, IsolationMountAccess, IsolationNetwork, IsolationPath, IsolationPlan,
    IsolationTarget,
};

mod authority;
mod backend;
mod inspection;
mod policy;
mod provenance;

pub use authority::{IsolationLaunchContext, IsolationProjectAuthority, IsolationVerifiedCode};
pub use backend::ResolvedIsolationBackend;
pub use inspection::{IsolationBackendInspection, IsolationBackendStatus, IsolationInspection};
pub use policy::{
    IsolationEnvironmentPolicy, IsolationFilesystemPolicy, IsolationLimitsPolicy, IsolationMode,
    IsolationNetworkMode, IsolationNetworkPolicy, IsolationPolicy, ISOLATION_POLICY_RELATIVE_PATH,
    ISOLATION_POLICY_VERSION,
};
use provenance::redacted_plan_digest;
pub use provenance::{AppliedIsolationLaunch, IsolationLaunchProvenance};

const VERIFIED_CODE_ISOLATION_ROOT: &str = "/run/ryeos/verified-code";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IsolationRuntimeState {
    Disabled,
    Enforced,
}

/// Higher-level guard that binds a composed runtime to the exact registered
/// bundle generation from which it was built.
pub trait IsolationGenerationLifeline: Send + Sync {
    fn begin_operation(&self) -> Result<Box<dyn Send + Sync>, String>;
    fn ensure_current(&self) -> Result<(), String>;
}

/// A strictly parsed, immutable isolation snapshot shared by process launches.
#[derive(Clone)]
pub struct IsolationRuntime {
    inspection: IsolationInspection,
    state: IsolationRuntimeState,
    /// Canonical host path used for authority and overlap checks.
    app_root: Option<PathBuf>,
    /// Exact app-root directory inode captured while the policy was loaded.
    app_root_authority: Option<Arc<lillux::PinnedDirectory>>,
    /// Exact daemon-owned parent of runtime workspace project directories.
    runtime_workspaces: Option<Arc<lillux::PinnedDirectory>>,
    /// Node-configured spelling recreated inside the isolation namespace.
    app_root_destination: Option<PathBuf>,
    daemon_socket: Option<PinnedDaemonSocket>,
    verified_artifacts: Option<Arc<VerifiedArtifactStore>>,
    /// Exact daemon-lifetime backend capture used by enforced execution.
    /// Disabled snapshots always carry `None`.
    backend_capture: Option<Arc<ResolvedIsolationBackend>>,
    /// Optional higher-level generation guard retained by standalone
    /// composition roots. Daemon bootstrap owns its guard outside this value.
    _generation_lifeline: Option<Arc<dyn IsolationGenerationLifeline>>,
    generation_node_trust: Option<TrustStore>,
    generation_bundle_roots: Option<Vec<PathBuf>>,
}

impl std::fmt::Debug for IsolationRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IsolationRuntime")
            .field("inspection", &self.inspection)
            .field("state", &self.state)
            .field("app_root", &self.app_root)
            .field("has_backend_capture", &self.backend_capture.is_some())
            .field(
                "retains_registered_generation",
                &self._generation_lifeline.is_some(),
            )
            .finish_non_exhaustive()
    }
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
        limits: &IsolationLimitsPolicy,
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
                Some(writable_file) => {
                    // The creation descriptor is open for writing. Executing
                    // it through `/proc/self/fd` would fail with ETXTBSY, so
                    // close it and pin the published inode read-only.
                    drop(writable_file);
                    self.root
                        .open_regular(name.as_ref(), false)
                        .map_err(|error| {
                            refused(format!(
                                "verified artifact cannot be reopened read-only: {error}"
                            ))
                        })?
                        .ok_or_else(|| {
                            refused(format!(
                                "verified artifact {} disappeared after publication",
                                artifact.display()
                            ))
                        })?
                }
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

struct LoadedIsolationPolicy {
    policy: IsolationPolicy,
    source: PathBuf,
    digest: String,
    runtime_app_root: PathBuf,
    app_root_authority: lillux::PinnedDirectory,
}

impl IsolationRuntime {
    /// Load the node-owned policy from its fixed path and resolve its runtime.
    ///
    /// Missing files, malformed YAML, unknown fields, and unsupported versions
    /// are errors in both modes. Enforced mode validates and captures the
    /// configured backend before this value is returned. Disabled mode never
    /// resolves, validates, or probes the configured backend.
    pub fn load(app_root: &Path) -> Result<Self, EngineError> {
        Self::load_with_backend(app_root, None)
    }

    /// Securely read and fully validate the fixed node policy without
    /// resolving or executing its selected backend. Prospective composition
    /// uses this before selecting any privileged bundle artifact.
    pub fn load_policy(app_root: &Path) -> Result<IsolationPolicy, EngineError> {
        let loaded = load_policy_source(app_root)?;
        if loaded.policy.version != ISOLATION_POLICY_VERSION {
            return Err(refused(format!(
                "unsupported node isolation policy version {} (expected {})",
                loaded.policy.version, ISOLATION_POLICY_VERSION
            )));
        }
        validate_policy_semantics(&loaded.policy)?;
        if loaded.policy.mode == IsolationMode::Enforce {
            validate_enforced_limits(&loaded.policy.limits)?;
        }
        Ok(loaded.policy)
    }

    pub fn load_with_backend(
        app_root: &Path,
        backend: Option<Arc<ResolvedIsolationBackend>>,
    ) -> Result<Self, EngineError> {
        Self::load_inner(app_root, None, backend)
    }

    /// Load the daemon snapshot and retain the exact configured callback
    /// socket inode for every launch that is allowed callback IPC.
    pub fn load_for_daemon(
        app_root: &Path,
        daemon_socket: &Path,
        backend: Option<Arc<ResolvedIsolationBackend>>,
    ) -> Result<Self, EngineError> {
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
            .ok_or_else(|| {
                refused("daemon socket disappeared before isolation load".to_string())
            })?;
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
        Self::load_inner(app_root, Some(socket), backend)
    }

    fn load_inner(
        app_root: &Path,
        daemon_socket: Option<PinnedDaemonSocket>,
        backend: Option<Arc<ResolvedIsolationBackend>>,
    ) -> Result<Self, EngineError> {
        let loaded = load_policy_source(app_root)?;
        Self::resolve(
            loaded.policy,
            Some(loaded.source),
            Some(loaded.digest),
            Some(loaded.runtime_app_root),
            Some(Arc::new(loaded.app_root_authority)),
            Some(app_root.to_path_buf()),
            daemon_socket,
            backend,
        )
    }

    pub fn source(&self) -> Option<&Path> {
        self.inspection.source.as_deref()
    }

    pub fn version(&self) -> u32 {
        self.inspection.version
    }

    pub fn mode(&self) -> IsolationMode {
        self.inspection.mode
    }

    pub fn digest(&self) -> Option<&str> {
        self.inspection.digest.as_deref()
    }

    pub fn inspection(&self) -> &IsolationInspection {
        &self.inspection
    }

    pub fn retain_registered_generation(
        mut self,
        lifeline: Arc<dyn IsolationGenerationLifeline>,
        node_trust: TrustStore,
        bundle_roots: Vec<PathBuf>,
    ) -> Self {
        self._generation_lifeline = Some(lifeline);
        self.generation_node_trust = Some(node_trust);
        self.generation_bundle_roots = Some(bundle_roots);
        self
    }

    pub fn registered_generation_node_trust(&self) -> Option<&TrustStore> {
        self.generation_node_trust.as_ref()
    }

    pub fn registered_generation_bundle_roots(&self) -> Option<&[PathBuf]> {
        self.generation_bundle_roots.as_deref()
    }

    pub fn ensure_registered_generation_current(&self) -> Result<(), EngineError> {
        match &self._generation_lifeline {
            Some(lifeline) => {
                lifeline
                    .ensure_current()
                    .map_err(|reason| EngineError::IsolationPolicyRefused {
                        reason: format!("registered bundle generation changed: {reason}"),
                    })
            }
            None => Ok(()),
        }
    }

    pub fn begin_registered_generation_operation(
        &self,
    ) -> Result<Option<Box<dyn Send + Sync>>, EngineError> {
        self._generation_lifeline
            .as_ref()
            .map(|lifeline| {
                lifeline
                    .begin_operation()
                    .map_err(|reason| EngineError::IsolationPolicyRefused {
                        reason: format!("cannot guard registered bundle generation: {reason}"),
                    })
            })
            .transpose()
    }

    /// Load and resolve a policy for an inspection-only caller such as doctor.
    /// This shares the production parser and validator rather than maintaining
    /// a second diagnostic interpretation of the policy.
    pub fn inspect(app_root: &Path) -> Result<IsolationInspection, EngineError> {
        Self::load(app_root).map(|runtime| runtime.inspection)
    }

    pub fn is_enforced(&self) -> bool {
        self.state == IsolationRuntimeState::Enforced
    }

    /// Apply this immutable policy snapshot to one executable request.
    pub fn apply(
        &self,
        request: lillux::SubprocessRequest,
        context: IsolationLaunchContext<'_>,
    ) -> Result<lillux::SubprocessRequest, EngineError> {
        self.apply_with_provenance(request, context).map(|applied| {
            tracing::debug!(
                isolation = ?applied.provenance,
                "compiled isolation launch provenance"
            );
            applied.request
        })
    }

    pub fn apply_with_provenance(
        &self,
        request: lillux::SubprocessRequest,
        context: IsolationLaunchContext<'_>,
    ) -> Result<AppliedIsolationLaunch, EngineError> {
        self.ensure_registered_generation_current()?;
        let applied = self.apply_with_provenance_current(request, context)?;
        self.ensure_registered_generation_current()?;
        Ok(applied)
    }

    fn apply_with_provenance_current(
        &self,
        request: lillux::SubprocessRequest,
        context: IsolationLaunchContext<'_>,
    ) -> Result<AppliedIsolationLaunch, EngineError> {
        if !request.timeout.is_finite() || request.timeout < 0.0 {
            return Err(refused(format!(
                "invalid subprocess timeout {}",
                request.timeout
            )));
        }
        if self.state == IsolationRuntimeState::Disabled {
            // Opt-out disables OS confinement, not daemon-memory
            // safety. Retained output remains bounded by the immutable node
            // policy, with any lower caller limit preserved.
            let mut request = request;
            let requested = request.limits.unwrap_or_default();
            request.limits = Some(lillux::SubprocessLimits {
                // `mode: disabled` is an OS-confinement opt-out. Preserve a
                // tighter limit already owned by the caller, but do not install
                // the isolation policy's RLIMIT_NOFILE until enforcement is on.
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
            return Ok(AppliedIsolationLaunch {
                request,
                provenance: self.launch_provenance(None),
            });
        }
        if !request.inherited_fds.is_empty() {
            return Err(refused(
                "enforced isolation launches cannot inherit caller-supplied file descriptors"
                    .to_string(),
            ));
        }
        if request.supervised_status.is_some() {
            return Err(refused(
                "enforced isolation launches cannot inherit caller-supplied process supervision"
                    .to_string(),
            ));
        }

        let _item_ref = CanonicalRef::parse(context.item_ref).map_err(|error| {
            refused(format!(
                "invalid isolation item reference `{}`: {error}",
                context.item_ref
            ))
        })?;
        // Retained in the launch context even though the adapter does not need it
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
            == IsolationProjectAuthority::RuntimeWorkspace
        {
            let workspaces = self
                .runtime_workspaces
                .as_deref()
                .expect("enforced isolation runtime has pinned workspace authority");
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
            .as_ref()
            .expect("enforced isolation runtime has a captured backend");
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
                .expect("enforced isolation runtime has an app root")
                .join("threads")
                .join(context.thread_id)
                .join("checkpoints");
            let app_root_authority = self
                .app_root_authority
                .as_deref()
                .expect("enforced isolation runtime has pinned app-root authority");
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
            if context.project_authority == IsolationProjectAuthority::ReadOnly {
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
        let command_path = if let Some(prepared) = verified_command {
            prepared.artifact.destination.clone()
        } else {
            let on_system_surface = is_lexically_on_system_runtime_surface(&lexical_command)
                && is_on_system_runtime_surface(&canonical_command);
            if on_system_surface {
                canonical_command.clone()
            } else {
                let is_runtime_workspace_project_code = context.project_authority
                    == IsolationProjectAuthority::RuntimeWorkspace
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
                command_path
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
                        "isolation launch requested daemon IPC without a daemon-pinned socket path"
                            .to_string(),
                    )
                })?;
                if requested != configured.destination {
                    return Err(refused(format!(
                        "isolation launch requested daemon socket {}, expected {}",
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
                        "daemon socket {} changed after isolation load",
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
                    "verified code {} is not pinned read-only by the node isolation policy",
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
                        "{kind} {} is not writable under the node isolation policy",
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
                    "daemon socket {} is not readable under the node isolation policy",
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
        let status = lillux::supervised_launcher_status_pipe().map_err(|reason| {
            refused(format!(
                "supervised launcher process tracking cannot be initialized: {reason}"
            ))
        })?;
        let status_fd = status.writer_fd();
        inherited_fds.push(status.writer);
        let mut authorities = Vec::new();
        let mut mounts = Vec::new();
        let mut authority_handles = Vec::new();
        let mut add_mount = |prefix: &str,
                             index: usize,
                             handle: Arc<std::fs::File>,
                             destination: &Path,
                             access: IsolationMountAccess,
                             purpose: IsolationAuthorityPurpose,
                             layer: u32|
         -> Result<IsolationAuthorityId, EngineError> {
            let id = IsolationAuthorityId::new(format!("{prefix}-{index}"))
                .map_err(|error| refused(error.to_string()))?;
            let inherited_fd = mount_fd_arg(&handle)
                .parse::<u32>()
                .map_err(|error| refused(format!("invalid authority descriptor: {error}")))?;
            authorities.push(IsolationAuthority {
                id: id.clone(),
                inherited_fd,
                purpose,
            });
            mounts.push(IsolationMount {
                source: id.clone(),
                destination: IsolationPath::new(destination.to_string_lossy().into_owned())
                    .map_err(|error| refused(error.to_string()))?,
                access,
                layer,
            });
            authority_handles.push(handle);
            Ok(id)
        };

        let mut system_readable_mounts = Vec::new();
        for path in ["/usr", "/bin", "/lib", "/lib64"] {
            let destination = PathBuf::from(path);
            if destination.exists() {
                let source = canonicalize_launch_path("system runtime mount", &destination)?;
                let source_handle = pin_mount_source("system runtime mount", &source)?;
                system_readable_mounts.push(ReadableMount {
                    source,
                    destination,
                    source_handle,
                });
            }
        }
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
                system_readable_mounts.push(ReadableMount {
                    source,
                    destination,
                    source_handle,
                });
            }
        }

        for (index, mount) in writable_mounts.iter().enumerate() {
            add_mount(
                "writable",
                index,
                mount.source_handle.clone(),
                &mount.destination,
                IsolationMountAccess::Writable,
                IsolationAuthorityPurpose::WritableMount,
                10,
            )?;
        }
        for (index, mount) in readable_mounts
            .iter()
            .filter(|mount| !verified_code_mounts.contains(mount))
            .enumerate()
        {
            add_mount(
                "readable",
                index,
                mount.source_handle.clone(),
                &mount.destination,
                IsolationMountAccess::ReadOnly,
                IsolationAuthorityPurpose::ReadOnlyMount,
                20,
            )?;
        }
        for (index, mount) in system_readable_mounts.iter().enumerate() {
            add_mount(
                "system",
                index,
                mount.source_handle.clone(),
                &mount.destination,
                IsolationMountAccess::ReadOnly,
                IsolationAuthorityPurpose::ReadOnlyMount,
                20,
            )?;
        }
        let mut target_authority = None;
        for (index, mount) in verified_code_mounts.iter().enumerate() {
            let is_target = mount.destination == command_path;
            let id = add_mount(
                "verified",
                index,
                mount.source_handle.clone(),
                &mount.destination,
                IsolationMountAccess::ReadOnly,
                if is_target {
                    IsolationAuthorityPurpose::Executable
                } else {
                    IsolationAuthorityPurpose::ReadOnlyMount
                },
                30,
            )?;
            if is_target {
                target_authority = Some(id);
            }
        }
        let target_authority = match target_authority {
            Some(id) => id,
            None => {
                let command_handle = pin_mount_source("target executable", &canonical_command)?;
                add_mount(
                    "target",
                    0,
                    command_handle,
                    &command_path,
                    IsolationMountAccess::ReadOnly,
                    IsolationAuthorityPurpose::Executable,
                    40,
                )?
            }
        };

        let mut environment = envs
            .into_iter()
            .filter(|(name, _)| name != "TMPDIR")
            .collect::<BTreeMap<_, _>>();
        environment.insert("TMPDIR".to_string(), "/tmp".to_string());
        let argv0 = command_argv0.unwrap_or_else(|| command_path.to_string_lossy().into_owned());
        let plan = IsolationPlan {
            target: IsolationTarget {
                executable: target_authority,
                argv0,
                arguments: args,
                cwd: IsolationPath::new(cwd_destination.to_string_lossy().into_owned())
                    .map_err(|error| refused(error.to_string()))?,
            },
            mounts,
            environment: IsolationEnvironment {
                values: environment,
            },
            network: match self.inspection.network.mode {
                IsolationNetworkMode::Host => IsolationNetwork::Host,
                IsolationNetworkMode::Isolated => IsolationNetwork::Isolated,
            },
            devices: IsolationDeviceSurface::Minimal,
            private_tmp: true,
            host_pid_namespace: true,
            shared_process_group: true,
        };
        let required_capabilities = plan
            .validate(&authorities)
            .map_err(|error| refused(format!("invalid compiled isolation plan: {error}")))?;
        if !required_capabilities.is_subset(&backend.effective_capabilities) {
            let missing = required_capabilities
                .difference(&backend.effective_capabilities)
                .map(|capability| format!("{capability:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(refused(format!(
                "selected isolation adapter is missing required capabilities: {missing}"
            )));
        }
        let plan_digest = redacted_plan_digest(&plan)?;
        let artifact_fds = backend
            .artifact_handles
            .iter()
            .map(|(role, handle)| {
                mount_fd_arg(handle)
                    .parse::<u32>()
                    .map(|fd| (*role, fd))
                    .map_err(|error| {
                        refused(format!("invalid isolation artifact descriptor: {error}"))
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let launch_request = AdapterLaunchRequest {
            protocol: IsolationAdapterProtocolVersion::V1,
            plan,
            authorities,
            artifacts: artifact_fds,
            status_fd: u32::try_from(status_fd)
                .map_err(|_| refused("invalid isolation status descriptor".to_string()))?,
        };
        launch_request
            .validate()
            .map_err(|error| refused(format!("invalid isolation launch request: {error}")))?;
        let request_bytes = serde_json::to_vec(&launch_request)
            .map_err(|error| refused(format!("serialize isolation request: {error}")))?;
        if request_bytes.len() > ryeos_isolation_protocol::MAX_REQUEST_BYTES {
            return Err(refused(format!(
                "isolation launch request exceeds {} bytes",
                ryeos_isolation_protocol::MAX_REQUEST_BYTES
            )));
        }
        let request_handle = lillux::sealed_memfd(c"ryeos-isolation-request", &request_bytes)
            .map_err(|error| refused(format!("seal isolation request: {error}")))?;
        let request_fd = mount_fd_arg(&request_handle);
        inherited_fds.extend(authority_handles);
        inherited_fds.extend(backend.artifact_handles.values().cloned());
        // The adapter descriptor is used only as the initial exec path. Keep
        // its parent handle alive through spawn but leave FD_CLOEXEC set so it
        // disappears in the adapter image and cannot reach its launcher or target.
        inherited_fds.push(request_handle);

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

        Ok(AppliedIsolationLaunch {
            request: lillux::SubprocessRequest {
                cmd: format!("/proc/self/fd/{}", mount_fd_arg(&backend.adapter_handle)),
                args: vec!["launch".to_string(), request_fd],
                cwd: Some(canonical_cwd.to_string_lossy().into_owned()),
                // The adapter receives only the sealed plan. It constructs the
                // target environment inside the selected isolation backend.
                envs: Vec::new(),
                stdin_data,
                timeout,
                limits,
                inherited_fds,
                supervised_status: Some(status.reader),
            },
            provenance: self.launch_provenance(Some(plan_digest)),
        })
    }

    fn launch_provenance(&self, plan_digest: Option<String>) -> IsolationLaunchProvenance {
        IsolationLaunchProvenance {
            policy_digest: self.inspection.digest.clone(),
            mode: self.inspection.mode,
            backend: self.inspection.backend.selection.clone(),
            backend_status: self.inspection.backend.status,
            bundle_manifest_digest: self.inspection.backend.bundle_manifest_digest.clone(),
            signer_fingerprint: self.inspection.backend.signer_fingerprint.clone(),
            adapter_digest: self.inspection.backend.adapter_digest.clone(),
            adapter_protocol: (self.state == IsolationRuntimeState::Enforced)
                .then_some(IsolationAdapterProtocolVersion::V1),
            payloads: self.inspection.backend.artifacts.clone(),
            effective_capabilities: self.inspection.backend.effective_capabilities.clone(),
            plan_digest,
        }
    }

    fn resolve(
        policy: IsolationPolicy,
        source: Option<PathBuf>,
        digest: Option<String>,
        app_root: Option<PathBuf>,
        app_root_authority: Option<Arc<lillux::PinnedDirectory>>,
        app_root_destination: Option<PathBuf>,
        daemon_socket: Option<PinnedDaemonSocket>,
        backend: Option<Arc<ResolvedIsolationBackend>>,
    ) -> Result<Self, EngineError> {
        if policy.version != ISOLATION_POLICY_VERSION {
            return Err(refused(format!(
                "unsupported node isolation policy version {} (expected {})",
                policy.version, ISOLATION_POLICY_VERSION
            )));
        }
        validate_policy_semantics(&policy)?;

        let state = match policy.mode {
            IsolationMode::Disabled => IsolationRuntimeState::Disabled,
            IsolationMode::Enforce => IsolationRuntimeState::Enforced,
        };
        if state == IsolationRuntimeState::Enforced {
            validate_enforced_limits(&policy.limits)?;
            if app_root.is_none() {
                return Err(refused(
                    "enforced isolation runtime requires an app root".to_string(),
                ));
            }
            let selection = policy.backend.as_ref().ok_or_else(|| {
                refused("enforced isolation requires an explicit backend selection".to_string())
            })?;
            let resolved = backend.as_ref().ok_or_else(|| {
                refused(format!(
                    "enforced isolation requires signed bundle `{}` implementation `{}`",
                    selection.bundle, selection.implementation
                ))
            })?;
            resolved.validate()?;
            if &resolved.selection != selection {
                return Err(refused(
                    "resolved isolation backend does not match node policy selection".to_string(),
                ));
            }
        }
        // Production snapshots retain the signed adapter and artifacts only when
        // enforcement is enabled. Disabled snapshots still retain the verified
        // artifact store for other execution-integrity duties, but never probe
        // or materialize the configured backend.
        let verified_artifacts = match app_root_authority.as_deref() {
            Some(app_root) => Some(Arc::new(VerifiedArtifactStore::create(
                app_root,
                &policy.limits,
            )?)),
            None => None,
        };
        let captured_backend = (state == IsolationRuntimeState::Enforced)
            .then(|| backend.expect("enforced backend was validated"));
        let runtime_workspaces = if state == IsolationRuntimeState::Enforced {
            let app_root = app_root_authority.as_deref().ok_or_else(|| {
                refused("enforced isolation runtime requires pinned app-root authority".to_string())
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
        let bundle_manifest_digest = captured_backend
            .as_ref()
            .map(|backend| backend.bundle_manifest_digest.clone());
        let signer_fingerprint = captured_backend
            .as_ref()
            .map(|backend| backend.signer_fingerprint.clone());
        let adapter_digest = captured_backend
            .as_ref()
            .map(|backend| backend.adapter_digest.clone());
        let adapter_build = captured_backend
            .as_ref()
            .map(|backend| backend.adapter_build.clone());
        let declared_capabilities = captured_backend
            .as_ref()
            .map(|backend| backend.declaration.capabilities.clone())
            .unwrap_or_default();
        let effective_capabilities = captured_backend
            .as_ref()
            .map(|backend| backend.effective_capabilities.clone())
            .unwrap_or_default();
        let inspected_artifacts = captured_backend
            .as_ref()
            .map(|backend| backend.inspected_artifacts.clone())
            .unwrap_or_default();
        Ok(Self {
            inspection: IsolationInspection {
                source,
                version: policy.version,
                mode: policy.mode,
                digest,
                backend: IsolationBackendInspection {
                    selection: policy.backend,
                    status: if state == IsolationRuntimeState::Enforced {
                        IsolationBackendStatus::Available
                    } else {
                        IsolationBackendStatus::Disabled
                    },
                    bundle_manifest_digest,
                    signer_fingerprint,
                    adapter_digest,
                    adapter_build,
                    declared_capabilities,
                    effective_capabilities,
                    artifacts: inspected_artifacts,
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
            backend_capture: captured_backend,
            _generation_lifeline: None,
            generation_node_trust: None,
            generation_bundle_roots: None,
        })
    }

    fn prepare_verified_code(
        &self,
        verified: &IsolationVerifiedCode,
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
            .expect("enforced isolation runtime has a verified artifact store");
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
            .expect("enforced isolation runtime has a verified artifact store");
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
            .expect("enforced isolation runtime has a verified artifact store");
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
/// Production composition must call [`IsolationRuntime::load`].
impl Default for IsolationRuntime {
    fn default() -> Self {
        Self::resolve(
            IsolationPolicy::default_disabled(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("compiled disabled isolation fixture policy is valid")
    }
}

fn validate_policy_semantics(policy: &IsolationPolicy) -> Result<(), EngineError> {
    if policy.mode == IsolationMode::Enforce && policy.backend.is_none() {
        return Err(refused(
            "enforced isolation requires an explicit backend selection".to_string(),
        ));
    }
    if let Some(backend) = &policy.backend {
        backend
            .validate()
            .map_err(|error| refused(error.to_string()))?;
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
                "isolation environment allow pattern `{allowed}` must be exact, `*`, or a prefix ending in one `*`"
            )));
        }
    }
    validate_artifact_limits(&policy.limits)?;
    Ok(())
}

fn validate_artifact_limits(policy: &IsolationLimitsPolicy) -> Result<(), EngineError> {
    if policy.stdout_bytes == 0 {
        return Err(refused(
            "isolation stdout byte limit must be greater than zero".to_string(),
        ));
    }
    if policy.stderr_bytes == 0 {
        return Err(refused(
            "isolation stderr byte limit must be greater than zero".to_string(),
        ));
    }
    if policy.verified_artifact_file_bytes == 0 {
        return Err(refused(
            "isolation verified-artifact per-file byte limit must be greater than zero".to_string(),
        ));
    }
    if policy.verified_artifact_total_bytes < policy.verified_artifact_file_bytes {
        return Err(refused(format!(
            "isolation verified-artifact total-byte limit {} is below per-file limit {}",
            policy.verified_artifact_total_bytes, policy.verified_artifact_file_bytes
        )));
    }
    if policy.verified_artifact_files == 0 {
        return Err(refused(
            "isolation verified-artifact file limit must be greater than zero".to_string(),
        ));
    }
    Ok(())
}

fn validate_enforced_limits(policy: &IsolationLimitsPolicy) -> Result<(), EngineError> {
    let limits = lillux::SubprocessLimits {
        max_open_files: policy.open_files,
        max_stdout_bytes: Some(policy.stdout_bytes),
        max_stderr_bytes: Some(policy.stderr_bytes),
    };
    lillux::validate_subprocess_limits(Some(&limits)).map_err(|reason| {
        refused(format!(
            "isolation subprocess limits are not enforceable: {reason}"
        ))
    })
}

fn environment_name_allowed(policy: &IsolationEnvironmentPolicy, name: &str) -> bool {
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
        let namespace_root = PathBuf::from(VERIFIED_CODE_ISOLATION_ROOT).join(authority_id);
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
    let namespace_root = PathBuf::from(VERIFIED_CODE_ISOLATION_ROOT).join(authority_id);
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
/// descriptor for the isolation adapter. Validation and mount execution therefore refer
/// to the same kernel object; a pathname swap after this point cannot redirect
/// the bind to a protected node path.
fn pin_mount_source(kind: &str, path: &Path) -> Result<Arc<std::fs::File>, EngineError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (kind, path);
        return Err(refused(
            "fd-pinned isolation mounts are supported only on Linux".to_string(),
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
        unreachable!("enforced isolation mounts are Linux-only")
    }
}

fn same_file_identity(left: &std::fs::File, right: &std::fs::File) -> Result<bool, EngineError> {
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        Err(refused(
            "isolation file-identity comparison is unavailable on this platform".to_string(),
        ))
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let left = left.metadata().map_err(|error| {
            refused(format!("isolation authority cannot be inspected: {error}"))
        })?;
        let right = right.metadata().map_err(|error| {
            refused(format!("isolation authority cannot be inspected: {error}"))
        })?;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
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
                    "isolation checkpoint source/destination mismatch".to_string(),
                ))
            }
        },
        other => {
            let destination = PathBuf::from(other);
            let source = std::fs::canonicalize(&destination).map_err(|error| {
                refused(format!(
                    "isolation path {} cannot be resolved: {error}",
                    destination.display()
                ))
            })?;
            (source, destination)
        }
    };
    if !destination.is_absolute() {
        return Err(refused(format!(
            "isolation path `{}` is not absolute",
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
                    "isolation runtime cannot resolve {node_public_identity} without an app root"
                        .to_string(),
                )
            })?
            .join(crate::AI_DIR)
            .join("node/identity/public-identity.json");
        let authority = app_root_authority.ok_or_else(|| {
            refused(
                "isolation runtime cannot resolve {node_public_identity} without pinned app-root authority"
                    .to_string(),
            )
        })?;
        let destination = app_root_destination
            .ok_or_else(|| {
                refused(
                    "isolation runtime cannot resolve {node_public_identity} destination without an app root"
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
                "isolation launch requested daemon IPC without a daemon-pinned socket path"
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
                "isolation launch requested daemon socket {}, expected {}",
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
                    "isolation readable path {} cannot be resolved: {error}",
                    destination.display()
                ))
            })?;
            (source, destination)
        }
    };
    if !destination.is_absolute() && !matches!(configured, "{project}" | "{cwd}") {
        return Err(refused(format!(
            "isolation path `{}` is not absolute",
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
            "isolation {kind} path must be absolute: {}",
            path.display()
        )));
    }
    canonicalize_launch_path(kind, path)
}

/// Require a path spelling that an isolation adapter cannot reinterpret through `.` or
/// `..` while constructing its new root. Host-side authority checks use
/// canonical sources, but namespace destinations deliberately retain their
/// configured spelling, so every destination must cross this lexical gate.
fn validate_namespace_destination(kind: &str, path: &Path) -> Result<(), EngineError> {
    use std::path::Component;

    let path_text = path.to_str().ok_or_else(|| {
        refused(format!(
            "isolation {kind} namespace destination is not valid UTF-8: {}",
            path.display()
        ))
    })?;
    if path_text.contains('\0') {
        return Err(refused(format!(
            "isolation {kind} namespace destination contains an interior NUL"
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
            "isolation {kind} namespace destination must contain only an absolute root followed by normal path components: {}",
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
            "isolation thread id must be one normal path component: `{thread_id}`"
        )));
    }
    Ok(())
}

fn validate_writable_mount(
    path: &Path,
    app_root: Option<&Path>,
    daemon_socket: Option<&Path>,
    canonical_project: &Path,
    project_authority: IsolationProjectAuthority,
    runtime_workspace_authorized: bool,
    mount_authority: WritableMountAuthority,
    canonical_checkpoint_dir: Option<&Path>,
) -> Result<(), EngineError> {
    if path == Path::new("/") {
        return Err(refused(
            "isolation writable path `/` would expose the entire host".to_string(),
        ));
    }

    if let Some(app_root) = app_root {
        let execution_root = app_root.join(crate::AI_DIR).join("state/cache/executions");
        let is_exact_runtime_workspace = project_authority
            == IsolationProjectAuthority::RuntimeWorkspace
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
                "isolation writable path {} overlaps protected app root {}",
                path.display(),
                app_root.display()
            )));
        }
    }
    if let Some(socket) = daemon_socket {
        if paths_overlap(path, socket) {
            return Err(refused(format!(
                "isolation writable path {} overlaps protected daemon socket {}",
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
                    "isolation writable path {} overlaps protected system root {}",
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
                    "isolation writable path {} contains protected home directory {}",
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

fn load_policy_source(app_root: &Path) -> Result<LoadedIsolationPolicy, EngineError> {
    validate_namespace_destination("app root", app_root)?;
    // Bind the policy bytes and all later authority checks to one canonical
    // app-root identity. The supplied spelling remains the namespace
    // destination, but it is never used as the host-side authority root.
    let runtime_app_root = canonicalize_context_mount("app root", app_root)?;
    let app_root_authority = lillux::PinnedDirectory::open(&runtime_app_root)
        .map_err(|error| refused(format!("app root cannot be pinned: {error}")))?
        .ok_or_else(|| {
            refused("app root disappeared while loading isolation policy".to_string())
        })?;
    let policy_parent = open_relative_directory(
        &app_root_authority,
        &[crate::AI_DIR, "node"],
        "isolation policy parent",
    )?;
    let source = runtime_app_root
        .join(crate::AI_DIR)
        .join(ISOLATION_POLICY_RELATIVE_PATH);
    let mut file = policy_parent
        .open_regular("isolation.yaml".as_ref(), false)
        .map_err(|error| refused(format!("node isolation policy cannot be opened: {error}")))?
        .ok_or_else(|| {
            refused(format!(
                "node isolation policy is required at {}",
                source.display()
            ))
        })?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|error| EngineError::IsolationPolicyRefused {
            reason: format!(
                "node isolation policy is required at {}: {error}",
                source.display()
            ),
        })?;
    let policy =
        serde_yaml::from_str(&raw).map_err(|error| EngineError::IsolationPolicyRefused {
            reason: format!(
                "invalid node isolation policy {}: {error}",
                source.display()
            ),
        })?;
    let digest = format!("sha256:{}", lillux::sha256_hex(raw.as_bytes()));
    let observed_app_root = lillux::PinnedDirectory::open(app_root)
        .map_err(|error| refused(format!("app root cannot be rechecked: {error}")))?
        .ok_or_else(|| {
            refused("app root disappeared while loading isolation policy".to_string())
        })?;
    if !app_root_authority
        .is_same_directory(&observed_app_root)
        .map_err(|error| refused(format!("app-root identity cannot be compared: {error}")))?
    {
        return Err(refused(format!(
            "app root {} changed while its isolation policy was being loaded",
            app_root.display()
        )));
    }
    Ok(LoadedIsolationPolicy {
        policy,
        source,
        digest,
        runtime_app_root,
        app_root_authority,
    })
}

fn refused(reason: String) -> EngineError {
    EngineError::IsolationPolicyRefused { reason }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_isolation_protocol::{
        InspectedArtifact, IsolationArtifactRole, IsolationBackendDeclaration,
        IsolationBackendSelection, IsolationCapability,
    };
    use std::collections::BTreeSet;

    #[cfg(unix)]
    fn resolved_backend() -> ResolvedIsolationBackend {
        let launcher = Arc::new(std::fs::File::open("/dev/null").unwrap());
        ResolvedIsolationBackend {
            selection: IsolationBackendSelection {
                bundle: "example-isolation-backend".to_string(),
                implementation: "example".to_string(),
            },
            declaration: IsolationBackendDeclaration {
                id: "example".to_string(),
                protocol: IsolationAdapterProtocolVersion::V1,
                targets: vec![
                    ryeos_isolation_protocol::IsolationTargetTriple::X86_64UnknownLinuxGnu,
                ],
                adapter: "adapter".to_string(),
                artifacts: BTreeMap::from([(
                    IsolationArtifactRole::Launcher,
                    "launcher".to_string(),
                )]),
                capabilities: BTreeSet::from([IsolationCapability::FilesystemPrivateRoot]),
            },
            bundle_manifest_digest: "a".repeat(64),
            signer_fingerprint: "b".repeat(64),
            adapter_digest: "d".repeat(64),
            adapter_handle: Arc::new(std::fs::File::open("/dev/null").unwrap()),
            artifact_handles: BTreeMap::from([(IsolationArtifactRole::Launcher, launcher)]),
            adapter_build: "0.1.0".to_string(),
            effective_capabilities: BTreeSet::from([IsolationCapability::FilesystemPrivateRoot]),
            inspected_artifacts: BTreeMap::from([(
                IsolationArtifactRole::Launcher,
                InspectedArtifact {
                    version: "example 1.0.0".to_string(),
                    digest: "c".repeat(64),
                },
            )]),
        }
    }

    fn write_policy(app_root: &Path, policy: &IsolationPolicy) {
        let policy_path = app_root
            .join(crate::AI_DIR)
            .join(ISOLATION_POLICY_RELATIVE_PATH);
        std::fs::create_dir_all(policy_path.parent().unwrap()).unwrap();
        std::fs::write(policy_path, serde_yaml::to_string(policy).unwrap()).unwrap();
    }

    #[test]
    fn disabled_runtime_retains_policy_identity_without_capturing_a_backend() {
        let app_root = tempfile::tempdir().unwrap();
        write_policy(app_root.path(), &IsolationPolicy::default_disabled());

        let runtime = IsolationRuntime::load(app_root.path()).unwrap();
        assert_eq!(runtime.mode(), IsolationMode::Disabled);
        assert!(!runtime.is_enforced());
        assert!(runtime.digest().unwrap().starts_with("sha256:"));
        assert_eq!(
            runtime.inspection().backend.status,
            IsolationBackendStatus::Disabled
        );
        assert!(runtime
            .inspection()
            .backend
            .bundle_manifest_digest
            .is_none());
        assert!(runtime.inspection().backend.signer_fingerprint.is_none());
        assert!(runtime.inspection().backend.adapter_digest.is_none());
        assert!(runtime
            .inspection()
            .backend
            .declared_capabilities
            .is_empty());
        assert!(runtime.inspection().backend.adapter_build.is_none());
        assert!(runtime.inspection().backend.artifacts.is_empty());
    }

    #[test]
    fn launch_plan_digest_redacts_arguments_and_environment_values() {
        let mut plan = IsolationPlan {
            target: IsolationTarget {
                executable: IsolationAuthorityId::new("target").unwrap(),
                argv0: "tool".to_string(),
                arguments: vec!["first-secret".to_string()],
                cwd: IsolationPath::new("/workspace").unwrap(),
            },
            mounts: Vec::new(),
            environment: IsolationEnvironment {
                values: BTreeMap::from([("API_TOKEN".to_string(), "first-token".to_string())]),
            },
            network: IsolationNetwork::Isolated,
            devices: IsolationDeviceSurface::Minimal,
            private_tmp: true,
            host_pid_namespace: true,
            shared_process_group: true,
        };
        let digest = redacted_plan_digest(&plan).unwrap();

        plan.target.arguments[0] = "second-secret".to_string();
        plan.environment
            .values
            .insert("API_TOKEN".to_string(), "second-token".to_string());
        assert_eq!(redacted_plan_digest(&plan).unwrap(), digest);

        plan.network = IsolationNetwork::Host;
        assert_ne!(redacted_plan_digest(&plan).unwrap(), digest);
    }

    #[cfg(unix)]
    #[test]
    fn resolved_backend_requires_exact_identity_capabilities_and_artifact_sets() {
        resolved_backend().validate().unwrap();

        let mut wrong_implementation = resolved_backend();
        wrong_implementation.selection.implementation = "other".to_string();
        assert!(wrong_implementation.validate().is_err());

        let mut narrowed_capabilities = resolved_backend();
        narrowed_capabilities.effective_capabilities.clear();
        narrowed_capabilities.validate().unwrap();

        let mut broadened_capabilities = resolved_backend();
        broadened_capabilities
            .effective_capabilities
            .insert(IsolationCapability::NetworkHost);
        assert!(broadened_capabilities
            .validate()
            .unwrap_err()
            .to_string()
            .contains("exceed its signed declaration"));

        let mut mismatched_artifacts = resolved_backend();
        mismatched_artifacts.inspected_artifacts.clear();
        assert!(mismatched_artifacts
            .validate()
            .unwrap_err()
            .to_string()
            .contains("artifact sets do not exactly match"));

        let mut invalid_digest = resolved_backend();
        invalid_digest
            .inspected_artifacts
            .get_mut(&IsolationArtifactRole::Launcher)
            .unwrap()
            .digest = "invalid".to_string();
        assert!(invalid_digest
            .validate()
            .unwrap_err()
            .to_string()
            .contains("lowercase SHA-256"));
    }

    #[test]
    fn enforced_runtime_requires_the_exact_selected_signed_backend() {
        let app_root = tempfile::tempdir().unwrap();
        let mut policy = IsolationPolicy::default_disabled();
        policy.mode = IsolationMode::Enforce;
        policy.backend = Some(resolved_backend().selection);
        write_policy(app_root.path(), &policy);

        let error = IsolationRuntime::load(app_root.path()).unwrap_err();
        assert!(error
            .to_string()
            .contains("requires signed bundle `example-isolation-backend`"));
    }

    #[test]
    fn policy_schema_and_semantic_limits_fail_closed_in_disabled_mode() {
        let app_root = tempfile::tempdir().unwrap();
        let policy_path = app_root
            .path()
            .join(crate::AI_DIR)
            .join(ISOLATION_POLICY_RELATIVE_PATH);
        std::fs::create_dir_all(policy_path.parent().unwrap()).unwrap();

        let mut unsupported = IsolationPolicy::default_disabled();
        unsupported.version = ISOLATION_POLICY_VERSION + 1;
        write_policy(app_root.path(), &unsupported);
        assert!(IsolationRuntime::load(app_root.path())
            .unwrap_err()
            .to_string()
            .contains("expected 1"));

        let mut unknown = serde_yaml::to_string(&IsolationPolicy::default_disabled()).unwrap();
        unknown.push_str("unknown_policy_field: true\n");
        std::fs::write(&policy_path, unknown).unwrap();
        assert!(IsolationRuntime::load(app_root.path())
            .unwrap_err()
            .to_string()
            .contains("unknown field"));

        let mut zero_output = IsolationPolicy::default_disabled();
        zero_output.limits.stdout_bytes = 0;
        write_policy(app_root.path(), &zero_output);
        assert!(IsolationRuntime::load(app_root.path())
            .unwrap_err()
            .to_string()
            .contains("stdout byte limit"));
    }

    #[cfg(unix)]
    #[test]
    fn policy_source_must_be_a_regular_non_symlink_file() {
        use std::os::unix::fs::symlink;

        let app_root = tempfile::tempdir().unwrap();
        let policy_path = app_root
            .path()
            .join(crate::AI_DIR)
            .join(ISOLATION_POLICY_RELATIVE_PATH);
        std::fs::create_dir_all(policy_path.parent().unwrap()).unwrap();
        let real_policy = policy_path.with_file_name("isolation-source.yaml");
        std::fs::write(
            &real_policy,
            serde_yaml::to_string(&IsolationPolicy::default_disabled()).unwrap(),
        )
        .unwrap();
        symlink(real_policy, policy_path).unwrap();

        let error = IsolationRuntime::load(app_root.path()).unwrap_err();
        assert!(matches!(&error, EngineError::IsolationPolicyRefused { .. }));
        assert!(error
            .to_string()
            .contains("node isolation policy cannot be opened"));
    }

    #[test]
    fn verified_path_rewrite_requires_unambiguous_token_boundaries() {
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
                "/project/entry.py.backup",
                "/project/entry.py",
                "/run/verified/entry.py"
            ),
            None
        );

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("entry.py");
        std::fs::write(&source, b"verified\n").unwrap();
        let canonical_source = std::fs::canonicalize(&source).unwrap();
        let mut arguments = vec![format!("{}.backup", source.display())];
        let mut environment = Vec::new();
        assert!(rewrite_verified_code_references(
            &mut arguments,
            &mut environment,
            &source,
            &canonical_source,
            Path::new("/run/verified/entry.py"),
        )
        .unwrap_err()
        .to_string()
        .contains("cannot be rewritten safely"));
    }

    #[test]
    fn namespace_and_thread_destinations_reject_ambiguous_paths() {
        validate_namespace_destination("test", Path::new("/safe/normal/path")).unwrap();
        for path in ["relative/path", "/safe/../usr"] {
            assert!(validate_namespace_destination("test", Path::new(path))
                .unwrap_err()
                .to_string()
                .contains("normal path components"));
        }
        for thread_id in ["", ".", "..", "../thread", "thread/child"] {
            assert!(
                validate_thread_path_component(thread_id).is_err(),
                "{thread_id}"
            );
        }
        validate_thread_path_component("T-valid_123").unwrap();
    }
}
