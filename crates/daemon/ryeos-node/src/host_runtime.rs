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

pub const HOST_RUNTIME_BINDING_SCHEMA_VERSION: u32 = 2;
/// Provider id of a binding the native host service installed and placed.
pub const HOST_SERVICE_PROVIDER: &str = "host_service";
/// Provider id of a binding an external supervisor prepared and handed in,
/// without an OCI lifecycle.
pub const EXTERNAL_CONTROLLER_PROVIDER: &str = "external_controller";
/// Provider id of a binding an OCI hook prepared from the container's own
/// lifecycle cgroup.
pub const EXTERNAL_CONTROLLER_OCI_PROVIDER: &str = "external_controller:oci";

/// One selected host runtime and the transport that delivered it. The
/// capability report names the transport as the provider of every per-scope
/// proof, so a floor can say which transports it accepts.
#[derive(Debug, Clone, Copy)]
pub struct SelectedHostRuntime<'a> {
    pub binding: &'a HostRuntimeBinding,
    pub provider: &'static str,
}
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
    pub node_resources: lillux::ProcessScopeResourceLimits,
    pub process_scopes: lillux::ProcessScopeConfiguration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci_lifecycle: Option<lillux::OciLifecycleGeneration>,
}

impl HostRuntimeBinding {
    /// Host-adapter publication for a path observed through an OCI init mount
    /// namespace. The stored path is the container spelling; physical identity
    /// and node identity come from the already-pinned observed directory.
    pub fn capture_oci_observed(
        observed_app_root: &PinnedDirectory,
        runtime_app_root: PathBuf,
        account: lillux::ControllerAccount,
        node_resources: lillux::ProcessScopeResourceLimits,
        process_scopes: lillux::ProcessScopeConfiguration,
        oci_lifecycle: lillux::OciLifecycleGeneration,
    ) -> Result<Self> {
        account.validate().map_err(anyhow::Error::msg)?;
        node_resources.validate().map_err(anyhow::Error::msg)?;
        account.require_directory_owner(observed_app_root)?;
        process_scopes.validate().map_err(anyhow::Error::msg)?;
        oci_lifecycle.validate().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            node_resources == oci_lifecycle.node_resources(),
            "host runtime node envelope differs from OCI generation"
        );
        process_scopes
            .require_oci_generation_as_administrator(&account, &oci_lifecycle)
            .map_err(anyhow::Error::msg)?;
        if !runtime_app_root.is_absolute() || runtime_app_root.parent().is_none() {
            bail!("OCI runtime app root must be an absolute non-root path");
        }
        let identity_path =
            Path::new(ryeos_engine::AI_DIR).join("node/identity/public-identity.json");
        let identity_file = observed_app_root
            .open_pinned_regular_descendant(&identity_path, false)?
            .context("OCI app root has no node public identity")?;
        let observation = identity_file.observation()?;
        let identity: ryeos_node_runtime::identity::PublicIdentityDoc = serde_json::from_slice(
            &identity_file.read_stable_bounded(&observation, MAX_HOST_RUNTIME_DOCUMENT_BYTES)?,
        )?;
        Ok(Self {
            schema_version: HOST_RUNTIME_BINDING_SCHEMA_VERSION,
            app_root: runtime_app_root,
            app_root_identity: observed_app_root.identity()?,
            node_fingerprint: identity.verified_fingerprint()?,
            account,
            node_resources,
            process_scopes,
            oci_lifecycle: Some(oci_lifecycle),
        })
    }

    pub fn capture(
        app_root: &PinnedDirectory,
        node_fingerprint: String,
        account: lillux::ControllerAccount,
        node_resources: lillux::ProcessScopeResourceLimits,
        process_scopes: lillux::ProcessScopeConfiguration,
    ) -> Result<Self> {
        let binding = Self {
            schema_version: HOST_RUNTIME_BINDING_SCHEMA_VERSION,
            app_root: app_root.path().to_path_buf(),
            app_root_identity: app_root.identity()?,
            node_fingerprint,
            account,
            node_resources,
            process_scopes,
            oci_lifecycle: None,
        };
        binding.validate(app_root.path())?;
        Ok(binding)
    }

    /// Validate the complete protected binding and return the exact pinned app
    /// root. This never creates, repairs or reinterprets node state.
    pub fn validate(&self, expected_root: &Path) -> Result<PinnedDirectory> {
        let app_root = self.validate_common(expected_root)?;
        if let Some(lifecycle) = &self.oci_lifecycle {
            self.process_scopes
                .require_oci_generation_as_controller(&self.account, lifecycle)
                .map_err(anyhow::Error::msg)?;
        }
        Ok(app_root)
    }

    /// Privileged bootstrap validation. Root observes the selected controller
    /// account and exact prepared delegation; it never enters the ordinary
    /// controller-side provider opener under a false current-UID assumption.
    pub fn validate_as_administrator(&self, expected_root: &Path) -> Result<PinnedDirectory> {
        lillux::require_administrator()?;
        let app_root = self.validate_common(expected_root)?;
        if let Some(lifecycle) = &self.oci_lifecycle {
            self.process_scopes
                .require_oci_generation_as_administrator(&self.account, lifecycle)
                .map_err(anyhow::Error::msg)?;
        }
        Ok(app_root)
    }

    fn validate_common(&self, expected_root: &Path) -> Result<PinnedDirectory> {
        if self.schema_version != HOST_RUNTIME_BINDING_SCHEMA_VERSION
            || self.app_root != expected_root
            || !self.app_root.is_absolute()
            || self.app_root.parent().is_none()
        {
            bail!("host runtime has a wrong epoch or app root");
        }
        self.account.validate().map_err(anyhow::Error::msg)?;
        self.node_resources.validate().map_err(anyhow::Error::msg)?;
        if let Some(lifecycle) = &self.oci_lifecycle {
            lifecycle.validate().map_err(anyhow::Error::msg)?;
            anyhow::ensure!(
                self.node_resources == lifecycle.node_resources(),
                "host runtime node envelope differs from OCI generation"
            );
        }
        self.process_scopes.validate().map_err(anyhow::Error::msg)?;
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
        let identity: ryeos_node_runtime::identity::PublicIdentityDoc = serde_json::from_slice(
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
        node_resources: lillux::ProcessScopeResourceLimits,
        process_scopes: lillux::ProcessScopeConfiguration,
        oci_lifecycle: lillux::OciLifecycleGeneration,
    ) -> Result<Self> {
        let mut binding = Self::capture(
            app_root,
            node_fingerprint,
            account,
            node_resources,
            process_scopes,
        )?;
        binding.oci_lifecycle = Some(oci_lifecycle);
        binding.validate_as_administrator(app_root.path())?;
        Ok(binding)
    }

    pub fn verify_loaded_node_identity(
        &self,
        identity: &ryeos_node_runtime::identity::NodeIdentity,
    ) -> Result<()> {
        self.account.require_current_process()?;
        self.validate(&self.app_root)?;
        if identity.fingerprint() != self.node_fingerprint {
            bail!("loaded node signing identity differs from host-runtime authority");
        }
        Ok(())
    }

    /// The external route this binding came by, as the capability report
    /// names it: through an OCI hook when it carries an OCI lifecycle, else
    /// an externally prepared controller leaf. A binding the native host
    /// service installed is named by its transport, not by the binding.
    pub fn provider_id(&self) -> &'static str {
        if self.oci_lifecycle.is_some() {
            EXTERNAL_CONTROLLER_OCI_PROVIDER
        } else {
            EXTERNAL_CONTROLLER_PROVIDER
        }
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

    /// A supervised claim is placement, not association: refuse unless this
    /// process runs as the bound account inside the exact controller leaf the
    /// host supervisor places it in. Call before any lock, debit or allowance.
    pub fn require_current_process_placed(&self) -> Result<()> {
        // Placement first: for a direct launch it is the precise cause.
        self.process_scopes
            .require_current_process_in_controller(&self.account)
            .map_err(anyhow::Error::msg)
            .with_context(|| {
                format!(
                    "supervised route refused for app root {}",
                    self.app_root.display()
                )
            })?;
        self.account.require_current_process()
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
    let app_root = binding.validate_as_administrator(&binding.app_root)?;

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
    let arguments = ["--app-root".to_owned(), root.to_owned()];
    match &binding.oci_lifecycle {
        Some(lifecycle) => binding
            .process_scopes
            .exec_prepared_oci_controller_with_inherited_document(
                &binding.account,
                binding.node_resources,
                lifecycle,
                &executable,
                &arguments,
                &app_root,
                &[],
                document,
                HOST_RUNTIME_AUTHORITY_FD_ENV,
            )
            .map_err(anyhow::Error::msg),
        None => binding
            .process_scopes
            .exec_controller_with_inherited_document(
                &binding.account,
                binding.node_resources,
                &executable,
                &arguments,
                &app_root,
                &[],
                document,
                HOST_RUNTIME_AUTHORITY_FD_ENV,
            )
            .map_err(anyhow::Error::msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding_value() -> serde_json::Value {
        serde_json::json!({
            "schema_version": HOST_RUNTIME_BINDING_SCHEMA_VERSION,
            "app_root": "/node",
            "app_root_identity": {"containing_device": 1, "inode": 2},
            "node_fingerprint": "a".repeat(64),
            "account": {"implementation": "unix", "uid": 1000, "gid": 1000},
            "node_resources": {"maximum_memory_bytes": 268435456, "maximum_processes": 64},
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

    #[test]
    fn node_envelope_is_required_closed_and_part_of_authority_identity() {
        let original = binding_value();
        let first: HostRuntimeBinding = serde_json::from_value(original.clone()).unwrap();
        let mut absent = original.clone();
        absent.as_object_mut().unwrap().remove("node_resources");
        assert!(serde_json::from_value::<HostRuntimeBinding>(absent).is_err());
        let mut unknown = original.clone();
        unknown["node_resources"]["ambient_allowance"] = true.into();
        assert!(serde_json::from_value::<HostRuntimeBinding>(unknown).is_err());
        let mut changed = original;
        changed["node_resources"]["maximum_memory_bytes"] = 536870912_u64.into();
        let changed: HostRuntimeBinding = serde_json::from_value(changed).unwrap();
        assert_ne!(
            first.identity_digest().unwrap(),
            changed.identity_digest().unwrap()
        );
        let mut no_capacity = first.node_resources;
        no_capacity.maximum_memory_bytes = 0;
        assert!(no_capacity.validate().is_err());
        no_capacity = first.node_resources;
        no_capacity.maximum_processes = 0;
        assert!(no_capacity.validate().is_err());
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
            "node_resources": {"maximum_memory_bytes": 268435456, "maximum_processes": 64},
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
        let (uid, gid) = lillux::path_owner_ids(&root).unwrap();
        let binding = binding_for_directory(&root, uid, gid);
        std::fs::rename(&root, parent.path().join("old-node")).unwrap();
        std::fs::create_dir(&root).unwrap();
        let error = binding.validate(&root).unwrap_err();
        assert!(format!("{error:#}").contains("app root has been replaced"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn direct_process_on_an_associated_root_is_not_supervised() {
        // This test process stands in for a directly launched daemon: it is
        // the bound account, but no host supervisor placed it.
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("node");
        std::fs::create_dir(&root).unwrap();
        let (uid, gid) = lillux::path_owner_ids(&root).unwrap();
        let mut binding = binding_for_directory(&root, uid, gid);
        binding.process_scopes = serde_json::from_value(serde_json::json!({
            "version": 3,
            "backend": {
                "implementation": "linux_cgroup_v2",
                "parent": "/sys/fs/cgroup/lillux-host-services/ryeos-direct-fixture"
            }
        }))
        .unwrap();
        let error = format!(
            "{:#}",
            binding.require_current_process_placed().unwrap_err()
        );
        assert!(error.contains("supervised route refused"), "{error}");
        assert!(error.contains("ryeos-direct-fixture/controller"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rootless_uid_mapping_mismatch_is_refused() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("node");
        std::fs::create_dir(&root).unwrap();
        let (uid, gid) = lillux::path_owner_ids(&root).unwrap();
        let binding = binding_for_directory(&root, uid.wrapping_add(1), gid);
        let error = binding.validate(&root).unwrap_err();
        assert!(format!("{error:#}").contains("owner"));
    }
}
