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

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use ryeos_engine::roots::{InstallRoot, RuntimeRoot};
use serde::{Deserialize, Serialize};

const DAEMON_CONFIG_MAX_BYTES: u64 = 64 * 1024;
const RETIRED_ACCOUNTING_ISSUE_ACCEPTANCE_WINDOW_MS: u64 = 60_000;

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
    /// App root — the single directory containing the `.ai/` tree.
    /// Defaults to `<data_dir>/ryeos`; override with `--app-root` or
    /// `RYEOS_APP_ROOT`.
    pub app_root: PathBuf,
    /// Daemon-internal signing key.
    /// Defaults to `<app_root>/.ai/node/identity/private_key.pem`.
    pub node_signing_key_path: PathBuf,
    /// Operator signing key — used for operator edits in project/config space.
    /// Defaults to `<app_root>/.ai/config/keys/signing/private_key.pem`.
    pub operator_signing_key_path: PathBuf,
    pub authorized_keys_dir: PathBuf,
}

/// Plain-data inputs for [`Config::load`]. Constructed by the daemon
/// from its `Cli` (clap) and any other CLI plumbing. Keeps `ryeos-app`
/// free of any CLI / argument-parsing dependencies.
#[derive(Debug, Clone, Default)]
pub struct ConfigSources {
    pub config_file: Option<PathBuf>,
    pub app_root: Option<PathBuf>,
    pub bind: Option<SocketAddr>,
    pub db_path: Option<PathBuf>,
    pub uds_path: Option<PathBuf>,
    pub authorized_keys_dir: Option<PathBuf>,
    pub force: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialConfig {
    bind: Option<SocketAddr>,
    db_path: Option<PathBuf>,
    uds_path: Option<PathBuf>,
    app_root: Option<PathBuf>,
    node_signing_key_path: Option<PathBuf>,
    operator_signing_key_path: Option<PathBuf>,
    authorized_keys_dir: Option<PathBuf>,
}

/// Exact daemon-config shape written before semantic node policy moved into
/// the atomic node-policy generation. This is accepted only by the explicit
/// stopped-node init cutover below; normal config loading remains strict.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreNodePolicyConfig {
    bind: SocketAddr,
    db_path: PathBuf,
    uds_path: PathBuf,
    app_root: PathBuf,
    node_signing_key_path: PathBuf,
    operator_signing_key_path: PathBuf,
    require_auth: bool,
    authorized_keys_dir: PathBuf,
    #[serde(default)]
    tool_env_passthrough: Vec<String>,
    #[serde(default = "retired_accounting_issue_acceptance_window_ms")]
    accounting_issue_acceptance_window_ms: u64,
}

fn retired_accounting_issue_acceptance_window_ms() -> u64 {
    RETIRED_ACCOUNTING_ISSUE_ACCEPTANCE_WINDOW_MS
}

/// Permanently retire the exact predecessor daemon-config shape during
/// explicit stopped-node initialization.
///
/// This is a clean schema cut, not a compatibility parser: [`Config::load`]
/// never accepts the retired fields. Init preserves only bootstrap location
/// fields and refuses to discard any non-default policy value, which must be
/// represented deliberately in the node's atomic policy generation instead.
pub fn retire_pre_node_policy_config(app_root: &Path) -> Result<bool> {
    let node_directory_path = app_root.join(ryeos_engine::AI_DIR).join("node");
    let path = node_directory_path.join("config.yaml");
    let node_directory =
        lillux::PinnedDirectory::open(&node_directory_path)?.with_context(|| {
            format!(
                "open daemon config directory {}",
                node_directory_path.display()
            )
        })?;
    let Some(config_file) =
        node_directory.open_pinned_regular(std::ffi::OsStr::new("config.yaml"), false)?
    else {
        return Ok(false);
    };
    let bytes = config_file
        .read_bounded(DAEMON_CONFIG_MAX_BYTES)
        .with_context(|| format!("read daemon config cutover input {}", path.display()))?;

    if serde_yaml::from_slice::<PartialConfig>(&bytes).is_ok() {
        return Ok(false);
    }

    let predecessor: PreNodePolicyConfig = serde_yaml::from_slice(&bytes).with_context(|| {
        format!(
            "daemon config {} is neither the current bootstrap schema nor the exact predecessor node-policy schema",
            path.display()
        )
    })?;
    if predecessor.app_root != app_root {
        bail!(
            "predecessor daemon config app_root {} does not match initialized app root {}",
            predecessor.app_root.display(),
            app_root.display()
        );
    }
    if predecessor.require_auth
        || !predecessor.tool_env_passthrough.is_empty()
        || predecessor.accounting_issue_acceptance_window_ms
            != RETIRED_ACCOUNTING_ISSUE_ACCEPTANCE_WINDOW_MS
    {
        bail!(
            "predecessor daemon config contains non-default semantic policy; author the equivalent current node policy explicitly before retiring {}",
            path.display()
        );
    }

    let current = Config {
        bind: predecessor.bind,
        db_path: predecessor.db_path,
        uds_path: predecessor.uds_path,
        app_root: predecessor.app_root,
        node_signing_key_path: predecessor.node_signing_key_path,
        operator_signing_key_path: predecessor.operator_signing_key_path,
        authorized_keys_dir: predecessor.authorized_keys_dir,
    };
    let yaml = serde_yaml::to_string(&current)
        .context("serialize current daemon bootstrap config during policy cutover")?;
    node_directory
        .atomic_write_pinned_if_same(
            std::ffi::OsStr::new("config.yaml"),
            Some(&config_file),
            yaml.as_bytes(),
            0o600,
        )
        .with_context(|| format!("publish current daemon config {}", path.display()))?;
    Ok(true)
}

