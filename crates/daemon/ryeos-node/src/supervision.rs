//! Local host-service association, not worker admission or node policy.
//!
//! Administrator-owned service data selects the privileged executable/account/
//! delegation. Node-writable policy is only the daemon's admission ceiling and
//! must never be used to discover privileged launch instructions here.
//!
//! The app root remains account-owned, including beneath a writable home.
//! Association checks detect replacement; they do not make that namespace
//! immutable against its owner. Containing untrusted workers belongs to their
//! admitted filesystem/process/client authorities, not root-owning user homes.

use std::ffi::OsStr;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use lillux::{PinnedDirectory, PinnedDirectoryIdentity, PinnedDirectoryLock};
use serde::{Deserialize, Serialize};

use crate::NodeConfig;

const DESCRIPTION: &str = "host.json";
const CONTROL: &str = "operator-intent";
const DESIRED: &str = "desired.json";
const UPGRADE: &str = "upgrade.json";
const LAUNCH_FAILURE: &str = "launch-failure.json";
const MAX_HOST_DOCUMENT_BYTES: u64 = 64 * 1024;
const MAX_HOST_LAUNCH_ERROR_BYTES: usize = 8 * 1024;
const HOST_SERVICE_BINDING_SCHEMA_VERSION: u32 = 2;
const HOST_LAUNCH_FAILURE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostServiceBinding {
    pub schema_version: u32,
    pub app_root: PathBuf,
    pub app_root_identity: PinnedDirectoryIdentity,
    pub node_fingerprint: String,
    pub account: lillux::ControllerAccount,
    pub daemon_executable: PathBuf,
    pub process_scopes: lillux::ProcessScopeConfiguration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradePhase {
    Installing,
    RestoreReady,
}

/// Host-local crash testimony. It grants no worker/session recovery authority.
/// Only the administrator may publish it; retries retain the original intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpgradeIntent {
    pub schema_version: u32,
    pub binding_digest: String,
    pub desired: DesiredState,
    pub phase: UpgradePhase,
    pub expected_daemon_sha256: String,
    /// Only the native host integration decodes its captured boot disposition.
    pub native_state: serde_json::Value,
}

/// Root-owned testimony for a host-service attempt that failed before the
/// selected node account could exec the daemon. It is diagnostic and
/// correlational only; it grants no process, lifecycle, or recovery authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostLaunchFailure {
    pub schema_version: u32,
    pub binding_digest: String,
    pub pid: u32,
    pub started_at: String,
    pub failed_at: String,
    pub error: String,
}

fn launch_failure_status(
    config: &NodeConfig,
    failure: HostLaunchFailure,
) -> crate::LifecycleStatus {
    crate::LifecycleStatus::Failed {
        metadata: crate::DaemonMetadata {
            pid: Some(failure.pid),
            bind: Some(config.bind.to_string()),
            uds_path: Some(config.uds_path.clone()),
            started_at: Some(failure.started_at.clone()),
            version: None,
            revision: None,
            build_date: None,
            app_root: config.app_root.clone(),
        },
        startup: crate::StartupSnapshot::failed_before_control(
            failure.started_at,
            failure.failed_at,
            failure.error,
        ),
    }
}

impl UpgradeIntent {
    fn require_target(&self, binding_digest: &str, expected_daemon_sha256: &str) -> Result<()> {
        if self.schema_version != 1
            || self.binding_digest != binding_digest
            || self.expected_daemon_sha256 != expected_daemon_sha256
            || !lillux::valid_hash(expected_daemon_sha256)
        {
            bail!("host upgrade belongs to a different association or package generation");
        }
        Ok(())
    }
}

pub struct InstalledService {
    pub binding: HostServiceBinding,
    directory: PinnedDirectory,
    app_root: PinnedDirectory,
    binding_digest: String,
    supervisor: Box<dyn lillux::HostServiceController>,
}

/// A deterministic local service filename, not a generated node identity.
/// The complete administrator-owned record must still prove the association.
fn service_name(app_root: &Path) -> Result<String> {
    if !app_root.is_absolute() {
        bail!("host service requires an exact absolute app root");
    }
    let mut normalized = PathBuf::new();
    for component in app_root.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                normalized.push(component.as_os_str())
            }
            _ => bail!("host service app root is not canonical"),
        }
    }
    if normalized.as_os_str() != app_root.as_os_str() || app_root.parent().is_none() {
        bail!("host service app root must be a normalized non-root path");
    }
    Ok(format!(
        "ryeos-{}",
        lillux::sha256_hex(&serde_json::to_vec(app_root)?)
    ))
}

