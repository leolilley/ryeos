//! Protected host-runtime authority shared by native and external supervisors.
//!
//! This binds one exact node/account to Lillux's opaque process-scope
//! configuration. Native lifecycle state and externally mounted launch
//! documents are transports for this same contract; neither node policy nor a
//! workload may author it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use lillux::{InheritedReadonlyDocument, PinnedDirectory, PinnedDirectoryIdentity};
use serde::{Deserialize, Serialize};

pub const HOST_RUNTIME_BINDING_SCHEMA_VERSION: u32 = 1;
pub const HOST_RUNTIME_AUTHORITY_FD_ENV: &str = "RYEOS_HOST_RUNTIME_AUTHORITY_FD";
const MAX_HOST_RUNTIME_DOCUMENT_BYTES: u64 = 64 * 1024;

/// Administrator-selected host runtime for one exact node incarnation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRuntimeBinding {
    pub schema_version: u32,
    pub app_root: PathBuf,
    pub app_root_identity: PinnedDirectoryIdentity,
    pub node_fingerprint: String,
    pub account: lillux::ControllerAccount,
    pub process_scopes: lillux::ProcessScopeConfiguration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci_lifecycle: Option<lillux::OciLifecycleGeneration>,
}

impl HostRuntimeBinding {
    pub fn capture(
        app_root: &PinnedDirectory,
        node_fingerprint: String,
        account: lillux::ControllerAccount,
        process_scopes: lillux::ProcessScopeConfiguration,
    ) -> Result<Self> {
        let binding = Self {
            schema_version: HOST_RUNTIME_BINDING_SCHEMA_VERSION,
            app_root: app_root.path().to_path_buf(),
            app_root_identity: app_root.identity()?,
            node_fingerprint,
            account,
            process_scopes,
            oci_lifecycle: None,
        };
        binding.validate(app_root.path())?;
        Ok(binding)
    }

    /// Validate the complete protected binding and return the exact pinned app
    /// root. This never creates, repairs or reinterprets node state.
    pub fn validate(&self, expected_root: &Path) -> Result<PinnedDirectory> {
        if self.schema_version != HOST_RUNTIME_BINDING_SCHEMA_VERSION
            || self.app_root != expected_root
            || !self.app_root.is_absolute()
            || self.app_root.parent().is_none()
        {
            bail!("host runtime has a wrong epoch or app root");
        }
        self.account.validate().map_err(anyhow::Error::msg)?;
        self.process_scopes.validate().map_err(anyhow::Error::msg)?;
        if let Some(lifecycle) = &self.oci_lifecycle {
            self.process_scopes
                .require_oci_generation(lifecycle)
                .map_err(anyhow::Error::msg)?;
        }
        if !lillux::valid_hash(&self.node_fingerprint) {
            bail!("host runtime has an invalid node public identity");
        }
        let app_root =
            PinnedDirectory::open(expected_root)?.context("host-runtime app root is absent")?;
        self.account.require_directory_owner(&app_root)?;
        if app_root.identity()? != self.app_root_identity {
            bail!("host-runtime app root has been replaced");
        }
        let identity_path =
            Path::new(ryeos_engine::AI_DIR).join("node/identity/public-identity.json");
        let identity_file = app_root
            .open_pinned_regular_descendant(&identity_path, false)?
            .context("host-runtime node public identity is absent")?;
        let observation = identity_file.observation()?;
        let identity: ryeos_app::identity::PublicIdentityDoc = serde_json::from_slice(
            &identity_file.read_stable_bounded(&observation, MAX_HOST_RUNTIME_DOCUMENT_BYTES)?,
        )?;
        if identity.verified_fingerprint()? != self.node_fingerprint {
            bail!("host-runtime node identity changed");
        }
        Ok(app_root)
    }

    pub fn capture_oci(
        app_root: &PinnedDirectory,
        node_fingerprint: String,
        account: lillux::ControllerAccount,
        process_scopes: lillux::ProcessScopeConfiguration,
        oci_lifecycle: lillux::OciLifecycleGeneration,
    ) -> Result<Self> {
        let mut binding = Self::capture(app_root, node_fingerprint, account, process_scopes)?;
        binding.oci_lifecycle = Some(oci_lifecycle);
        binding.validate(app_root.path())?;
        Ok(binding)
    }

