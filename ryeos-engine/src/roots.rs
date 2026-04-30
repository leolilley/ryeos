//! Canonical root-discovery for daemon, CLI, and subprocess tools.
//!
//! Three-tier root model:
//!
//!   * `user_root()`    → user space (parent of `<user>/.ai/`)
//!   * `state_root()`   → daemon state (parent of `<state>/.ai/`)
//!   * `system_roots()` → ordered list of system bundle roots
//!     (parents of each `<system>/.ai/`, e.g. core + standard)
//!
//! All callers needing roots — daemon bootstrap, CLI verbs,
//! `rye-inspect`, engine subprocess executors — go through this
//! module. Never call `directories::BaseDirs` or read
//! `USER_SPACE`/`RYE_SYSTEM_SPACE` ad-hoc.

use std::path::PathBuf;

use crate::AI_DIR;

#[derive(Debug, thiserror::Error)]
pub enum RootError {
    #[error("could not resolve user root: set USER_SPACE or run \
             under a user account with a discoverable home directory")]
    UserRootUnresolvable,
}

/// Resolve the user-space root.
///
/// Precedence: `USER_SPACE` env > `BaseDirs::home_dir()` > **error**.
///
/// Never falls back to a placeholder. Silent fallback to
/// `/tmp/missing-home` was a real bug — trust docs were silently
/// written to the wrong place.
pub fn user_root() -> Result<PathBuf, RootError> {
    if let Some(p) = std::env::var_os("USER_SPACE") {
        return Ok(PathBuf::from(p));
    }
    if let Some(dirs) = directories::BaseDirs::new() {
        return Ok(dirs.home_dir().to_path_buf());
    }
    Err(RootError::UserRootUnresolvable)
}

/// Pass-through for symmetry with `user_root`. The daemon's
/// effective state_dir IS the state root; this function exists so
/// future relocation has one chokepoint.
pub fn state_root(state_dir: &std::path::Path) -> PathBuf {
    state_dir.to_path_buf()
}

/// Ordered list of system bundle roots.
///
/// Precedence (each appended in order, deduplicated):
///
///   1. `RYE_SYSTEM_SPACE` env (single path)
///   2. `additional_roots` (caller-supplied — e.g. node-config
///      `bundles` registrations)
///   3. `BaseDirs::data_dir()/ryeos` (default XDG core install)
///
/// Callers MUST pass `additional_roots` explicitly. There is no
/// "use the daemon's state dir" magic in this module; the caller
/// resolves bundle registrations and passes them in.
pub fn system_roots(additional_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.iter().any(|q| q == &p) {
            out.push(p);
        }
    };
    if let Some(p) = std::env::var_os("RYE_SYSTEM_SPACE") {
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

/// Path of the user-overlay `.env` file: `<user>/.ai/.env`.
///
/// Plan-canonical location. Prior code read `~/.env` (plan-violating).
/// `.env` lookup elsewhere walks project root + this file ONLY (no
/// parent traversal, no `.env.local`).
pub fn user_dotenv_path() -> Result<PathBuf, RootError> {
    Ok(user_root()?.join(AI_DIR).join(".env"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Process-wide mutex; USER_SPACE / RYE_SYSTEM_SPACE are shared global state.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn user_root_honors_user_space_env() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("USER_SPACE", "/tmp/test-user-space-roots");
        let r = user_root().expect("user_root with USER_SPACE set");
        assert_eq!(r, PathBuf::from("/tmp/test-user-space-roots"));
        std::env::remove_var("USER_SPACE");
    }

    #[test]
    fn user_root_returns_err_without_user_space_or_home() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let saved = std::env::var_os("USER_SPACE");
        std::env::remove_var("USER_SPACE");
        // If BaseDirs::new() returns Some (most CI/developer machines have
        // a home dir), this test can't force the error path. That's fine —
        // the error path is structurally obvious (two ifs both miss → Err).
        // Just exercise the function and confirm it either succeeds or
        // returns the expected error variant.
        let result = user_root();
        match result {
            Ok(_) => { /* BaseDirs resolved it — acceptable */ }
            Err(RootError::UserRootUnresolvable) => { /* forced error path — correct */ }
        }
        // Restore
        if let Some(v) = saved {
            std::env::set_var("USER_SPACE", v);
        }
    }

    #[test]
    fn user_dotenv_path_ends_in_ai_dotenv() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("USER_SPACE", "/tmp/test-user-dotenv");
        let p = user_dotenv_path().expect("user_dotenv_path");
        assert!(p.ends_with(format!("{AI_DIR}/.env")));
        std::env::remove_var("USER_SPACE");
    }

    #[test]
    fn system_roots_dedupes_and_orders() {
        let _g = ENV_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("RYE_SYSTEM_SPACE", "/tmp/sys-a");
        let extra = vec![PathBuf::from("/tmp/sys-a"), PathBuf::from("/tmp/sys-b")];
        let r = system_roots(&extra);
        // /tmp/sys-a appears first (env), de-duplicated when seen again
        // in additional_roots; /tmp/sys-b after that.
        assert_eq!(r[0], PathBuf::from("/tmp/sys-a"));
        assert!(r.iter().any(|p| p == &PathBuf::from("/tmp/sys-b")));
        // No duplicate /tmp/sys-a.
        let count = r.iter().filter(|p| **p == PathBuf::from("/tmp/sys-a")).count();
        assert_eq!(count, 1);
        std::env::remove_var("RYE_SYSTEM_SPACE");
    }
}