/// Replace ordinary endpoint configuration while the exact node is stopped.
/// This is the unprivileged half of host setup: native service provisioning
/// remains a separate administrator operation and never parses this file.
pub fn configure_host_endpoints(
    app_root: &Path,
    bind: Option<SocketAddr>,
    uds_path: Option<PathBuf>,
) -> Result<NodeConfig> {
    if bind.is_none() && uds_path.is_none() {
        return NodeConfig::load_local(Some(app_root.to_path_buf()));
    }
    let _lifecycle = crate::LifecycleStartLock::try_acquire(app_root)?
        .context("another node lifecycle operation is active")?;
    if let Some(service) = InstalledService::discover_app_root(app_root)?
        && service.desired_state()? != DesiredState::Down
    {
        bail!("stop the supervised node before changing its configured endpoints");
    }
    configure_host_endpoints_while_locked(app_root, bind, uds_path, &_lifecycle)
}

/// Publish endpoint configuration while the caller retains the exact shared
/// lifecycle operation. The second state lock excludes both the daemon and
/// standalone state owners; neither lock substitutes for the other.
pub(crate) fn configure_host_endpoints_while_locked(
    app_root: &Path,
    bind: Option<SocketAddr>,
    uds_path: Option<PathBuf>,
    lifecycle: &crate::LifecycleStartLock,
) -> Result<NodeConfig> {
    lifecycle.ensure_protects_app_root(app_root)?;
    let state_lock = ryeos_app::state_lock::StateLock::acquire_with_timeout(
        &ryeos_app::state_lock::default_lock_path(app_root),
        lillux::time::Duration::from_secs(30),
    )
    .context("endpoint configuration requires a stopped node")?;
    let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        app_root: Some(app_root.to_path_buf()),
        bind,
        uds_path,
        force: true,
        ..Default::default()
    })
    .context("resolve replacement node endpoints")?;
    ryeos_app::config::replace_bootstrap_config(&config, &state_lock)?;
    Ok(NodeConfig::from_app_config(&config))
}

/// Administrator-only one-time host association. This consumes only explicit
/// host-maintenance inputs plus the public node identity; it intentionally
/// never loads account-owned node config or policy while privileged. The
/// selected service remains DOWN after publication.
pub fn provision_host_service(
    app_root: &Path,
    account: lillux::ControllerAccount,
) -> Result<DesiredState> {
    lillux::require_administrator()?;
    // The administrator boundary receives only the explicit app root. Retain
    // the same two locks as ordinary lifecycle/config mutation so a concurrent
    // start cannot select direct mode immediately before this association is
    // published. Root ownership is not a substitute for either node lock.
    let lifecycle = crate::LifecycleStartLock::try_acquire(app_root)?.context(
        "another node lifecycle operation is active; host setup requires a stopped node",
    )?;
    lifecycle.ensure_protects_app_root(app_root)?;
    let state_lock = ryeos_app::state_lock::StateLock::acquire_with_timeout(
        &ryeos_app::state_lock::default_lock_path(app_root),
        lillux::time::Duration::from_secs(5),
    )
    .context("host setup requires the selected node to be stopped")?;
    state_lock.ensure_protects_app_root(app_root)?;
    let association_name = service_name(app_root)?;
    account.validate().map_err(anyhow::Error::msg)?;
    let app_root = PinnedDirectory::open(app_root)?.context("host setup app root is absent")?;
    account.require_directory_owner(&app_root)?;
    let identity_path = Path::new(ryeos_engine::AI_DIR).join("node/identity/public-identity.json");
    let identity_file = app_root
        .open_pinned_regular_descendant(&identity_path, false)?
        .context("host setup requires an initialized node public identity")?;
    let observation = identity_file.observation()?;
    let identity: ryeos_app::identity::PublicIdentityDoc = serde_json::from_slice(
        &identity_file.read_stable_bounded(&observation, MAX_HOST_DOCUMENT_BYTES)?,
    )?;
    let node_fingerprint = identity.verified_fingerprint()?;

    let daemon_executable = std::env::current_exe().context("locate host setup daemon image")?;
    let daemon_parent = PinnedDirectory::open_owned_hierarchy(
        daemon_executable
            .parent()
            .context("host setup daemon image has no parent")?,
        0,
    )?
    .context("host setup daemon image directory is absent")?;
    let daemon = daemon_parent
        .open_pinned_regular(
            daemon_executable
                .file_name()
                .context("host setup daemon image has no filename")?,
            false,
        )?
        .context("host setup daemon image is absent")?;
    daemon.require_owner(0)?;
    daemon.require_executable()?;

    let process_scopes =
        lillux::ProcessScopeConfiguration::provision_host_delegation(&association_name)
            .map_err(anyhow::Error::msg)?;
    let binding = HostServiceBinding {
        schema_version: HOST_SERVICE_BINDING_SCHEMA_VERSION,
        app_root: app_root.path().to_path_buf(),
        app_root_identity: app_root.identity()?,
        node_fingerprint,
        account,
        daemon_executable: daemon.path().to_path_buf(),
        process_scopes,
    };
    let launch = expected_native_launch(&binding);
    lillux::provision_host_service(&association_name, &launch)?;
    let installation = lillux::discover_host_service(&association_name)?
        .context("published host service disappeared")?;
    publish_service_state(&installation.state_directory, &binding)?;
    InstalledService::from_installation(installation, app_root.path())?.desired_state()
}