    pub fn verify_loaded_node_identity(
        &self,
        identity: &ryeos_app::identity::NodeIdentity,
    ) -> Result<()> {
        self.account.require_current_process()?;
        self.validate(&self.app_root)?;
        if identity.fingerprint() != self.node_fingerprint {
            bail!("loaded node signing identity differs from host-runtime authority");
        }
        Ok(())
    }

    /// Stable, non-secret identity of this complete protected binding. Status
    /// surfaces may publish the digest, never its host paths or native control
    /// configuration.
    pub fn identity_digest(&self) -> Result<String> {
        let value = serde_json::to_value(self).context("serialize host-runtime identity")?;
        let canonical = lillux::canonical_json(&value).map_err(anyhow::Error::msg)?;
        Ok(format!(
            "sha256:{}",
            lillux::sha256_hex(canonical.as_bytes())
        ))
    }

    pub fn open_process_scope_provider(
        &self,
    ) -> Result<std::sync::Arc<lillux::ProcessScopeProvider>> {
        self.account.require_current_process()?;
        self.validate(&self.app_root)?;
        lillux::ProcessScopeProvider::open(&self.process_scopes)
            .map(std::sync::Arc::new)
            .map_err(anyhow::Error::msg)
    }
}

/// Child-side external authority. The exact document remains descriptor-owned
/// until its bytes and complete node binding have been validated.
pub struct ExternalHostRuntime {
    binding: HostRuntimeBinding,
}

impl ExternalHostRuntime {
    pub fn take_from_environment() -> Result<Option<Self>> {
        let Some(document) =
            InheritedReadonlyDocument::take_from_environment(HOST_RUNTIME_AUTHORITY_FD_ENV)
                .map_err(anyhow::Error::msg)?
        else {
            return Ok(None);
        };
        let bytes = document.read_stable_bounded(MAX_HOST_RUNTIME_DOCUMENT_BYTES)?;
        let binding: HostRuntimeBinding =
            serde_json::from_slice(&bytes).context("parse inherited host-runtime authority")?;
        // The descriptor is intentionally consumed after the stable bounded
        // read. Every durable fact is revalidated against live pinned owners.
        drop(document);
        Ok(Some(Self { binding }))
    }

    pub fn binding(&self) -> &HostRuntimeBinding {
        &self.binding
    }
}

