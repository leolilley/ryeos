//! Principal path and YAML helpers for RyeOS UI state.
//!
//! This module is the local-install seam for future principal/tenant-aware
//! resolution. Callers should use logical `config/*` and `state/*` paths here
//! instead of constructing app-root paths ad hoc.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::{Mutex, MutexGuard};

/// Synthetic principal for the current local single-user install.
///
/// Hosted/multi-principal mode should derive a real principal from the
/// authenticated caller and resolve through [`PrincipalResolver`] without
/// changing callers that operate on [`PrincipalPaths`].
pub const LOCAL_PRINCIPAL_ID: &str = "local";

static PRINCIPAL_YAML_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalPaths {
    pub root: PathBuf,
}

impl PrincipalPaths {
    pub fn resolve() -> Result<Self> {
        LocalPrincipalResolver.resolve(LOCAL_PRINCIPAL_ID)
    }

    fn resolve_local() -> Result<Self> {
        let root = ryeos_engine::roots::app_root().context("failed to resolve app root")?;
        Ok(Self { root })
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn config(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(".ai").join("config").join(rel.as_ref())
    }

    pub fn state(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(".ai").join("state").join(rel.as_ref())
    }

    pub fn projects_config(&self) -> PathBuf {
        self.config("projects.yaml")
    }

    pub fn ryeos_config(&self) -> PathBuf {
        self.config("ryeos-ui.yaml")
    }

    pub fn ryeos_tty_config(&self) -> PathBuf {
        self.config("ryeos-tty.yaml")
    }

    pub fn ryeos_recent(&self) -> PathBuf {
        self.state("ryeos-ui/recent.yaml")
    }

    pub fn ryeos_tty_home(&self) -> PathBuf {
        self.state("ryeos-tty/home.yaml")
    }
}

/// Resolves logical storage space for a principal.
///
/// Local RyeOS maps every caller to the same local space. Future hosted
/// mode can replace this with a resolver backed by per-principal filesystem,
/// database, or object storage while preserving the logical config/state paths.
pub trait PrincipalResolver {
    fn resolve(&self, principal_id: &str) -> Result<PrincipalPaths>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LocalPrincipalResolver;

impl PrincipalResolver for LocalPrincipalResolver {
    fn resolve(&self, principal_id: &str) -> Result<PrincipalPaths> {
        if principal_id.trim().is_empty() {
            anyhow::bail!("principal id is required to resolve principal space");
        }
        PrincipalPaths::resolve_local()
    }
}

#[derive(Debug, Clone)]
pub struct HostedPrincipalResolver {
    principal_root: PathBuf,
}

impl HostedPrincipalResolver {
    pub fn for_app_root(app_root: impl Into<PathBuf>) -> Self {
        Self {
            principal_root: app_root.into().join(".ai").join("principals"),
        }
    }
}

impl PrincipalResolver for HostedPrincipalResolver {
    fn resolve(&self, principal_id: &str) -> Result<PrincipalPaths> {
        let principal_key = principal_storage_key(principal_id)?;
        Ok(PrincipalPaths::new(
            self.principal_root.join(principal_key).join("space"),
        ))
    }
}

pub fn principal_storage_key(principal_id: &str) -> Result<String> {
    Ok(ryeos_state::refs::principal_storage_key(principal_id)?.to_owned())
}

#[derive(Debug, Clone)]
pub struct PrincipalStore {
    paths: PrincipalPaths,
}

pub struct LockedPrincipalStore {
    store: PrincipalStore,
    _guard: MutexGuard<'static, ()>,
}

impl PrincipalStore {
    pub fn resolve_principal(principal_id: &str) -> Result<Self> {
        Self::resolve_with(&LocalPrincipalResolver, principal_id)
    }

    pub fn resolve_with<R>(resolver: &R, principal_id: &str) -> Result<Self>
    where
        R: PrincipalResolver,
    {
        Ok(Self {
            paths: resolver.resolve(principal_id)?,
        })
    }