/// Publish RyeOS's generic lifecycle state into the opaque state directory
/// returned by Lillux. The service manager never parses this record, and
/// RyeOS never names a native service-manager path.
fn publish_service_state(directory: &PinnedDirectory, binding: &HostServiceBinding) -> Result<()> {
    directory.require_owner(0)?;
    let expected = lillux::canonical_json(&serde_json::to_value(binding)?)?.into_bytes();
    match directory.open_pinned_regular(OsStr::new(DESCRIPTION), false)? {
        Some(existing) => {
            existing.require_owner(0)?;
            let current =
                existing.read_stable_bounded(&existing.observation()?, MAX_HOST_DOCUMENT_BYTES)?;
            if current != expected {
                bail!("existing host association conflicts with this exact application binding");
            }
        }
        None => directory.atomic_write_pinned_if_same(
            OsStr::new(DESCRIPTION),
            None,
            &expected,
            0o644,
        )?,
    }
    let control = directory.open_or_create_child(OsStr::new(CONTROL), 0o700)?;
    binding.account.grant_private_directory(&control)?;
    match control.open_pinned_regular(OsStr::new(DESIRED), false)? {
        Some(existing) => {
            binding.account.grant_private_file(&existing)?;
            let _: DesiredState = serde_json::from_slice(
                &existing.read_stable_bounded(&existing.observation()?, 1024)?,
            )
            .context("existing host desired-state record is invalid")?;
        }
        None => {
            control.atomic_write_pinned_if_same(
                OsStr::new(DESIRED),
                None,
                &serde_json::to_vec(&DesiredState::Down)?,
                0o600,
            )?;
            let desired = control
                .open_pinned_regular(OsStr::new(DESIRED), false)?
                .context("published host desired-state record disappeared")?;
            binding.account.grant_private_file(&desired)?;
        }
    }
    Ok(())
}

fn expected_native_launch(binding: &HostServiceBinding) -> lillux::HostServiceLaunch {
    lillux::HostServiceLaunch {
        schema_version: 1,
        executable: binding.daemon_executable.clone(),
        arguments: vec![
            "host-service".to_owned(),
            "--app-root".to_owned(),
            binding.app_root.to_string_lossy().into_owned(),
        ],
        // RyeOS bootstrap configuration is complete before host setup. The
        // node service therefore has no ambient HOME, PATH, XDG, or login-
        // session dependency. This remains explicit launch data at Lillux's
        // generic native-service boundary.
        environment: Default::default(),
        account: binding.account.clone(),
    }
}

fn read_document<T: for<'de> Deserialize<'de>>(
    directory: &PinnedDirectory,
    name: &str,
) -> Result<T> {
    let file = directory
        .open_pinned_regular(OsStr::new(name), false)?
        .with_context(|| format!("host service document {name} is absent"))?;
    let observation = file.observation()?;
    let bytes = file.read_stable_bounded(&observation, MAX_HOST_DOCUMENT_BYTES)?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid host service document {name}"))
}

/// Native service entry, not a workload-callable operation. Resolve only the
/// administrator-owned association before executing the existing Lillux
/// credential-drop boundary. Never call Config::load or initialize node state
/// in this privileged phase. The launch gate remains held through exec.
pub fn exec_host_service(app_root: &Path) -> Result<std::convert::Infallible> {
    let name = service_name(app_root)?;
    let installation = lillux::discover_host_service(&name)?
        .context("host service association is absent; refusing direct startup")?;
    let service = InstalledService::from_installation(installation, app_root)?;
    let started_at = lillux::time::iso8601_now();
    let _gate = service.gate()?;
    service.clear_launch_failure()?;
    let result = (|| {
        service.check_binding()?;
        let upgrade = service.upgrade_intent()?;
        require_launch_allowed(service.desired_state()?, upgrade.as_ref())?;
        let executable = service.installed_daemon()?;
        if let Some(intent) = upgrade {
            if executable.digest_stable_exact(&executable.observation()?)?
                != intent.expected_daemon_sha256
            {
                bail!("service launch image differs from the restoring installation generation");
            }
        }
        let root = service
            .binding
            .app_root
            .to_str()
            .context("host app root cannot be represented in daemon arguments")?;
        service
            .binding
            .process_scopes
            .exec_controller(
                &service.binding.account,
                &executable,
                &["--app-root".to_owned(), root.to_owned()],
                &service.app_root,
                &[],
            )
            .map_err(anyhow::Error::msg)
    })();
    match result {
        Ok(never) => match never {},
        Err(error) => {
            service
                .record_launch_failure(&started_at, &format!("{error:#}"))
                .context("record failed host-service launch")?;
            Err(error)
        }
    }
}

impl InstalledService {
    fn clear_launch_failure(&self) -> Result<()> {
        lillux::require_administrator()?;
        self.check_binding()?;
        if let Some(existing) = self
            .directory
            .open_pinned_regular(OsStr::new(LAUNCH_FAILURE), false)?
        {
            existing.require_owner(0)?;
            self.directory.remove_pinned_regular_if_same(&existing)?;
        }
        Ok(())
    }

