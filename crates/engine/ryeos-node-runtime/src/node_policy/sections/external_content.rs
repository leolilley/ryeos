//! Node-owned external-content import roots and resource policy.
//!
//! This section is system/state-only. Bundle and project content cannot add a
//! host path, loosen capture exclusions, or increase a node storage budget.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_policy::{ErasedNodePolicy, NodePolicyContext, NodePolicySection, TypedNodePolicy};

pub const SECTION_NAME: &str = "external_content";

/// Prefix a declared root path may start with to name a directory under the
/// node's app root. It must be followed by `/` and a relative normalized path.
pub const APP_ROOT_PLACEHOLDER: &str = "{app_root}";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentImportPolicyRecord {
    pub schema: u32,
    pub roots: BTreeMap<String, ExternalContentImportRoot>,
    /// Roots a publisher names by path. Initialization measures each one and
    /// moves it into `roots` with its filesystem identity.
    #[serde(default)]
    pub declared_roots: BTreeMap<String, ExternalContentImportRootDeclaration>,
    pub limits: ExternalContentImportLimits,
    pub managed_activation: ManagedExternalContentPolicy,
}

/// An import root named by path before its filesystem identity is measured.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentImportRootDeclaration {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentImportRoot {
    pub path: PathBuf,
    /// The measured root directory, as the filesystem's own identity plus the
    /// inode, so it survives the filesystem being re-attached.
    pub root_identity: lillux::PinnedDirectoryIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentImportLimits {
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub store_budget_bytes: u64,
    pub minimum_free_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedExternalContentActivationPolicy {
    pub allow_online: bool,
    pub allowed_https_hosts: Vec<String>,
    pub max_redirects: usize,
    pub max_archives: usize,
    pub max_compressed_bytes: u64,
    pub max_expanded_bytes: u64,
    pub max_members: usize,
    pub max_member_bytes: u64,
    /// V1 deliberately serializes managed activation. The field is retained
    /// in the node-owned contract so a future protocol can widen it without
    /// conflating that change with bundle authority.
    pub max_concurrent_activations: usize,
    pub cache_budget_bytes: u64,
    pub store_budget_bytes: u64,
    pub minimum_free_bytes: u64,
    pub max_attempts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedExternalContentPolicy {
    pub enabled: bool,
    pub limits: Option<ManagedExternalContentActivationPolicy>,
}

impl ManagedExternalContentPolicy {
    pub fn require_enabled(&self) -> anyhow::Result<&ManagedExternalContentActivationPolicy> {
        if !self.enabled {
            bail!("managed external-content activation is disabled by node policy");
        }
        self.limits
            .as_ref()
            .context("enabled managed external-content activation has no limits")
    }

    fn validate(&self) -> anyhow::Result<()> {
        match (self.enabled, self.limits.as_ref()) {
            (true, Some(limits)) => limits.validate(),
            (false, None) => Ok(()),
            (true, None) => bail!("enabled managed activation requires exact node limits"),
            (false, Some(_)) => bail!("disabled managed activation must not retain latent limits"),
        }
    }
}

pub struct ExternalContentImportPolicySection;

impl TypedNodePolicy for ExternalContentImportPolicyRecord {
    const SECTION_NAME: &'static str = SECTION_NAME;
}

impl NodePolicySection for ExternalContentImportPolicySection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn parse(
        &self,
        _context: &NodePolicyContext,
        body: &Value,
    ) -> anyhow::Result<Arc<dyn ErasedNodePolicy>> {
        let record: ExternalContentImportPolicyRecord =
            serde_json::from_value(body.clone()).context("parse external-content import policy")?;
        record.validate()?;
        Ok(Arc::new(record))
    }
}

impl ExternalContentImportPolicyRecord {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != 1 {
            bail!("external-content import policy schema is not current");
        }
        for (name, root) in &self.roots {
            validate_root_name(name)?;
            if !root.path.is_absolute()
                || root.path.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::CurDir | Component::Prefix(_)
                    )
                })
            {
                bail!("external-content import root `{name}` must be an absolute normalized path");
            }
            if !root.root_identity.names_an_inode() || !root.root_identity.filesystem().is_named() {
                bail!("external-content import root `{name}` has an invalid filesystem identity");
            }
        }
        for (name, declaration) in &self.declared_roots {
            validate_root_name(name)?;
            declaration.validate(name)?;
        }
        self.limits.validate()?;
        self.managed_activation.validate()?;
        Ok(())
    }
}

