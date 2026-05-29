//! Resolved daemon configuration.
//!
//! `Config` is the cross-cutting, fully-resolved configuration struct
//! shared across `ryeos-app`, executor, api, and `ryeosd`. It contains
//! only data — no CLI parsing or sourcing logic.
//!
//! Daemon (`ryeosd`) owns the `clap`-based `Cli` type and uses
//! [`Config::load`] with a `ConfigSources` to produce a `Config`.
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub bind: SocketAddr,
    pub db_path: PathBuf,
    pub uds_path: PathBuf,
    /// System space root — the single directory containing the `.ai/` tree.
    /// Holds node identity, vault, runtime DB, bundles, node-config, and
    /// all bundle content. Defaults to `~/.local/share/ryeos/`.
    /// Override with `--system-space-dir` or `RYEOS_SYSTEM_SPACE_DIR` env var.
    pub system_space_dir: PathBuf,
    /// Daemon-internal signing key.
    /// Defaults to `<system_space_dir>/.ai/node/identity/private_key.pem`.
    pub node_signing_key_path: PathBuf,
    /// Operator signing key — used for operator edits in project + user space.
    /// Defaults to `<user_root>/.ai/config/keys/signing/private_key.pem`.
    pub user_signing_key_path: PathBuf,
    pub require_auth: bool,
    pub authorized_keys_dir: PathBuf,
    /// Comma-separated list of host-env var names that tool subprocesses
    /// may reference via `${VAR}` in their `env_config.env` values.
    /// This is distinct from `required_secrets`: declared secrets can
    /// be resolved from host env by name without appearing here.
    /// Also set via `RYEOS_TOOL_ENV_PASSTHROUGH` env var (env var wins).
    /// Empty by default — most deployments don't need passthrough.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_env_passthrough: Vec<String>,
}

/// Plain-data inputs for [`Config::load`]. Constructed by the daemon
/// from its `Cli` (clap) and any other CLI plumbing. Keeps `ryeos-app`
/// free of any CLI / argument-parsing dependencies.
#[derive(Debug, Clone, Default)]
pub struct ConfigSources {
    pub config_file: Option<PathBuf>,
    pub system_space_dir: Option<PathBuf>,
    pub bind: Option<SocketAddr>,
    pub db_path: Option<PathBuf>,
    pub uds_path: Option<PathBuf>,
    pub require_auth: bool,
    pub authorized_keys_dir: Option<PathBuf>,
    pub force: bool,
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
    tool_env_passthrough: Option<Vec<String>>,
}