    fn record_launch_failure(&self, started_at: &str, error: &str) -> Result<()> {
        lillux::require_administrator()?;
        self.check_binding()?;
        let mut end = error.len().min(MAX_HOST_LAUNCH_ERROR_BYTES);
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        let failure = HostLaunchFailure {
            schema_version: HOST_LAUNCH_FAILURE_SCHEMA_VERSION,
            binding_digest: self.binding_digest.clone(),
            pid: std::process::id(),
            started_at: started_at.to_owned(),
            failed_at: lillux::time::iso8601_now(),
            error: error[..end].trim().to_owned(),
        };
        let existing = self
            .directory
            .open_pinned_regular(OsStr::new(LAUNCH_FAILURE), false)?;
        if let Some(file) = &existing {
            file.require_owner(0)?;
        }
        self.directory.atomic_write_pinned_if_same(
            OsStr::new(LAUNCH_FAILURE),
            existing.as_ref(),
            &serde_json::to_vec(&failure)?,
            0o644,
        )
    }

    pub fn launch_failure(&self) -> Result<Option<HostLaunchFailure>> {
        let Some(file) = self
            .directory
            .open_pinned_regular(OsStr::new(LAUNCH_FAILURE), false)?
        else {
            return Ok(None);
        };
        file.require_owner(0)?;
        let failure: HostLaunchFailure = serde_json::from_slice(
            &file.read_stable_bounded(&file.observation()?, MAX_HOST_DOCUMENT_BYTES)?,
        )?;
        if failure.schema_version != HOST_LAUNCH_FAILURE_SCHEMA_VERSION
            || failure.binding_digest != self.binding_digest
            || failure.pid <= 1
            || failure.started_at.trim().is_empty()
            || failure.failed_at.trim().is_empty()
            || failure.error.trim().is_empty()
            || failure.error.len() > MAX_HOST_LAUNCH_ERROR_BYTES
        {
            bail!("host launch failure belongs to another association or contract epoch");
        }
        Ok(Some(failure))
    }

    pub fn launch_failure_status(
        &self,
        config: &NodeConfig,
    ) -> Result<Option<crate::LifecycleStatus>> {
        let Some(failure) = self.launch_failure()? else {
            return Ok(None);
        };
        Ok(Some(launch_failure_status(config, failure)))
    }

    /// Unprivileged startup corroboration using the identity actually loaded
    /// by the daemon, not merely its public envelope inspected by the launcher.
    /// Call before recovery or readiness; the caller separately proves its
    /// existing StateLock still protects this exact app root.
    pub fn verify_loaded_node_identity(
        &self,
        identity: &ryeos_app::identity::NodeIdentity,
    ) -> Result<()> {
        self.binding.account.require_current_process()?;
        self.check_binding()?;
        if identity.fingerprint() != self.binding.node_fingerprint {
            bail!("loaded node signing identity differs from the host service association");
        }
        Ok(())
    }

    pub fn discover(config: &NodeConfig) -> Result<Option<Self>> {
        Self::discover_app_root(&config.app_root)
    }

    /// Host maintenance must locate the protected record without first loading
    /// account-owned daemon configuration in the administrator process.
    pub fn discover_app_root(app_root: &Path) -> Result<Option<Self>> {
        let name = service_name(app_root)?;
        let Some(installation) = lillux::discover_host_service(&name)? else {
            return Ok(None);
        };
        Self::from_installation(installation, app_root).map(Some)
    }

