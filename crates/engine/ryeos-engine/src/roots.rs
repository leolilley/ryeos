//! Canonical root-discovery for daemon, CLI, and subprocess tools.
//!
//! RyeOS uses one app root plus project roots:
//!
//!   * `app_root()`     → operator-owned `.ai/` tree
//!   * `runtime_root()` → typed writable runtime/config/state view
//!   * `install_root()` → typed read-only installed-content view
//!   * `bundle_roots()` → ordered list of installed bundle roots
//!
//! Every Rye-aware root is identified by the presence of a `.ai/`
//! sub-directory. The *parent* of that `.ai/` is the root. Callers needing
//! operator roots go through this module rather than reading app-root env vars
//! ad hoc.

use std::path::{Path, PathBuf};

use crate::AI_DIR;

/// Read-only handle to the installed bundle/config zone.
///
/// Today install and runtime roots share one physical app root. The type split
/// keeps write-capable APIs from accidentally receiving the install zone, and
/// leaves room for a physical split later.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct InstallRoot(PathBuf);

impl InstallRoot {
    pub fn new(root: PathBuf) -> Self {
        Self(root)
    }

    fn ai(&self) -> PathBuf {
        self.0.join(AI_DIR)
    }

    pub fn read_path(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.ai().join(rel)
    }

    pub fn bundles(&self) -> PathBuf {
        self.ai().join("bundles")
    }
}

/// Writable handle to the runtime/config/state zone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct RuntimeRoot(PathBuf);

impl RuntimeRoot {
    pub fn new(root: PathBuf) -> Self {
        Self(root)
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn ai(&self) -> PathBuf {
        self.0.join(AI_DIR)
    }

    pub fn config(&self) -> PathBuf {
        self.ai().join("config")
    }

    pub fn state(&self) -> PathBuf {
        self.ai().join("state")
    }

    pub fn node(&self) -> PathBuf {
        self.ai().join("node")
    }

    pub fn cache(&self) -> PathBuf {
        self.state().join("cache")
    }

    pub fn operator_signing_key_path(&self) -> PathBuf {
        self.config()
            .join("keys")
            .join("signing")
            .join("private_key.pem")
    }

    pub fn trusted_keys_dir(&self) -> PathBuf {
        self.config().join("keys").join("trusted")
    }

    pub fn node_signing_key_path(&self) -> PathBuf {
        self.node().join("identity").join("private_key.pem")
    }

    pub fn authorized_keys_dir(&self) -> PathBuf {
        self.node().join("auth").join("authorized_keys")
    }
}

impl AsRef<Path> for RuntimeRoot {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

pub fn write_guarded<'a>(runtime_root: &RuntimeRoot, path: &'a Path) -> &'a Path {
    let runtime_ai = runtime_root.ai();
    let install_bundles = runtime_ai.join("bundles");
    debug_assert!(
        path.starts_with(&runtime_ai),
        "runtime write outside runtime root: {} not under {}",
        path.display(),
        runtime_ai.display()
    );
    debug_assert!(
        !path.starts_with(&install_bundles),
        "runtime write attempted under install bundles: {}",
        path.display()
    );
    path
}

#[derive(Debug, thiserror::Error)]
pub enum RootError {
    #[error(
        "could not resolve app root: set RYEOS_APP_ROOT or run \
             under a user account with a discoverable data directory"
    )]
    AppRootUnresolvable,
}

/// Resolve the single Rye app root.
///
/// Precedence: `RYEOS_APP_ROOT` env > `<data_dir>/ryeos` > error.
pub fn app_root() -> Result<PathBuf, RootError> {
    if let Some(p) = std::env::var_os("RYEOS_APP_ROOT") {
        return Ok(PathBuf::from(p));
    }
    if let Some(dirs) = directories::BaseDirs::new() {
        return Ok(dirs.data_dir().join("ryeos"));
    }
    Err(RootError::AppRootUnresolvable)
}

pub fn install_root() -> Result<InstallRoot, RootError> {
    Ok(InstallRoot::new(app_root()?))
}

pub fn runtime_root() -> Result<RuntimeRoot, RootError> {
    Ok(RuntimeRoot::new(app_root()?))
}

/// Ordered list of system bundle roots.
///
/// Precedence (each appended in order, deduplicated):
///
///   1. `RYEOS_APP_ROOT` env (single path)
///   2. `additional_roots` (caller-supplied — e.g. node-config
///      `bundles` registrations)
///   3. `BaseDirs::data_dir()/ryeos` (default XDG core install)
///
/// Callers MUST pass `additional_roots` explicitly. There is no
/// "use the daemon's state dir" magic in this module; the caller
/// resolves bundle registrations and passes them in.
pub fn bundle_roots(additional_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.iter().any(|q| q == &p) {
            out.push(p);
        }
    };
    if let Some(p) = std::env::var_os("RYEOS_APP_ROOT") {
        push(PathBuf::from(p));
    }
    for r in additional_roots {
        push(r.clone());
    }
    if let Some(dirs) = directories::BaseDirs::new() {
        push(dirs.data_dir().join("ryeos"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    // Process-wide mutex; RYEOS_APP_ROOT is shared global state.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn bundle_roots_dedupes_and_orders() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("RYEOS_APP_ROOT", "/tmp/sys-a");
        let extra = vec![PathBuf::from("/tmp/sys-a"), PathBuf::from("/tmp/sys-b")];
        let r = bundle_roots(&extra);
        // /tmp/sys-a appears first (env), de-duplicated when seen again
        // in additional_roots; /tmp/sys-b after that.
        assert_eq!(r[0], PathBuf::from("/tmp/sys-a"));
        assert!(r.iter().any(|p| p == &PathBuf::from("/tmp/sys-b")));
        // No duplicate /tmp/sys-a.
        let count = r.iter().filter(|p| **p == Path::new("/tmp/sys-a")).count();
        assert_eq!(count, 1);
        std::env::remove_var("RYEOS_APP_ROOT");
    }

    #[test]
    #[should_panic(expected = "runtime write attempted under install bundles")]
    fn write_guarded_rejects_install_bundle_path() {
        let root = RuntimeRoot::new(PathBuf::from("/tmp/ryeos-test-root"));
        let path = root.ai().join("bundles/core/.ai/some-file");
        let _ = write_guarded(&root, &path);
    }
}