    pub async fn locked_principal(principal_id: &str) -> Result<LockedPrincipalStore> {
        Self::locked_with(&LocalPrincipalResolver, principal_id).await
    }

    pub async fn locked_with<R>(resolver: &R, principal_id: &str) -> Result<LockedPrincipalStore>
    where
        R: PrincipalResolver,
    {
        let store = Self::resolve_with(resolver, principal_id)?;
        let guard = principal_yaml_lock().lock().await;
        Ok(LockedPrincipalStore {
            store,
            _guard: guard,
        })
    }

    pub fn paths(&self) -> &PrincipalPaths {
        &self.paths
    }

    pub fn load_yaml<T>(&self, path: &Path) -> Result<T>
    where
        T: DeserializeOwned + Default,
    {
        read_yaml_or_default(path)
    }
}

impl LockedPrincipalStore {
    pub fn write_yaml<T>(&self, path: &Path, value: &T) -> Result<()>
    where
        T: Serialize,
    {
        write_yaml_atomic(path, value)
    }
}

impl std::ops::Deref for LockedPrincipalStore {
    type Target = PrincipalStore;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

fn principal_yaml_lock() -> &'static Mutex<()> {
    PRINCIPAL_YAML_LOCK.get_or_init(|| Mutex::new(()))
}

pub fn read_yaml_or_default<T>(path: &Path) -> Result<T>
where
    T: DeserializeOwned + Default,
{
    if !path.exists() {
        return Ok(T::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn write_yaml_atomic<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    ensure_private_parent_dirs(path)?;
    let body = serde_yaml::to_string(value)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    crate::io::atomic::atomic_write(path, body.as_bytes())?;
    set_private_file_permissions(path)?;
    Ok(())
}

fn ensure_private_parent_dirs(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir {}", parent.display()))?;
        let mut dirs = Vec::new();
        let mut current = Some(parent);
        let mut outermost_ai_dir_index = None;
        while let Some(dir) = current {
            if dir.file_name().is_some_and(|name| name == ".ai") {
                outermost_ai_dir_index = Some(dirs.len());
            }
            dirs.push(dir);
            current = dir.parent();
        }
        let dirs_to_chmod: Vec<&Path> = if let Some(outermost_ai_dir_index) = outermost_ai_dir_index
        {
            dirs.into_iter()
                .take(outermost_ai_dir_index + 1)
                .rev()
                .collect()
        } else {
            vec![parent]
        };
        for dir in dirs_to_chmod {
            set_private_dir_permissions(dir)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to chmod 0700 {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod 0600 {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[derive(Debug, Default, Serialize, serde::Deserialize, PartialEq)]
    struct Demo {
        value: String,
    }

    struct FixedResolver {
        root: PathBuf,
    }

    impl PrincipalResolver for FixedResolver {
        fn resolve(&self, principal_id: &str) -> Result<PrincipalPaths> {
            if principal_id != "fp:test" {
                anyhow::bail!("unexpected principal {principal_id}");
            }
            Ok(PrincipalPaths::new(self.root.clone()))
        }
    }

    #[test]
    fn logical_paths_live_under_user_ai_config_and_state() {
        let paths = PrincipalPaths::new(PathBuf::from("/tmp/user"));
        assert_eq!(
            paths.projects_config(),
            PathBuf::from("/tmp/user/.ai/config/projects.yaml")
        );
        assert_eq!(
            paths.ryeos_recent(),
            PathBuf::from("/tmp/user/.ai/state/ryeos-ui/recent.yaml")
        );
        assert_eq!(
            paths.ryeos_tty_config(),
            PathBuf::from("/tmp/user/.ai/config/ryeos-tty.yaml")
        );
        assert_eq!(
            paths.ryeos_tty_home(),
            PathBuf::from("/tmp/user/.ai/state/ryeos-tty/home.yaml")
        );
    }

    #[test]
    fn local_resolver_requires_a_principal_but_keeps_local_storage() {
        let err = LocalPrincipalResolver.resolve("").unwrap_err();
        assert!(err.to_string().contains("principal id is required"));

        let resolved = LocalPrincipalResolver
            .resolve("fp:test")
            .expect("local resolver should ignore principal storage partitioning");
        assert_eq!(resolved, PrincipalPaths::resolve().unwrap());
    }

    #[test]
    fn principal_resolver_maps_fp_to_isolated_space() {
        let resolver = HostedPrincipalResolver::for_app_root("/tmp/system");
        let principal = format!("fp:{}", "ab".repeat(32));
        let paths = resolver.resolve(&principal).unwrap();

        assert_eq!(principal_storage_key(&principal).unwrap(), "ab".repeat(32));
        assert_eq!(
            paths.root,
            PathBuf::from(format!(
                "/tmp/system/.ai/principals/{}/space",
                "ab".repeat(32)
            ))
        );
    }

    #[test]
    fn principal_storage_key_rejects_non_fp_principals() {
        let err = principal_storage_key("session:abc").unwrap_err();
        assert!(err.to_string().contains("fp:<64 lowercase hex>"));

        let err = principal_storage_key("fp:not-hex").unwrap_err();
        assert!(err.to_string().contains("fp:<64 lowercase hex>"));

        let err = principal_storage_key(&format!("fp:{}", "AB".repeat(32))).unwrap_err();
        assert!(err.to_string().contains("fp:<64 lowercase hex>"));
    }

    #[test]
    fn yaml_helpers_round_trip_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/config.yaml");

        let missing: Demo = read_yaml_or_default(&path).unwrap();
        assert_eq!(missing, Demo::default());

        write_yaml_atomic(&path, &Demo { value: "ok".into() }).unwrap();

        let loaded: Demo = read_yaml_or_default(&path).unwrap();
        assert_eq!(loaded.value, "ok");
        assert!(!path.with_extension("tmp~").exists());
    }

    #[tokio::test]
    async fn principal_store_resolves_principal_and_serializes_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = FixedResolver {
            root: tmp.path().to_path_buf(),
        };
        let store = PrincipalStore::resolve_with(&resolver, "fp:test").unwrap();
        let path = store.paths().ryeos_config();

        let missing: Demo = store.load_yaml(&path).unwrap();
        assert_eq!(missing, Demo::default());

        let locked = PrincipalStore::locked_with(&resolver, "fp:test")
            .await
            .expect("locked store should resolve through supplied resolver");
        locked
            .write_yaml(&path, &Demo { value: "ok".into() })
            .unwrap();
        let loaded: Demo = store.load_yaml(&path).unwrap();
        assert_eq!(loaded.value, "ok");
    }

    #[cfg(unix)]
    #[test]
    fn yaml_helpers_write_private_files_and_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/config.yaml");

        write_yaml_atomic(&path, &Demo { value: "ok".into() }).unwrap();

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn yaml_helpers_make_ai_dir_chain_private() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join(".ai")
            .join("state")
            .join("ryeos-ui")
            .join("recent.yaml");

        write_yaml_atomic(&path, &Demo { value: "ok".into() }).unwrap();

        for dir in [".ai", ".ai/state", ".ai/state/ryeos-ui"] {
            let mode = std::fs::metadata(tmp.path().join(dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "{dir} should be private");
        }
    }

    #[cfg(unix)]
    #[test]
    fn yaml_helpers_make_hosted_principal_dir_chain_private() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join(".ai")
            .join("principals")
            .join("ab".repeat(32))
            .join("space")
            .join(".ai")
            .join("config")
            .join("ryeos-ui.yaml");

        write_yaml_atomic(&path, &Demo { value: "ok".into() }).unwrap();

        let principal_key = "ab".repeat(32);
        let dirs = vec![
            ".ai".to_string(),
            ".ai/principals".to_string(),
            format!(".ai/principals/{principal_key}"),
            format!(".ai/principals/{principal_key}/space"),
            format!(".ai/principals/{principal_key}/space/.ai"),
            format!(".ai/principals/{principal_key}/space/.ai/config"),
        ];
        for dir in dirs {
            let mode = std::fs::metadata(tmp.path().join(&dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "{dir} should be private");
        }
    }
}
