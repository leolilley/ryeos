//! Shared RyeOS local node lifecycle and bootstrap semantics.

mod control;
pub mod init;
pub mod init_check;
pub mod lifecycle_marker;
pub mod lifecycle_wire;
pub mod metadata;
pub mod start;
pub mod status;
pub mod stop;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use init::{run_init, InitOptions, InitReport};
pub use init_check::{require_initialized, InitDiagnostics, InitState};
pub use lifecycle_wire::{
    LifecycleIdentity, LifecycleResponse, LifecycleWireState, StartupPhase, StartupSnapshot,
    LIFECYCLE_FRAME_MAX_BYTES, LIFECYCLE_PROTOCOL_VERSION,
};
pub use metadata::DaemonMetadata;
pub use start::{LifecycleStartLock, StartReport};
pub use status::{is_ready, LifecycleStatus, StaleDiagnostics};
pub use stop::{StopOptions, StopReport};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub app_root: PathBuf,
    pub bind: SocketAddr,
    pub uds_path: PathBuf,
}

impl NodeConfig {
    pub fn default_local() -> Result<Self> {
        let bind: SocketAddr = "127.0.0.1:7400".parse().expect("compiled bind parses");
        let app_root = std::env::var_os("RYEOS_APP_ROOT")
            .map(PathBuf::from)
            .or_else(|| dirs::data_dir().map(|d| d.join("ryeos")))
            .ok_or_else(|| anyhow::anyhow!("could not determine XDG data directory"))?;
        let runtime_root = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join(format!("ryeosd-{}", current_uid())));
        Ok(Self {
            app_root,
            bind,
            uds_path: runtime_root.join("ryeosd.sock"),
        })
    }

    pub fn load_local(app_root: Option<PathBuf>) -> Result<Self> {
        Self::load_local_with_overrides(app_root, None, None, false)
    }

    pub fn load_local_with_overrides(
        app_root: Option<PathBuf>,
        bind: Option<SocketAddr>,
        uds_path: Option<PathBuf>,
        force: bool,
    ) -> Result<Self> {
        let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
            app_root,
            bind,
            uds_path,
            force,
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

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
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

    pub fn load_with_overrides(
        app_root: Option<PathBuf>,
        bind: Option<SocketAddr>,
        uds_path: Option<PathBuf>,
        force: bool,
    ) -> Result<Self> {
        Ok(Self {
            config: NodeConfig::load_local_with_overrides(app_root, bind, uds_path, force)?,
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
        if let Some(meta) = hint {
            if let Some(uds) = &meta.uds_path {
                out.push(uds.clone());
            }
        }
        if !out.iter().any(|p| p == &self.config.uds_path) {
            out.push(self.config.uds_path.clone());
        }
        out
    }

    pub fn rpc_timeout(&self) -> Duration {
        Self::RPC_TIMEOUT
    }

    /// Acquire the (flock-based) start lock guarding concurrent
    /// `ryeos start` invocations. Self-clearing on process death.
    pub fn try_acquire_start_lock(&self) -> std::io::Result<LifecycleStartLock> {
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
        status::status(&self.env).await
    }

    pub async fn start(&self) -> Result<StartReport> {
        // First startup after a recovery generation/schema epoch bump may build
        // a new selected projection instance from CAS/refs. The lifecycle
        // socket remains responsive and reports progress throughout.
        start::start(&self.env, Duration::from_secs(900)).await
    }

    pub async fn stop(&self, opts: StopOptions) -> Result<StopReport> {
        stop::stop(&self.env, opts).await
    }
}