/// Privileged external-supervisor entry. The administrator-selected pathname
/// is opened only through a root-owned hierarchy, converted to descriptor
/// authority, and never passed to the unprivileged daemon.
pub fn exec_external_controller(
    binding_path: &Path,
    daemon_executable: &Path,
) -> Result<std::convert::Infallible> {
    lillux::require_administrator()?;
    if !binding_path.is_absolute() {
        bail!("external host-runtime binding requires an absolute path");
    }
    let parent = PinnedDirectory::open_owned_hierarchy(
        binding_path
            .parent()
            .context("external host-runtime binding has no parent")?,
        0,
    )?
    .context("external host-runtime binding directory is absent")?;
    let file = parent
        .open_pinned_regular(
            binding_path
                .file_name()
                .context("external host-runtime binding has no filename")?,
            false,
        )?
        .context("external host-runtime binding is absent")?;
    file.require_owner(0)?;
    // Parse and pass the same immutable snapshot. A replacement or mutation of
    // the administrator file can affect a later launch, never split authority
    // between this privileged bootstrap and its unprivileged child.
    let document =
        InheritedReadonlyDocument::from_administrator_file(&file, MAX_HOST_RUNTIME_DOCUMENT_BYTES)?;
    let binding: HostRuntimeBinding =
        serde_json::from_slice(&document.read_stable_bounded(MAX_HOST_RUNTIME_DOCUMENT_BYTES)?)
            .context("parse external host-runtime binding")?;
    let app_root = binding.validate(&binding.app_root)?;

    let executable_parent = PinnedDirectory::open_owned_hierarchy(
        daemon_executable
            .parent()
            .context("external controller daemon image has no parent")?,
        0,
    )?
    .context("external controller daemon image directory is absent")?;
    let executable = executable_parent
        .open_pinned_regular(
            daemon_executable
                .file_name()
                .context("external controller daemon image has no filename")?,
            false,
        )?
        .context("external controller daemon image is absent")?;
    executable.require_owner(0)?;
    executable.require_executable()?;
    let root = binding
        .app_root
        .to_str()
        .context("external host-runtime app root is not UTF-8")?;
    binding
        .process_scopes
        .exec_controller_with_inherited_document(
            &binding.account,
            &executable,
            &["--app-root".to_owned(), root.to_owned()],
            &app_root,
            &[],
            document,
            HOST_RUNTIME_AUTHORITY_FD_ENV,
        )
        .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;

    fn binding_value() -> serde_json::Value {
        serde_json::json!({
            "schema_version": HOST_RUNTIME_BINDING_SCHEMA_VERSION,
            "app_root": "/node",
            "app_root_identity": {"containing_device": 1, "inode": 2},
            "node_fingerprint": "a".repeat(64),
            "account": {"implementation": "unix", "uid": 1000, "gid": 1000},
            "process_scopes": {
                "version": 3,
                "backend": {"implementation": "linux_cgroup_v2", "parent": "/scope"}
            }
        })
    }

    #[test]
    fn host_runtime_contract_is_closed_and_clean_cut() {
        let binding: HostRuntimeBinding = serde_json::from_value(binding_value()).unwrap();
        assert!(binding.validate(Path::new("/node")).is_err());

        let mut predecessor = binding_value();
        predecessor["schema_version"] = 0.into();
        let predecessor: HostRuntimeBinding = serde_json::from_value(predecessor).unwrap();
        assert!(
            format!(
                "{:#}",
                predecessor.validate(Path::new("/node")).unwrap_err()
            )
            .contains("wrong epoch")
        );

        let mut unknown = binding_value();
        unknown["scope_path"] = "/ambient".into();
        assert!(serde_json::from_value::<HostRuntimeBinding>(unknown).is_err());
    }

    #[test]
    fn host_runtime_identity_covers_the_complete_binding() {
        let first: HostRuntimeBinding = serde_json::from_value(binding_value()).unwrap();
        let mut changed = binding_value();
        changed["app_root_identity"]["inode"] = 3.into();
        let changed: HostRuntimeBinding = serde_json::from_value(changed).unwrap();
        assert_ne!(
            first.identity_digest().unwrap(),
            changed.identity_digest().unwrap()
        );
        assert_eq!(first.identity_digest().unwrap().len(), "sha256:".len() + 64);
    }

    #[cfg(unix)]
    fn binding_for_directory(path: &Path, uid: u32, gid: u32) -> HostRuntimeBinding {
        let directory = PinnedDirectory::open(path).unwrap().unwrap();
        serde_json::from_value(serde_json::json!({
            "schema_version": HOST_RUNTIME_BINDING_SCHEMA_VERSION,
            "app_root": path,
            "app_root_identity": directory.identity().unwrap(),
            "node_fingerprint": "a".repeat(64),
            "account": {"implementation": "unix", "uid": uid, "gid": gid},
            "process_scopes": {
                "version": 3,
                "backend": {"implementation": "linux_cgroup_v2", "parent": "/scope"}
            }
        }))
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn replaced_app_root_is_not_the_bound_runtime_generation() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("node");
        std::fs::create_dir(&root).unwrap();
        let metadata = std::fs::metadata(&root).unwrap();
        let binding = binding_for_directory(&root, metadata.uid(), metadata.gid());
        std::fs::rename(&root, parent.path().join("old-node")).unwrap();
        std::fs::create_dir(&root).unwrap();
        let error = binding.validate(&root).unwrap_err();
        assert!(format!("{error:#}").contains("app root has been replaced"));
    }

    #[cfg(unix)]
    #[test]
    fn rootless_uid_mapping_mismatch_is_refused() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("node");
        std::fs::create_dir(&root).unwrap();
        let metadata = std::fs::metadata(&root).unwrap();
        let binding = binding_for_directory(&root, metadata.uid().wrapping_add(1), metadata.gid());
        let error = binding.validate(&root).unwrap_err();
        assert!(format!("{error:#}").contains("owner"));
    }
}
