use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
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
    #[arg(long)]
    pub config: Option<PathBuf>,

    #[arg(long, default_value = "127.0.0.1:7400")]
    pub bind: SocketAddr,

    #[arg(long)]
    pub db_path: Option<PathBuf>,

    #[arg(long)]
    pub uds_path: Option<PathBuf>,

    #[arg(long)]
    pub cas_root: Option<PathBuf>,

    #[arg(long)]
    pub system_data_dir: Option<PathBuf>,

    #[arg(long)]
    pub require_auth: bool,

    #[arg(long)]
    pub authorized_keys_dir: Option<PathBuf>,

    /// Run init with defaults before starting if not initialized
    #[arg(long)]
    pub init_if_missing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub bind: SocketAddr,
    pub db_path: PathBuf,
    pub uds_path: PathBuf,
    pub state_dir: PathBuf,
    pub signing_key_path: PathBuf,
    pub cas_root: PathBuf,
    pub system_data_dir: PathBuf,
    pub require_auth: bool,
    pub authorized_keys_dir: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct PartialConfig {
    bind: Option<SocketAddr>,
    db_path: Option<PathBuf>,
    uds_path: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    signing_key_path: Option<PathBuf>,
    cas_root: Option<PathBuf>,
    system_data_dir: Option<PathBuf>,
    require_auth: Option<bool>,
    authorized_keys_dir: Option<PathBuf>,
}

impl Config {
    pub fn load(cli: &Cli) -> Result<Self> {
        let defaults = Self::default_paths(cli.bind)?;
        let file_cfg = if let Some(path) = &cli.config {
            Some(Self::load_file(path)?)
        } else {
            let default_config = defaults.state_dir.join("config.yaml");
            if default_config.exists() {
                Self::load_file(&default_config).ok()
            } else {
                None
            }
        };

        let state_dir = cli
            .db_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.state_dir.clone()))
            .unwrap_or_else(|| defaults.state_dir.clone());

        let cfg = Self {
            bind: file_cfg
                .as_ref()
                .and_then(|cfg| cfg.bind)
                .unwrap_or(cli.bind),
            db_path: cli
                .db_path
                .clone()
                .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.db_path.clone()))
                .unwrap_or_else(|| state_dir.join("db").join("ryeosd.sqlite3")),
            uds_path: cli
                .uds_path
                .clone()
                .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.uds_path.clone()))
                .unwrap_or_else(|| defaults.uds_path.clone()),
            state_dir: state_dir.clone(),
            signing_key_path: file_cfg
                .as_ref()
                .and_then(|cfg| cfg.signing_key_path.clone())
                .unwrap_or_else(|| state_dir.join("identity").join("node-key.pem")),
            cas_root: cli
                .cas_root
                .clone()
                .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.cas_root.clone()))
                .unwrap_or_else(|| state_dir.join("cas")),
            system_data_dir: env::var_os("RYE_SYSTEM_SPACE")
                .map(PathBuf::from)
                .or_else(|| cli.system_data_dir.clone())
                .or_else(|| {
                    file_cfg
                        .as_ref()
                        .and_then(|cfg| cfg.system_data_dir.clone())
                })
                .unwrap_or_else(|| defaults.system_data_dir.clone()),
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
                .unwrap_or_else(|| state_dir.join("auth").join("authorized_keys")),
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
        let state_dir = base_dirs
            .state_dir()
            .context("could not determine XDG state directory")?
            .join("ryeosd");
        let data_dir = base_dirs
            .data_dir()
            .join("ryeos");

        let runtime_root = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| env::temp_dir().join(format!("ryeosd-{}", current_uid())));

        Ok(Self {
            bind,
            db_path: state_dir.join("db").join("ryeosd.sqlite3"),
            uds_path: runtime_root.join("ryeosd.sock"),
            state_dir: state_dir.clone(),
            signing_key_path: state_dir.join("identity").join("node-key.pem"),
            cas_root: state_dir.join("cas"),
            system_data_dir: data_dir,
            require_auth: false,
            authorized_keys_dir: state_dir.join("auth").join("authorized_keys"),
        })
    }
}