impl ExternalContentImportRootDeclaration {
    fn validate(&self, name: &str) -> anyhow::Result<()> {
        match self.app_root_relative() {
            Some(relative) => {
                if relative.is_empty()
                    || relative
                        .split('/')
                        .any(|part| part.is_empty() || part == "." || part == "..")
                {
                    bail!(
                        "declared external-content import root `{name}` must name a relative \
                         normalized path after `{APP_ROOT_PLACEHOLDER}/`"
                    );
                }
                Ok(())
            }
            None => {
                let path = Path::new(&self.path);
                if !path.is_absolute()
                    || self.path.contains("//")
                    || path.components().any(|component| {
                        matches!(
                            component,
                            Component::ParentDir | Component::CurDir | Component::Prefix(_)
                        )
                    })
                {
                    bail!(
                        "declared external-content import root `{name}` must be an absolute \
                         normalized path or start with `{APP_ROOT_PLACEHOLDER}/`"
                    );
                }
                Ok(())
            }
        }
    }

    /// The part after `{app_root}/`, when the declaration uses the placeholder.
    fn app_root_relative(&self) -> Option<&str> {
        self.path
            .strip_prefix(APP_ROOT_PLACEHOLDER)
            .and_then(|rest| rest.strip_prefix('/'))
    }

    fn resolve(&self, app_root: &Path) -> PathBuf {
        match self.app_root_relative() {
            Some(relative) => app_root.join(relative),
            None => PathBuf::from(&self.path),
        }
    }
}

/// Measure every declared import root that is not yet admitted and return the
/// policy body with the measured identities recorded in `roots`.
///
/// A placeholder path is created under `app_root` with private permissions,
/// one pinned component at a time. An absolute path must already exist. A name
/// already admitted for the same path is left alone; a different path is
/// refused.
pub fn admit_declared_roots(body: &Value, app_root: &Path) -> anyhow::Result<Value> {
    if !app_root.is_absolute() {
        bail!("external-content import roots are admitted against an absolute app root");
    }
    let mut record: ExternalContentImportPolicyRecord = serde_json::from_value(body.clone())
        .context("parse external-content import policy for root admission")?;
    record.validate()?;
    let declarations = record.declared_roots.clone();
    for (name, declaration) in &declarations {
        let resolved = declaration.resolve(app_root);
        if let Some(admitted) = record.roots.get(name) {
            if admitted.path != resolved {
                bail!(
                    "external-content import root `{name}` is admitted at {} but declared at {}",
                    admitted.path.display(),
                    resolved.display()
                );
            }
            continue;
        }
        let directory = match declaration.app_root_relative() {
            Some(relative) => {
                let mut directory = lillux::PinnedDirectory::open(app_root)
                    .with_context(|| format!("open app root {}", app_root.display()))?
                    .with_context(|| format!("app root {} is absent", app_root.display()))?;
                for component in Path::new(relative).components() {
                    directory = directory
                        .open_or_create_child(component.as_os_str(), 0o700)
                        .with_context(|| {
                            format!(
                                "create external-content import root `{name}` at {}",
                                resolved.display()
                            )
                        })?;
                }
                directory
            }
            None => lillux::PinnedDirectory::open(&resolved)
                .with_context(|| {
                    format!(
                        "open external-content import root `{name}` at {}",
                        resolved.display()
                    )
                })?
                .with_context(|| {
                    format!(
                        "external-content import root `{name}` names an absent directory {}",
                        resolved.display()
                    )
                })?,
        };
        let root_identity = directory
            .identity()
            .with_context(|| format!("measure external-content import root `{name}`"))?;
        record.roots.insert(
            name.clone(),
            ExternalContentImportRoot {
                path: resolved,
                root_identity,
            },
        );
    }
    record.validate()?;
    serde_json::to_value(&record).context("serialize admitted external-content import policy")
}

impl ExternalContentImportLimits {
    pub fn validate(&self) -> anyhow::Result<()> {
        let limits = self;
        if limits.max_depth == 0 || limits.max_depth > 256 {
            bail!("external-content import max_depth is outside the supported range");
        }
        if limits.max_entries == 0
            || limits.max_entries > ryeos_state::objects::MAX_EXTERNAL_REALIZATION_ENTRIES
        {
            bail!("external-content import max_entries is outside the manifest bound");
        }
        if limits.max_file_bytes == 0
            || limits.max_file_bytes > ryeos_state::objects::MAX_LARGE_CONTENT_FILE_BYTES
        {
            bail!("external-content import max_file_bytes is outside the manifest bound");
        }
        if limits.max_total_bytes == 0
            || limits.max_total_bytes > ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES
            || limits.max_file_bytes > limits.max_total_bytes
        {
            bail!("external-content import max_total_bytes is incoherent");
        }
        if limits.store_budget_bytes == 0
            || limits.max_total_bytes > limits.store_budget_bytes
            || limits.minimum_free_bytes == 0
        {
            bail!("external-content import storage budget is incoherent");
        }
        Ok(())
    }
}

impl ManagedExternalContentActivationPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.allowed_https_hosts.len() > 64 {
            bail!("managed external-content host allowlist exceeds 64 entries");
        }
        let mut seen = std::collections::BTreeSet::new();
        for host in &self.allowed_https_hosts {
            if host.is_empty()
                || host.len() > 253
                || host.bytes().any(|byte| {
                    !(byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'-'))
                })
                || host.starts_with('.')
                || host.ends_with('.')
                || host.contains("..")
                || !seen.insert(host)
            {
                bail!("managed external-content HTTPS host is not canonical");
            }
        }
        if self.allow_online && self.allowed_https_hosts.is_empty() {
            bail!("online managed external-content activation requires an HTTPS host allowlist");
        }
        if self.max_redirects > 4 {
            bail!("managed external-content max_redirects exceeds 4");
        }
        if self.max_archives == 0 || self.max_archives > 8 {
            bail!("managed external-content max_archives is outside 1..=8");
        }
        let maximum_archive_entries =
            (ryeos_state::objects::MAX_EXTERNAL_REALIZATION_ENTRIES + 1).saturating_mul(8);
        if self.max_members == 0 || self.max_members > maximum_archive_entries {
            bail!("managed external-content max_members is outside 1..={maximum_archive_entries}");
        }
        if self.max_compressed_bytes == 0
            || self.max_expanded_bytes == 0
            || self.max_member_bytes == 0
            || self.max_compressed_bytes > self.cache_budget_bytes
            || self.max_member_bytes > self.max_expanded_bytes
            || self.max_expanded_bytes
                > ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES.saturating_mul(8)
        {
            bail!("managed external-content byte ceilings are incoherent");
        }
        if self.cache_budget_bytes == 0
            || self.store_budget_bytes == 0
            || self.max_expanded_bytes > self.store_budget_bytes
            || self.minimum_free_bytes == 0
        {
            bail!("managed external-content storage reserve is incoherent");
        }
        if self.max_concurrent_activations != 1 {
            bail!("managed external-content v1 requires exactly one concurrent activation");
        }
        if self.max_attempts == 0 || self.max_attempts > 16 {
            bail!("managed external-content max_attempts is outside 1..=16");
        }
        Ok(())
    }
}