    fn from_installation(
        installation: lillux::HostServiceInstallation,
        expected_root: &Path,
    ) -> Result<Self> {
        let lillux::HostServiceInstallation {
            state_directory: directory,
            launch,
            controller: supervisor,
        } = installation;
        let description = directory
            .open_pinned_regular(OsStr::new(DESCRIPTION), false)?
            .context("configured service has no host association; refusing direct fallback")?;
        description.require_owner(0)?;
        let observation = description.observation()?;
        let raw = description.read_stable_bounded(&observation, MAX_HOST_DOCUMENT_BYTES)?;
        let binding: HostServiceBinding = serde_json::from_slice(&raw)?;
        if binding.schema_version != HOST_SERVICE_BINDING_SCHEMA_VERSION
            || binding.app_root != expected_root
        {
            bail!("host service has a wrong epoch or app root");
        }
        binding.account.validate().map_err(anyhow::Error::msg)?;
        if launch != expected_native_launch(&binding) {
            bail!("native host service launch differs from its application association");
        }
        if !lillux::valid_hash(&binding.node_fingerprint) {
            bail!("host association has an invalid node public identity");
        }
        binding
            .process_scopes
            .validate()
            .map_err(anyhow::Error::msg)?;
        let app_root =
            PinnedDirectory::open(expected_root)?.context("associated app root is absent")?;
        binding.account.require_directory_owner(&app_root)?;
        if app_root.identity()? != binding.app_root_identity {
            bail!("host-associated app root has been replaced");
        }
        let identity_path =
            Path::new(ryeos_engine::AI_DIR).join("node/identity/public-identity.json");
        let identity_file = app_root
            .open_pinned_regular_descendant(&identity_path, false)?
            .context("host-associated node public identity is absent")?;
        let observation = identity_file.observation()?;
        let identity: ryeos_app::identity::PublicIdentityDoc = serde_json::from_slice(
            &identity_file.read_stable_bounded(&observation, MAX_HOST_DOCUMENT_BYTES)?,
        )?;
        // Use the existing identity envelope. This still does not replace live
        // node authentication or prove which key a future process will open.
        if identity.verified_fingerprint()? != binding.node_fingerprint {
            bail!("host-associated node identity changed");
        }
        let binding_digest = lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(&binding)?)?.as_bytes(),
        );
        Ok(Self {
            binding,
            directory,
            app_root,
            binding_digest,
            supervisor,
        })
    }

    fn gate(&self) -> Result<PinnedDirectoryLock> {
        self.directory
            .lock_exclusive_with_timeout(lillux::time::Duration::from_secs(15))
    }

    fn check_binding(&self) -> Result<()> {
        self.directory.ensure_path_binding()?;
        self.app_root.ensure_path_binding()?;
        let file = self
            .directory
            .open_pinned_regular(OsStr::new(DESCRIPTION), false)?
            .context("host service association disappeared")?;
        file.require_owner(0)?;
        let current: HostServiceBinding = read_document(&self.directory, DESCRIPTION)?;
        let digest =
            lillux::sha256_hex(lillux::canonical_json(&serde_json::to_value(current)?)?.as_bytes());
        if digest != self.binding_digest {
            bail!("host service association changed during lifecycle operation");
        }
        Ok(())
    }

    fn control_directory(&self) -> Result<PinnedDirectory> {
        let directory = self
            .directory
            .open_child_directory(OsStr::new(CONTROL))?
            .context("configured service operator controls are missing")?;
        self.binding.account.require_directory_owner(&directory)?;
        Ok(directory)
    }

    pub fn desired_state(&self) -> Result<DesiredState> {
        read_document(&self.control_directory()?, DESIRED)
    }

    fn set_desired(&self, state: DesiredState) -> Result<()> {
        let directory = self.control_directory()?;
        let current = directory
            .open_pinned_regular(OsStr::new(DESIRED), false)?
            .context("configured service has no desired-state record")?;
        directory.atomic_write_pinned_if_same(
            OsStr::new(DESIRED),
            Some(&current),
            &serde_json::to_vec(&state)?,
            0o600,
        )
    }

    fn upgrade_intent(&self) -> Result<Option<UpgradeIntent>> {
        let Some(file) = self
            .directory
            .open_pinned_regular(OsStr::new(UPGRADE), false)?
        else {
            return Ok(None);
        };
        file.require_owner(0)?;
        let observation = file.observation()?;
        let intent: UpgradeIntent = serde_json::from_slice(
            &file.read_stable_bounded(&observation, MAX_HOST_DOCUMENT_BYTES)?,
        )?;
        intent.require_target(&self.binding_digest, &intent.expected_daemon_sha256)?;
        Ok(Some(intent))
    }

    fn write_upgrade_intent(&self, intent: &UpgradeIntent) -> Result<()> {
        let existing = self
            .directory
            .open_pinned_regular(OsStr::new(UPGRADE), false)?;
        if let Some(file) = &existing {
            file.require_owner(0)?;
        }
        // Root-owned and readable by the operator: this is public recovery
        // testimony, never credential material. Only the administrator can
        // replace the containing entry. A mode-0600 journal would prevent the
        // unprivileged lifecycle caller from enforcing installation inhibition.
        self.directory.atomic_write_pinned_if_same(
            OsStr::new(UPGRADE),
            existing.as_ref(),
            &serde_json::to_vec(intent)?,
            0o644,
        )
    }

    fn installed_daemon(&self) -> Result<lillux::PinnedRegularFile> {
        let executable = &self.binding.daemon_executable;
        let directory = PinnedDirectory::open_owned_hierarchy(
            executable
                .parent()
                .context("host daemon executable has no parent")?,
            0,
        )?
        .context("host daemon executable directory is absent")?;
        let file = directory
            .open_pinned_regular(
                executable
                    .file_name()
                    .context("host daemon executable has no filename")?,
                false,
            )?
            .context("host daemon executable is absent")?;
        file.require_owner(0)?;
        file.require_executable()?;
        Ok(file)
    }

    /// Administrator install boundary, called only after prospective package
    /// validation and BEFORE stopping the node. Filesystem ownership enforces
    /// journal publication privilege. Retrying never recaptures stopped state
    /// as the original intent or changes the package being installed.
    pub fn begin_upgrade(&self, expected_daemon_sha256: &str) -> Result<UpgradeIntent> {
        if !lillux::valid_hash(expected_daemon_sha256) {
            bail!("host upgrade requires an exact staged daemon digest");
        }
        let _gate = self.gate()?;
        self.check_binding()?;
        self.supervisor.check_available()?;
        if let Some(mut intent) = self.upgrade_intent()? {
            intent.require_target(&self.binding_digest, expected_daemon_sha256)?;
            // A same-package installer retry may have been interrupted after
            // restore-ready. Re-establish inhibition BEFORE it requests down
            // or replaces files, retaining the original boot disposition.
            if intent.phase == UpgradePhase::RestoreReady {
                intent.phase = UpgradePhase::Installing;
                self.write_upgrade_intent(&intent)?;
            }
            return Ok(intent);
        }
        let intent = UpgradeIntent {
            schema_version: 1,
            binding_digest: self.binding_digest.clone(),
            desired: self.desired_state()?,
            phase: UpgradePhase::Installing,
            expected_daemon_sha256: expected_daemon_sha256.to_owned(),
            native_state: self.supervisor.capture_upgrade_state()?,
        };
        self.write_upgrade_intent(&intent)?;
        Ok(intent)
    }

    /// Called after successful package/node validation. Restore native boot
    /// disposition while launch is still inhibited, then publish restore-ready.
    /// The installer must use the ordinary operator start/stop path to restore
    /// intent; do not replace an operator-owned record with a root-owned file.
    pub fn mark_upgrade_restore_ready(
        &self,
        expected_daemon_sha256: &str,
    ) -> Result<UpgradeIntent> {
        let _gate = self.gate()?;
        self.check_binding()?;
        let mut intent = self
            .upgrade_intent()?
            .context("no host installation to restore")?;
        intent.require_target(&self.binding_digest, expected_daemon_sha256)?;
        let daemon = self.installed_daemon()?;
        if daemon.digest_stable_exact(&daemon.observation()?)? != expected_daemon_sha256 {
            bail!("installed daemon does not match the staged host upgrade generation");
        }
        if intent.phase == UpgradePhase::Installing {
            self.supervisor
                .restore_upgrade_state(&intent.native_state)?;
            intent.phase = UpgradePhase::RestoreReady;
            self.write_upgrade_intent(&intent)?;
        }
        Ok(intent)
    }

    /// Required after stop and before package replacement. The durable
    /// installing phase continues excluding native restart after this short
    /// gate is released. No PID-name scan or fabricated worker settlement.
    pub fn require_upgrade_replacement_safe(&self, expected_daemon_sha256: &str) -> Result<()> {
        let _gate = self.gate()?;
        self.check_binding()?;
        let intent = self
            .upgrade_intent()?
            .context("package replacement requires durable installation inhibition")?;
        intent.require_target(&self.binding_digest, expected_daemon_sha256)?;
        if intent.phase != UpgradePhase::Installing {
            bail!("package replacement is forbidden after restoration has begun");
        }
        self.binding
            .process_scopes
            .require_controller_tree_empty(&self.binding.account)
            .map_err(anyhow::Error::msg)
    }

    /// Retire installation inhibition only after observing the restored state.
    /// The caller cannot supply a success flag or a claimed running digest.
    /// Up requires the authenticated live image; Down requires a stopped node
    /// and an empty entire delegated tree. All failure paths retain the journal.
    pub fn finish_upgrade(&self, expected_daemon_sha256: &str) -> Result<()> {
        let _gate = self.gate()?;
        self.check_binding()?;
        let intent = self
            .upgrade_intent()?
            .context("no host installation to finish")?;
        intent.require_target(&self.binding_digest, expected_daemon_sha256)?;
        if intent.phase != UpgradePhase::RestoreReady || self.desired_state()? != intent.desired {
            bail!("host installation has not restored its original desired state");
        }
        self.supervisor.check_available()?;
        let daemon = self.installed_daemon()?;
        let observation = daemon.observation()?;
        if daemon.digest_stable_exact(&observation)? != expected_daemon_sha256 {
            bail!("installed daemon changed before upgrade completion");
        }
        // The exact installed observer runs under the selected account. Never
        // read account-owned Config/init/metadata as root, and never accept an
        // operator-supplied completion flag in place of executing this observer.
        let executable = daemon.inherited_descriptor_authority()?;
        let root = self
            .binding
            .app_root
            .to_str()
            .context("host app root is not UTF-8")?;
        let result = lillux::exec::lib_run_as_account(
            lillux::SubprocessRequest {
                cmd: executable.path().to_string_lossy().into_owned(),
                argv0: Some("ryeosd".to_owned()),
                args: vec![
                    "host-upgrade".to_owned(),
                    "--app-root".to_owned(),
                    root.to_owned(),
                    "--expected-daemon-sha256".to_owned(),
                    expected_daemon_sha256.to_owned(),
                    "observe".to_owned(),
                ],
                cwd: None,
                envs: vec![],
                stdin_data: None,
                timeout: 15.0,
                limits: Some(lillux::SubprocessLimits {
                    max_stdout_bytes: Some(1024),
                    max_stderr_bytes: Some(16 * 1024),
                    ..Default::default()
                }),
                inherited_fds: vec![executable],
                inherited_fd_mappings: vec![],
                supervised_status: None,
            },
            &self.binding.account,
        );
        if !result.success {
            bail!(
                "unprivileged host-upgrade observation failed: {}",
                result.stderr
            );
        }
        if intent.desired == DesiredState::Down {
            self.binding
                .process_scopes
                .require_controller_tree_empty(&self.binding.account)
                .map_err(anyhow::Error::msg)?;
        }
        self.check_binding()?;
        if self.desired_state()? != intent.desired {
            bail!("host desired state changed during upgrade observation");
        }
        let journal = self
            .directory
            .open_pinned_regular(OsStr::new(UPGRADE), false)?
            .context("host upgrade journal disappeared before completion")?;
        journal.require_owner(0)?;
        self.directory.remove_pinned_regular_if_same(&journal)
    }

    /// Read-only half of finish, executed by the fixed installed image under
    /// the node account while the administrator parent retains the launch gate.
    /// It must not acquire that gate again or mutate the protected journal.
    pub async fn observe_upgrade(&self, expected_daemon_sha256: &str) -> Result<()> {
        self.binding.account.require_current_process()?;
        let env = crate::LocalLifecycleEnv::load(Some(self.binding.app_root.clone()))?;
        self.check_binding()?;
        let intent = self
            .upgrade_intent()?
            .context("no host installation to observe")?;
        intent.require_target(&self.binding_digest, expected_daemon_sha256)?;
        if intent.phase != UpgradePhase::RestoreReady || self.desired_state()? != intent.desired {
            bail!("host installation has not restored its original desired state");
        }
        let daemon = self.installed_daemon()?;
        let observation = daemon.observation()?;
        if daemon.digest_stable_exact(&observation)? != expected_daemon_sha256 {
            bail!("installed daemon changed before upgrade observation");
        }
        match intent.desired {
            DesiredState::Up => {
                let target = crate::stop::pin_live_daemon(&env).await?;
                let status = crate::status::status(&env).await?;
                let crate::LifecycleStatus::Running { metadata, .. } = status else {
                    bail!("restored daemon has not reached lifecycle readiness");
                };
                if metadata.pid != Some(target.pid())
                    || target.executable_digest_exact(observation.size())? != expected_daemon_sha256
                {
                    bail!("running daemon is not the exact restored installation generation");
                }
            }
            DesiredState::Down => {
                if !matches!(
                    crate::status::status(&env).await?,
                    crate::LifecycleStatus::Stopped { .. }
                ) {
                    bail!("originally stopped host service is not conclusively stopped");
                }
            }
        }
        self.check_binding()
    }

    pub fn check_supervisor(&self) -> Result<()> {
        self.check_binding()?;
        self.supervisor.check_available()
    }

    pub fn check_start_allowed(&self) -> Result<()> {
        let _gate = self.gate()?;
        self.check_binding()?;
        require_launch_allowed(DesiredState::Up, self.upgrade_intent()?.as_ref())?;
        self.supervisor.check_available()
    }

    pub fn request_up(&self) -> Result<()> {
        let _gate = self.gate()?;
        self.check_binding()?;
        require_launch_allowed(DesiredState::Up, self.upgrade_intent()?.as_ref())?;
        self.supervisor.check_available()?;
        self.set_desired(DesiredState::Up)?;
        self.supervisor.request_up()
    }

    pub fn request_down(&self) -> Result<()> {
        let _gate = self.gate()?;
        self.check_binding()?;
        self.supervisor.check_available()?;
        self.set_desired(DesiredState::Down)?;
        self.supervisor.request_down()
    }
}