impl Config {
    pub fn runtime_root(&self) -> RuntimeRoot {
        RuntimeRoot::new(self.app_root.clone())
    }

    pub fn install_root(&self) -> InstallRoot {
        InstallRoot::new(self.app_root.clone())
    }

    pub fn runtime_config_dir(&self) -> PathBuf {
        self.runtime_root().config()
    }

    pub fn runtime_state_dir(&self) -> PathBuf {
        self.runtime_root().state()
    }

    pub fn runtime_node_dir(&self) -> PathBuf {
        self.runtime_root().node()
    }

    pub fn load(sources: &ConfigSources) -> Result<Self> {
        let compiled_default: SocketAddr = "127.0.0.1:7400".parse().unwrap();
        let defaults = Self::default_paths(compiled_default)?;

        // Resolve app_root from CLI/env BEFORE looking up
        // `<app_root>/.ai/node/config.yaml` so an explicit
        // `--app-root` (or `RYEOS_APP_ROOT`) is honored
        // when locating the stored config. Without this, the loader
        // would always read `<XDG default>/.ai/node/config.yaml` —
        // which causes test fixtures to surprise-load a developer's
        // real install config.
        let ssd_explicit = sources
            .app_root
            .clone()
            .or_else(|| env::var_os("RYEOS_APP_ROOT").map(PathBuf::from));

        let file_cfg = if let Some(path) = &sources.config_file {
            Some(Self::load_file(path)?)
        } else {
            let lookup_dir = ssd_explicit.as_deref().unwrap_or(&defaults.app_root);
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

        // Final app root: explicit CLI/env > config file > default.
        let app_root = ssd_explicit
            .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.app_root.clone()))
            .unwrap_or_else(|| defaults.app_root.clone());
        let resolved_runtime_root = RuntimeRoot::new(app_root.clone());
        let canonical_operator_key_path = resolved_runtime_root.operator_signing_key_path();
        if let Some(path) = file_cfg
            .as_ref()
            .and_then(|cfg| cfg.operator_signing_key_path.as_ref())
            && path != &canonical_operator_key_path
        {
            bail!(
                "operator_signing_key_path must be {}; got {}",
                canonical_operator_key_path.display(),
                path.display()
            );
        }

