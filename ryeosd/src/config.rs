use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[derive(Debug, Parser)]
#[command(name = "ryeosd", about = "Rust control plane daemon for Rye OS")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<DaemonCommand>,

    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Override the system space directory (default: XDG data dir / ryeos)
    #[arg(long)]
    pub system_space_dir: Option<PathBuf>,

    #[arg(long)]
    pub bind: Option<SocketAddr>,

    #[arg(long)]
    pub db_path: Option<PathBuf>,

    #[arg(long)]
    pub uds_path: Option<PathBuf>,

    #[arg(long)]
    pub require_auth: bool,

    #[arg(long)]
    pub authorized_keys_dir: Option<PathBuf>,

    /// Run init with defaults before starting if not initialized
    #[arg(long)]
    pub init_if_missing: bool,

    /// Run bootstrap init only, then exit (no server)
    #[arg(long)]
    pub init_only: bool,

    /// Force regenerate the node signing key during init.
    /// Does NOT affect the user signing key.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, clap::Subcommand)]
pub enum DaemonCommand {
    /// Run a service handler in standalone mode (daemon must be stopped).
    RunService {
        /// Canonical service ref, e.g. service:system/status
        service_ref: String,

        /// JSON parameters for the service call
        #[arg(long)]
        params: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub bind: SocketAddr,
    pub db_path: PathBuf,
    pub uds_path: PathBuf,
    /// System space root — the single directory containing the `.ai/` tree.
    /// Holds node identity, vault, runtime DB, bundles, node-config, and
    /// all bundle content. Defaults to `~/.local/share/ryeos/`.
    /// Override with `--system-space-dir` or `RYE_SYSTEM_SPACE` env var.
    pub system_space_dir: PathBuf,
    /// Daemon-internal signing key.
    /// Defaults to `<system_space_dir>/.ai/node/identity/private_key.pem`.
    pub node_signing_key_path: PathBuf,
    /// Operator signing key — used for operator edits in project + user space.
    /// Defaults to `~/.ai/config/keys/signing/private_key.pem`.
    pub user_signing_key_path: PathBuf,
    pub require_auth: bool,
    pub authorized_keys_dir: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialConfig {
    bind: Option<SocketAddr>,
    db_path: Option<PathBuf>,
    uds_path: Option<PathBuf>,
    system_space_dir: Option<PathBuf>,
    node_signing_key_path: Option<PathBuf>,
    user_signing_key_path: Option<PathBuf>,
    require_auth: Option<bool>,
    authorized_keys_dir: Option<PathBuf>,
}

impl Config {
    pub fn load(cli: &Cli) -> Result<Self> {
        let compiled_default: SocketAddr = "127.0.0.1:7400".parse().unwrap();
        let defaults = Self::default_paths(compiled_default)?;
        let file_cfg = if let Some(path) = &cli.config {
            Some(Self::load_file(path)?)
        } else {
            let default_config = defaults.system_space_dir.join(".ai").join("node").join("config.yaml");
            if default_config.exists() {
                Some(Self::load_file(&default_config)
                    .with_context(|| format!("failed to load existing config at {}", default_config.display()))?)
            } else {
                None
            }
        };

        // R1: Typed --bind precedence. CLI `--bind` is Option<SocketAddr>;
        // None means the operator omitted it.
        let file_bind = file_cfg.as_ref().and_then(|cfg| cfg.bind);
        let resolved_bind = match (file_bind, cli.bind) {
            // Neither file nor CLI → compiled default
            (None, None) => compiled_default,
            // File only → use file value, no error
            (Some(fb), None) => fb,
            // CLI only → use CLI value (fresh-init or unconfigured-bind)
            (None, Some(cb)) => cb,
            // Both agree → use it
            (Some(fb), Some(cb)) if fb == cb => cb,
            // Both present but disagree — require --force
            (Some(fb), Some(cb)) => {
                if !cli.force {
                    bail!(
                        "conflict between CLI --bind ({cb}) and stored config.yaml ({fb}) — \
                         pass --force to overwrite"
                    );
                }
                // --force: use CLI value, caller (bootstrap::init) will
                // rewrite config.yaml so subsequent boots are consistent.
                cb
            }
        };

        // Resolve system_space_dir: CLI > env > config file > default
        let system_space_dir = cli
            .system_space_dir
            .clone()
            .or_else(|| env::var_os("RYE_SYSTEM_SPACE").map(PathBuf::from))
            .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.system_space_dir.clone()))
            .unwrap_or_else(|| defaults.system_space_dir.clone());

        let cfg = Self {
            bind: resolved_bind,
            db_path: cli
                .db_path
                .clone()
                .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.db_path.clone()))
                .unwrap_or_else(|| system_space_dir.join(".ai").join("state").join("runtime.sqlite3")),
            uds_path: cli
                .uds_path
                .clone()
                .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.uds_path.clone()))
                .unwrap_or_else(|| defaults.uds_path.clone()),
            system_space_dir: system_space_dir.clone(),
            node_signing_key_path: file_cfg
                .as_ref()
                .and_then(|cfg| cfg.node_signing_key_path.clone())
                .unwrap_or_else(|| {
                    system_space_dir.join(".ai").join("node").join("identity").join("private_key.pem")
                }),
            user_signing_key_path: file_cfg
                .as_ref()
                .and_then(|cfg| cfg.user_signing_key_path.clone())
                .or_else(|| env::var_os("RYE_SIGNING_KEY_PATH").map(PathBuf::from))
                .unwrap_or_else(|| defaults.user_signing_key_path.clone()),
            require_auth: cli.require_auth
                || file_cfg
                    .as_ref()
                    .and_then(|cfg| cfg.require_auth)
                    .unwrap_or(false),
            authorized_keys_dir: cli
                .authorized_keys_dir
                .clone()
                .or_else(|| {
                    file_cfg
                        .as_ref()
                        .and_then(|cfg| cfg.authorized_keys_dir.clone())
                })
                .unwrap_or_else(|| system_space_dir.join(".ai").join("node").join("auth").join("authorized_keys")),
        };

        // Only create minimal runtime directories (db parent, socket parent).
        // Bootstrap directories are created by bootstrap::init().
        if let Some(parent) = cfg.db_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create db parent {}", parent.display()))?;
        }
        if let Some(parent) = cfg.uds_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create uds parent {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("failed to set runtime dir permissions on {}", parent.display()))?;
            }
        }

        Ok(cfg)
    }

    fn load_file(path: &Path) -> Result<PartialConfig> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse config file {}", path.display()))
    }

    fn default_paths(bind: SocketAddr) -> Result<Self> {
        let base_dirs = BaseDirs::new().context("could not determine base directories")?;
        let system_space_dir = base_dirs
            .data_dir()
            .join("ryeos");

        let runtime_root = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| env::temp_dir().join(format!("ryeosd-{}", current_uid())));

        let home = base_dirs.home_dir();

        Ok(Self {
            bind,
            db_path: system_space_dir.join(".ai").join("state").join("runtime.sqlite3"),
            uds_path: runtime_root.join("ryeosd.sock"),
            system_space_dir: system_space_dir.clone(),
            node_signing_key_path: system_space_dir
                .join(".ai")
                .join("node")
                .join("identity")
                .join("private_key.pem"),
            user_signing_key_path: home
                .join(".ai")
                .join("config")
                .join("keys")
                .join("signing")
                .join("private_key.pem"),
            require_auth: false,
            authorized_keys_dir: system_space_dir.join(".ai").join("node").join("auth").join("authorized_keys"),
        })
    }
}