impl Config {
    pub fn load(sources: &ConfigSources) -> Result<Self> {
        let compiled_default: SocketAddr = "127.0.0.1:7400".parse().unwrap();
        let defaults = Self::default_paths(compiled_default)?;

        // Resolve system_space_dir from CLI/env BEFORE looking up
        // `<system_space_dir>/.ai/node/config.yaml` so an explicit
        // `--system-space-dir` (or `RYEOS_SYSTEM_SPACE_DIR`) is honored
        // when locating the stored config. Without this, the loader
        // would always read `<XDG default>/.ai/node/config.yaml` —
        // which causes test fixtures to surprise-load a developer's
        // real install config.
        let ssd_explicit = sources
            .system_space_dir
            .clone()
            .or_else(|| env::var_os("RYEOS_SYSTEM_SPACE_DIR").map(PathBuf::from));

        let file_cfg = if let Some(path) = &sources.config_file {
            Some(Self::load_file(path)?)
        } else {
            let lookup_dir = ssd_explicit
                .as_deref()
                .unwrap_or(&defaults.system_space_dir);
            let default_config = lookup_dir.join(".ai").join("node").join("config.yaml");
            if default_config.exists() {
                Some(Self::load_file(&default_config).with_context(|| {
                    format!(
                        "failed to load existing config at {}",
                        default_config.display()
                    )
                })?)
            } else {
                None
            }
        };

        // R1: Typed --bind precedence. CLI `--bind` is Option<SocketAddr>;
        // None means the operator omitted it.
        let file_bind = file_cfg.as_ref().and_then(|cfg| cfg.bind);
        let resolved_bind = match (file_bind, sources.bind) {
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
                if !sources.force {
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

        // Final system_space_dir: explicit CLI/env > config file > default
        let system_space_dir = ssd_explicit
            .or_else(|| {
                file_cfg
                    .as_ref()
                    .and_then(|cfg| cfg.system_space_dir.clone())
            })
            .unwrap_or_else(|| defaults.system_space_dir.clone());

        let cfg = Self {
            bind: resolved_bind,
            db_path: sources
                .db_path
                .clone()
                .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.db_path.clone()))
                .unwrap_or_else(|| {
                    system_space_dir
                        .join(".ai")
                        .join("state")
                        .join("runtime.sqlite3")
                }),
            uds_path: sources
                .uds_path
                .clone()
                .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.uds_path.clone()))
                .unwrap_or_else(|| defaults.uds_path.clone()),
            system_space_dir: system_space_dir.clone(),
            node_signing_key_path: file_cfg
                .as_ref()
                .and_then(|cfg| cfg.node_signing_key_path.clone())
                .unwrap_or_else(|| {
                    system_space_dir
                        .join(".ai")
                        .join("node")
                        .join("identity")
                        .join("private_key.pem")
                }),
            user_signing_key_path: file_cfg
                .as_ref()
                .and_then(|cfg| cfg.user_signing_key_path.clone())
                .or_else(|| env::var_os("RYEOS_SIGNING_KEY_PATH").map(PathBuf::from))
                .unwrap_or_else(|| defaults.user_signing_key_path.clone()),
            require_auth: sources.require_auth
                || file_cfg
                    .as_ref()
                    .and_then(|cfg| cfg.require_auth)
                    .unwrap_or(false),
            authorized_keys_dir: sources
                .authorized_keys_dir
                .clone()
                .or_else(|| {
                    file_cfg
                        .as_ref()
                        .and_then(|cfg| cfg.authorized_keys_dir.clone())
                })
                .unwrap_or_else(|| {
                    system_space_dir
                        .join(".ai")
                        .join("node")
                        .join("auth")
                        .join("authorized_keys")
                }),
            // tool_env_passthrough: config file list is the base.
            // RYEOS_TOOL_ENV_PASSTHROUGH env var (comma-separated)
            // overrides if set — mirrors Docker usage where the env
            // var is more convenient than editing config.yaml.
            tool_env_passthrough: if let Ok(raw) = env::var("RYEOS_TOOL_ENV_PASSTHROUGH") {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            } else {
                file_cfg
                    .as_ref()
                    .and_then(|cfg| cfg.tool_env_passthrough.clone())
                    .unwrap_or_default()
            },
        };

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
        let system_space_dir = base_dirs.data_dir().join("ryeos");

        let runtime_root = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| env::temp_dir().join(format!("ryeosd-{}", current_uid())));

        // User-space root: canonical `<home>/.ryeos/`. Resolved via
        // `ryeos_engine::roots::user_root()` so the daemon and CLI agree
        // on a single resolver. Honours `USER_SPACE` env override.
        let user_root = ryeos_engine::roots::user_root()
            .context("could not resolve user root for default user_signing_key_path")?;

        Ok(Self {
            bind,
            db_path: system_space_dir
                .join(".ai")
                .join("state")
                .join("runtime.sqlite3"),
            uds_path: runtime_root.join("ryeosd.sock"),
            system_space_dir: system_space_dir.clone(),
            node_signing_key_path: system_space_dir
                .join(".ai")
                .join("node")
                .join("identity")
                .join("private_key.pem"),
            user_signing_key_path: user_root
                .join(".ai")
                .join("config")
                .join("keys")
                .join("signing")
                .join("private_key.pem"),
            require_auth: false,
            authorized_keys_dir: system_space_dir
                .join(".ai")
                .join("node")
                .join("auth")
                .join("authorized_keys"),
            tool_env_passthrough: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn load_is_side_effect_free_for_runtime_paths() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let user = tmp.path().join("user");
        std::env::set_var("USER_SPACE", &user);
        let system_space_dir = tmp.path().join("state");
        let db_path = tmp.path().join("runtime/state/runtime.sqlite3");
        let uds_path = tmp.path().join("runtime/sock/ryeosd.sock");

        let cfg = Config::load(&ConfigSources {
            system_space_dir: Some(system_space_dir.clone()),
            db_path: Some(db_path.clone()),
            uds_path: Some(uds_path.clone()),
            ..ConfigSources::default()
        })
        .unwrap();

        assert_eq!(cfg.system_space_dir, system_space_dir);
        assert_eq!(cfg.db_path, db_path);
        assert_eq!(cfg.uds_path, uds_path);
        assert!(!cfg.system_space_dir.exists());
        assert!(!cfg.db_path.parent().unwrap().exists());
        assert!(!cfg.uds_path.parent().unwrap().exists());
        std::env::remove_var("USER_SPACE");
    }
}