/// The service entry must check this under the same gate before scope
/// provisioning. A native up request cannot override installation inhibition;
/// desired state must never be inferred from daemon liveness.
fn require_launch_allowed(desired: DesiredState, upgrade: Option<&UpgradeIntent>) -> Result<()> {
    if desired != DesiredState::Up {
        bail!("host service desired state is down");
    }
    if let Some(intent) = upgrade {
        if intent.phase != UpgradePhase::RestoreReady || intent.desired != DesiredState::Up {
            bail!("host service startup is inhibited by an unfinished installation");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_launch_failure_maps_to_bounded_pre_control_status() {
        let config = NodeConfig {
            app_root: PathBuf::from("/node"),
            bind: "127.0.0.1:7400".parse().unwrap(),
            uds_path: PathBuf::from("/runtime/node.sock"),
        };
        let status = launch_failure_status(
            &config,
            HostLaunchFailure {
                schema_version: HOST_LAUNCH_FAILURE_SCHEMA_VERSION,
                binding_digest: "a".repeat(64),
                pid: 42,
                started_at: "2026-09-10T00:00:00Z".to_owned(),
                failed_at: "2026-09-10T00:00:01Z".to_owned(),
                error: "credential drop failed".to_owned(),
            },
        );
        let crate::LifecycleStatus::Failed { metadata, startup } = status else {
            panic!("expected failed host launch status")
        };
        assert_eq!(metadata.pid, Some(42));
        assert_eq!(metadata.app_root, config.app_root);
        assert_eq!(startup.error.as_deref(), Some("credential drop failed"));
    }

    #[test]
    fn endpoint_replacement_uses_the_exact_stopped_node_state_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("node");
        std::fs::create_dir_all(app_root.join(".ai/node")).unwrap();
        std::fs::create_dir_all(app_root.join(".ai/state")).unwrap();
        let lock = ryeos_app::state_lock::StateLock::acquire(
            &ryeos_app::state_lock::default_lock_path(&app_root),
        )
        .unwrap();
        let initial = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
            app_root: Some(app_root.clone()),
            uds_path: Some(tmp.path().join("first.sock")),
            ..Default::default()
        })
        .unwrap();
        ryeos_app::config::seed_bootstrap_config(&initial, &lock).unwrap();
        drop(lock);

        let lifecycle = crate::LifecycleStartLock::try_acquire(&app_root)
            .unwrap()
            .unwrap();
        let replacement_uds = tmp.path().join("second.sock");
        let replaced = configure_host_endpoints_while_locked(
            &app_root,
            Some("127.0.0.1:17400".parse().unwrap()),
            Some(replacement_uds),
            &lifecycle,
        )
        .unwrap();

        let persisted = NodeConfig::load_local(Some(app_root)).unwrap();
        assert_eq!(persisted.app_root, replaced.app_root);
        assert_eq!(persisted.bind, replaced.bind);
        assert_eq!(persisted.uds_path, replaced.uds_path);
    }

    #[test]
    fn endpoint_replacement_rejects_another_nodes_lifecycle_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let selected = tmp.path().join("selected");
        let other = tmp.path().join("other");
        for root in [&selected, &other] {
            std::fs::create_dir_all(root.join(ryeos_engine::AI_DIR).join("node")).unwrap();
            std::fs::create_dir_all(root.join(ryeos_engine::AI_DIR).join("state")).unwrap();
        }
        let other_lock = crate::LifecycleStartLock::try_acquire(&other)
            .unwrap()
            .unwrap();
        let error = configure_host_endpoints_while_locked(
            &selected,
            Some("127.0.0.1:17400".parse().unwrap()),
            Some(tmp.path().join("selected.sock")),
            &other_lock,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("another app root"));
        assert!(
            !selected
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("config.yaml")
                .exists()
        );
    }

    #[test]
    fn upgrade_retry_cannot_substitute_package_or_association() {
        let binding = "a".repeat(64);
        let image = "b".repeat(64);
        let intent = UpgradeIntent {
            schema_version: 1,
            binding_digest: binding.clone(),
            desired: DesiredState::Up,
            phase: UpgradePhase::Installing,
            expected_daemon_sha256: image.clone(),
            native_state: serde_json::json!({"initially_down": false}),
        };
        let restored: UpgradeIntent =
            serde_json::from_slice(&serde_json::to_vec(&intent).unwrap()).unwrap();
        restored.require_target(&binding, &image).unwrap();
        assert_eq!(restored.desired, DesiredState::Up);
        assert!(restored.require_target(&"c".repeat(64), &image).is_err());
        assert!(restored.require_target(&binding, &"d".repeat(64)).is_err());
        assert!(restored.require_target(&binding, "").is_err());
        let mut missing = serde_json::to_value(&intent).unwrap();
        missing.as_object_mut().unwrap().remove("native_state");
        assert!(serde_json::from_value::<UpgradeIntent>(missing).is_err());
    }

    #[test]
    fn launch_gate_never_treats_native_up_or_daemon_absence_as_installation_authority() {
        assert!(require_launch_allowed(DesiredState::Up, None).is_ok());
        assert!(require_launch_allowed(DesiredState::Down, None).is_err());
        for phase in [UpgradePhase::Installing, UpgradePhase::RestoreReady] {
            for original in [DesiredState::Up, DesiredState::Down] {
                let intent = UpgradeIntent {
                    schema_version: 1,
                    binding_digest: "a".repeat(64),
                    desired: original,
                    phase,
                    expected_daemon_sha256: "b".repeat(64),
                    native_state: serde_json::json!({}),
                };
                for desired in [DesiredState::Up, DesiredState::Down] {
                    assert_eq!(
                        require_launch_allowed(desired, Some(&intent)).is_ok(),
                        phase == UpgradePhase::RestoreReady
                            && original == DesiredState::Up
                            && desired == DesiredState::Up
                    );
                }
            }
        }
    }

    #[test]
    fn association_filename_is_exact_and_not_a_caller_selected_service() {
        assert_eq!(
            service_name(Path::new("/a/node")).unwrap(),
            service_name(Path::new("/a/node")).unwrap()
        );
        assert_ne!(
            service_name(Path::new("/a/node")).unwrap(),
            service_name(Path::new("/b/node")).unwrap()
        );
        for invalid in ["relative", "/", "/a/../b", "/a/./b", "/a//b", "/a/b/"] {
            assert!(service_name(Path::new(invalid)).is_err(), "{invalid}");
        }
    }
}
