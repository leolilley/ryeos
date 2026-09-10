//! Shared RyeOS local node lifecycle and bootstrap semantics.

mod control;
pub mod init;
pub mod init_check;
pub mod lifecycle_marker;
pub mod lifecycle_wire;
pub mod metadata;
pub mod model_setup;
pub mod start;
pub mod status;
pub mod stop;
pub mod supervision;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use init::{
    InitCompletionReport, InitOperatorCeremony, InitOperatorProfile, InitOptions, InitPhase,
    InitProgress, InitReport, load_trusted_init_node_profile, run_init,
    run_init_with_operator_ceremony, run_init_with_progress,
    seal_init_completion_after_policy_update, verify_init_completion,
};
pub use init_check::{InitDiagnostics, InitState, require_initialized};
pub use lifecycle_wire::{
    LIFECYCLE_FRAME_MAX_BYTES, LIFECYCLE_PROTOCOL_VERSION, LifecycleIdentity, LifecycleResponse,
    LifecycleWireState, StartupPhase, StartupSnapshot,
};
pub use metadata::DaemonMetadata;
pub use model_setup::{
    PersistModelRouteOptions, PersistModelRouteReport, persist_default_model_route,
};
pub use start::{LifecycleStartLock, StartEndpointConfiguration, StartReport};
pub use status::{LifecycleStatus, StaleDiagnostics, is_ready};
pub use stop::{StopOptions, StopReport};

/// Synchronous observer for local lifecycle transitions. Implementations must
/// return quickly: startup and shutdown publish their latest authoritative
/// status after each lifecycle probe on the command's own task.
pub trait LifecycleProgressObserver {
    fn observe(&mut self, status: &LifecycleStatus);
}

impl<F> LifecycleProgressObserver for F
where
    F: FnMut(&LifecycleStatus),
{
    fn observe(&mut self, status: &LifecycleStatus) {
        self(status);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub app_root: PathBuf,
    pub bind: SocketAddr,
    pub uds_path: PathBuf,
}

impl NodeConfig {
    pub fn default_local() -> Result<Self> {
        let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources::default())?;
        Ok(Self::from_app_config(&config))
    }

    pub fn load_local(app_root: Option<PathBuf>) -> Result<Self> {
        let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
            app_root,
            ..Default::default()
        })?;
        Ok(Self::from_app_config(&config))
    }

    pub fn from_app_config(config: &ryeos_app::config::Config) -> Self {
        Self {
            app_root: config.app_root.clone(),
            bind: config.bind,
            uds_path: config.uds_path.clone(),
        }
    }
}

/// Lightweight local-node lifecycle environment.
///
/// Centralizes the small policy decisions that lifecycle reads/mutations
/// share: side-effect-free local config loading, the ordered set of UDS
/// candidate paths to probe (daemon metadata hint first, then the
/// configured path), the bounded lifecycle RPC timeout, and start-lock
/// acquisition.
///
/// Lifecycle operations are local-node operations. `RYEOSD_URL` is
/// intentionally ignored here — that env var only steers normal
/// daemon-backed dispatch.
#[derive(Debug, Clone)]
pub struct LocalLifecycleEnv {
    config: NodeConfig,
}

impl LocalLifecycleEnv {
    /// Bounded timeout for a single lifecycle RPC round-trip
    /// (connect + write + read + decode).
    pub const RPC_TIMEOUT: Duration = Duration::from_millis(750);

    /// Build the env from a side-effect-free `Config::load`.
    pub fn load(app_root: Option<PathBuf>) -> Result<Self> {
        Ok(Self {
            config: NodeConfig::load_local(app_root)?,
        })
    }

    /// Resolve only the selected node root, without requiring the complete
    /// bootstrap document to decode. This supports bounded diagnostics and
    /// supervised Down intent after a configuration failure; it never supplies
    /// substitute endpoints or direct-launch authority.
    pub fn selected_app_root(app_root: Option<PathBuf>) -> Result<PathBuf> {
        ryeos_app::config::Config::selected_app_root(&ryeos_app::config::ConfigSources {
            app_root,
            ..Default::default()
        })
    }