        let cfg = Self {
            bind: resolved_bind,
            db_path: sources
                .db_path
                .clone()
                .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.db_path.clone()))
                .unwrap_or_else(|| app_root.join(".ai").join("state").join("runtime.sqlite3")),
            uds_path: sources
                .uds_path
                .clone()
                .or_else(|| file_cfg.as_ref().and_then(|cfg| cfg.uds_path.clone()))
                .unwrap_or_else(|| defaults.uds_path.clone()),
            app_root: app_root.clone(),
            node_signing_key_path: file_cfg
                .as_ref()
                .and_then(|cfg| cfg.node_signing_key_path.clone())
                .unwrap_or_else(|| resolved_runtime_root.node_signing_key_path()),
            operator_signing_key_path: file_cfg
                .as_ref()
                .map(|_| canonical_operator_key_path.clone())
                .unwrap_or(canonical_operator_key_path),
            authorized_keys_dir: sources
                .authorized_keys_dir
                .clone()
                .or_else(|| {
                    file_cfg
                        .as_ref()
                        .and_then(|cfg| cfg.authorized_keys_dir.clone())
                })
                .unwrap_or_else(|| resolved_runtime_root.authorized_keys_dir()),
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
        let app_root = base_dirs.data_dir().join("ryeos");
        let runtime_root = RuntimeRoot::new(app_root.clone());

        let socket_runtime_root = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| env::temp_dir().join(format!("ryeosd-{}", current_uid())));

        Ok(Self {
            bind,
            db_path: runtime_root.state().join("runtime.sqlite3"),
            uds_path: socket_runtime_root.join("ryeosd.sock"),
            app_root: app_root.clone(),
            node_signing_key_path: runtime_root.node_signing_key_path(),
            operator_signing_key_path: runtime_root.operator_signing_key_path(),
            authorized_keys_dir: runtime_root.authorized_keys_dir(),
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
        let app_root = tmp.path().join("state");
        let db_path = tmp.path().join("runtime/state/runtime.sqlite3");
        let uds_path = tmp.path().join("runtime/sock/ryeosd.sock");

        let cfg = Config::load(&ConfigSources {
            app_root: Some(app_root.clone()),
            db_path: Some(db_path.clone()),
            uds_path: Some(uds_path.clone()),
            ..ConfigSources::default()
        })
        .unwrap();

        assert_eq!(cfg.app_root, app_root);
        assert_eq!(cfg.db_path, db_path);
        assert_eq!(cfg.uds_path, uds_path);
        assert!(!cfg.app_root.exists());
        assert!(!cfg.db_path.parent().unwrap().exists());
        assert!(!cfg.uds_path.parent().unwrap().exists());
    }

    #[test]
    fn explicit_init_cutover_retires_pre_policy_fields_once() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("state");
        let config_path = app_root.join(".ai/node/config.yaml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            format!(
                "bind: 127.0.0.1:7412\n\
                 db_path: {root}/.ai/state/runtime.sqlite3\n\
                 uds_path: {root}/node.sock\n\
                 app_root: {root}\n\
                 node_signing_key_path: {root}/.ai/node/identity/private_key.pem\n\
                 operator_signing_key_path: {root}/.ai/config/keys/signing/private_key.pem\n\
                 require_auth: false\n\
                 authorized_keys_dir: {root}/.ai/node/auth/authorized_keys\n\
                 accounting_issue_acceptance_window_ms: 60000\n",
                root = app_root.display()
            ),
        )
        .unwrap();

        assert!(retire_pre_node_policy_config(&app_root).unwrap());
        assert!(!retire_pre_node_policy_config(&app_root).unwrap());

        let body = std::fs::read_to_string(&config_path).unwrap();
        assert!(!body.contains("require_auth"));
        assert!(!body.contains("tool_env_passthrough"));
        assert!(!body.contains("accounting_issue_acceptance_window_ms"));
        let config = Config::load(&ConfigSources {
            app_root: Some(app_root.clone()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(config.bind, "127.0.0.1:7412".parse().unwrap());
        assert_eq!(config.app_root, app_root);
    }

    #[test]
    fn explicit_init_cutover_refuses_to_discard_semantic_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("state");
        let config_path = app_root.join(".ai/node/config.yaml");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let predecessor = format!(
            "bind: 127.0.0.1:7400\n\
             db_path: {root}/.ai/state/runtime.sqlite3\n\
             uds_path: {root}/node.sock\n\
             app_root: {root}\n\
             node_signing_key_path: {root}/.ai/node/identity/private_key.pem\n\
             operator_signing_key_path: {root}/.ai/config/keys/signing/private_key.pem\n\
             require_auth: true\n\
             authorized_keys_dir: {root}/.ai/node/auth/authorized_keys\n\
             accounting_issue_acceptance_window_ms: 60000\n",
            root = app_root.display()
        );
        std::fs::write(&config_path, &predecessor).unwrap();

        let error = retire_pre_node_policy_config(&app_root).unwrap_err();
        assert!(error.to_string().contains("non-default semantic policy"));
        assert_eq!(std::fs::read_to_string(config_path).unwrap(), predecessor);
    }
}