pub fn validate_root_name(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("external-content import root name `{value}` is invalid");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn policy_rejects_relative_roots_and_unbounded_limits() {
        let policy = ExternalContentImportPolicyRecord {
            schema: 1,
            roots: BTreeMap::from([(
                "source".to_owned(),
                ExternalContentImportRoot {
                    path: PathBuf::from("relative"),
                    root_identity: serde_json::from_value(
                        serde_json::json!({"filesystem": {"device": 1}, "inode": 1}),
                    )
                    .unwrap(),
                },
            )]),
            declared_roots: BTreeMap::new(),
            limits: ExternalContentImportLimits {
                max_depth: 8,
                max_entries: 8,
                max_file_bytes: 1024,
                max_total_bytes: 1024,
                store_budget_bytes: 2048,
                minimum_free_bytes: 1024,
            },
            managed_activation: ManagedExternalContentPolicy {
                enabled: false,
                limits: None,
            },
        };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn policy_requires_an_exact_root_filesystem_identity() {
        let value = serde_json::json!({
            "schema": 1,
            "roots": {
                "source": {"path": "/srv/source", "root_identity": {"filesystem": {"device": 7}}}
            },
            "limits": {
                "max_depth": 8,
                "max_entries": 8,
                "max_file_bytes": 1024,
                "max_total_bytes": 1024,
                "store_budget_bytes": 2048,
                "minimum_free_bytes": 1024
            },
            "managed_activation": {"enabled": false, "limits": null}
        });
        assert!(serde_json::from_value::<ExternalContentImportPolicyRecord>(value).is_err());
    }

    #[test]
    fn managed_activation_requires_explicit_bounded_online_hosts() {
        let policy = ManagedExternalContentActivationPolicy {
            allow_online: true,
            allowed_https_hosts: Vec::new(),
            max_redirects: 0,
            max_archives: 1,
            max_compressed_bytes: 1024,
            max_expanded_bytes: 2048,
            max_members: 4,
            max_member_bytes: 1024,
            max_concurrent_activations: 1,
            cache_budget_bytes: 4096,
            store_budget_bytes: 8192,
            minimum_free_bytes: 1024,
            max_attempts: 3,
        };
        assert!(policy.validate().is_err());
        let mut admitted = policy;
        admitted.allowed_https_hosts = vec!["releases.example.test".to_owned()];
        admitted.validate().unwrap();
        admitted.max_concurrent_activations = 2;
        assert!(admitted.validate().is_err());
    }

    fn declared_body(declared: serde_json::Value) -> Value {
        serde_json::json!({
            "schema": 1,
            "roots": {},
            "declared_roots": declared,
            "limits": {
                "max_depth": 8,
                "max_entries": 8,
                "max_file_bytes": 1024,
                "max_total_bytes": 1024,
                "store_budget_bytes": 2048,
                "minimum_free_bytes": 1024
            },
            "managed_activation": {"enabled": false, "limits": null}
        })
    }

    fn admitted_root(value: &Value, name: &str) -> ExternalContentImportRoot {
        let record: ExternalContentImportPolicyRecord =
            serde_json::from_value(value.clone()).unwrap();
        record.roots.get(name).unwrap().clone()
    }

    #[test]
    fn placeholder_declaration_is_created_and_admitted_with_measured_identity() {
        let app_root = tempfile::tempdir().unwrap();
        let body =
            declared_body(serde_json::json!({"products": {"path": "{app_root}/imports/products"}}));
        let admitted = admit_declared_roots(&body, app_root.path()).unwrap();
        let root = admitted_root(&admitted, "products");
        let expected = app_root.path().join("imports/products");
        assert_eq!(root.path, expected);
        let pinned = lillux::PinnedDirectory::open(&expected).unwrap().unwrap();
        assert_eq!(root.root_identity, pinned.identity().unwrap());
        assert!(root.root_identity.names_an_inode());
    }

    #[test]
    fn declarations_with_parent_components_or_no_anchor_are_refused() {
        let app_root = tempfile::tempdir().unwrap();
        for path in [
            "{app_root}/../escape",
            "{app_root}/a/./b",
            "{app_root}/",
            "{app_root}",
            "{app_root}/a//b",
            "imports/products",
            "/srv/../etc",
        ] {
            let body = declared_body(serde_json::json!({"products": {"path": path}}));
            assert!(
                admit_declared_roots(&body, app_root.path()).is_err(),
                "{path} must be refused"
            );
        }
    }

    #[test]
    fn absolute_declaration_naming_an_absent_directory_is_refused() {
        let app_root = tempfile::tempdir().unwrap();
        let absent = app_root.path().join("absent");
        let body =
            declared_body(serde_json::json!({"products": {"path": absent.to_string_lossy()}}));
        let error = format!(
            "{:#}",
            admit_declared_roots(&body, app_root.path()).unwrap_err()
        );
        assert!(error.contains("absent directory"), "{error}");
    }

    #[test]
    fn absolute_declaration_naming_an_existing_directory_is_admitted() {
        let app_root = tempfile::tempdir().unwrap();
        let existing = app_root.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        let body =
            declared_body(serde_json::json!({"source": {"path": existing.to_string_lossy()}}));
        let admitted = admit_declared_roots(&body, app_root.path()).unwrap();
        assert_eq!(admitted_root(&admitted, "source").path, existing);
    }

    #[test]
    fn admitting_declared_roots_twice_is_idempotent() {
        let app_root = tempfile::tempdir().unwrap();
        let body =
            declared_body(serde_json::json!({"products": {"path": "{app_root}/imports/products"}}));
        let once = admit_declared_roots(&body, app_root.path()).unwrap();
        let twice = admit_declared_roots(&once, app_root.path()).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn declared_name_admitted_at_a_different_path_is_refused() {
        let app_root = tempfile::tempdir().unwrap();
        let body =
            declared_body(serde_json::json!({"products": {"path": "{app_root}/imports/products"}}));
        let mut admitted = admit_declared_roots(&body, app_root.path()).unwrap();
        admitted["roots"]["products"]["path"] =
            serde_json::json!(app_root.path().join("elsewhere").to_string_lossy());
        let error = format!(
            "{:#}",
            admit_declared_roots(&admitted, app_root.path()).unwrap_err()
        );
        assert!(error.contains("admitted at"), "{error}");
    }
}