    pub fn from_config(config: NodeConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    /// Best-effort read of `daemon.json`. Returns `None` when the file
    /// is missing, unreadable, or malformed — callers treat it as a
    /// hint, never as truth.
    pub fn read_metadata_hint(&self) -> Option<DaemonMetadata> {
        match DaemonMetadata::read(&self.config.app_root) {
            Ok(Some(meta)) => Some(meta),
            Ok(None) => None,
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    "daemon.json present but unreadable; treating as no hint"
                );
                None
            }
        }
    }

    /// Liveness probe UDS candidates in priority order.
    ///
    /// `daemon.json` is only a hint; we try its `uds_path` first, then
    /// the configured `uds_path`. Duplicates are removed while
    /// preserving order.
    pub fn uds_candidates(&self) -> Vec<PathBuf> {
        self.uds_candidates_from_hint(self.read_metadata_hint().as_ref())
    }

    /// Expand a (pre-read) metadata hint into the ordered candidate
    /// set. Lets callers that already read `daemon.json` once avoid a
    /// second read.
    pub fn uds_candidates_from_hint(&self, hint: Option<&DaemonMetadata>) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::with_capacity(2);
        if let Some(meta) = hint
            && let Some(uds) = &meta.uds_path
        {
            out.push(uds.clone());
        }
        if !out.iter().any(|p| p == &self.config.uds_path) {
            out.push(self.config.uds_path.clone());
        }
        out
    }

    pub fn rpc_timeout(&self) -> Duration {
        Self::RPC_TIMEOUT
    }

    /// Acquire the Lillux-pinned lifecycle lock guarding concurrent start and
    /// stop operations. The returned `None` is ordinary contention; the lease
    /// is self-clearing on process death.
    pub fn try_acquire_start_lock(&self) -> Result<Option<LifecycleStartLock>> {
        LifecycleStartLock::try_acquire(&self.config.app_root)
    }
}

#[derive(Debug, Clone)]
pub struct LifecycleController {
    env: LocalLifecycleEnv,
}

impl LifecycleController {
    pub fn new(config: NodeConfig) -> Self {
        Self {
            env: LocalLifecycleEnv::from_config(config),
        }
    }

    pub fn from_env(env: LocalLifecycleEnv) -> Self {
        Self { env }
    }

    pub fn config(&self) -> &NodeConfig {
        self.env.config()
    }

    pub fn env(&self) -> &LocalLifecycleEnv {
        &self.env
    }

    pub fn init(&self, opts: InitOptions) -> Result<InitReport> {
        init::run_init(&opts)
    }

    pub fn init_state(&self) -> Result<InitState> {
        init_check::init_state(&self.env.config().app_root)
    }

    pub fn require_initialized(&self) -> Result<()> {
        init_check::require_initialized(&self.env.config().app_root)
    }

    pub async fn status(&self) -> Result<LifecycleStatus> {
        // One native-manager check per explicit status operation, not on every
        // daemon-readiness poll. A configured but missing supervisor is never
        // reported as an ordinary stopped/direct node.
        let service = supervision::InstalledService::discover(self.config())?;
        if let Some(service) = &service {
            service.check_supervisor()?;
        }
        let status = status::status(&self.env).await?;
        if matches!(status, LifecycleStatus::Stopped { .. })
            && let Some(service) = &service
            && service.desired_state()? == supervision::DesiredState::Up
            && let Some(failed) = service.launch_failure_status(self.config())?
        {
            return Ok(failed);
        }
        Ok(status)
    }

    pub async fn start(&self) -> Result<StartReport> {
        // First startup after a recovery generation/schema epoch bump may build
        // a new selected projection instance from CAS/refs. The lifecycle
        // socket remains responsive and reports progress throughout.
        start::start(&self.env, Duration::from_secs(900)).await
    }

    /// Start the node while publishing every observed lifecycle state to a
    /// caller-owned presentation surface.
    pub async fn start_with_progress(
        &self,
        observer: &mut dyn LifecycleProgressObserver,
    ) -> Result<StartReport> {
        start::start_with_progress(&self.env, Duration::from_secs(900), Some(observer)).await
    }

    /// Start with optional persisted endpoint selection. Differing values are
    /// accepted only for a stopped node and are committed under the same
    /// lifecycle operation that requests launch.
    pub async fn start_with_endpoint_configuration(
        &self,
        endpoints: StartEndpointConfiguration,
        observer: Option<&mut dyn LifecycleProgressObserver>,
    ) -> Result<StartReport> {
        start::start_with_endpoint_configuration(
            &self.env,
            Duration::from_secs(900),
            endpoints,
            observer,
        )
        .await
    }

    pub async fn stop(&self, opts: StopOptions) -> Result<StopReport> {
        stop::stop(&self.env, opts).await
    }

    /// Stop the node while publishing every observed lifecycle state to a
    /// caller-owned presentation surface.
    pub async fn stop_with_progress(
        &self,
        opts: StopOptions,
        observer: &mut dyn LifecycleProgressObserver,
    ) -> Result<StopReport> {
        stop::stop_with_progress(&self.env, opts, Some(observer)).await
    }
}
