//! Immutable source/output partition and one workspace generation's capture.
//!
//! The partition is complete admission input, not an activation signal. A
//! capture owns only its exact result snapshot and captured root manifests.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::external_content::products::{
    MAX_PRODUCTS, ProductBounds, ProductDeclaration, ProductSource, ProductStorage,
    validate_binding_name, validate_canonical_unsuffixed_ref, validate_name,
};

pub const WORKSPACE_OUTPUT_CAPTURE_KIND: &str = "workspace_output_capture";
pub const WORKSPACE_OUTPUT_CAPTURE_SCHEMA: &str = "ryeos.workspace_output_capture.v1";
pub const WORKSPACE_OUTPUT_PARTITION_SCHEMA: &str = "ryeos.workspace_output_partition.v1";
pub const WORKSPACE_OUTPUT_CAPTURE_POLICY_SCHEMA: &str = "ryeos.workspace_output_capture_policy.v1";
pub const MAX_WORKSPACE_OUTPUT_CAPTURE_BYTES: usize = 64 * 1024;
pub const MAX_WORKSPACE_OUTPUT_PARTITION_BYTES: usize = 64 * 1024;

/// The indivisible source/output generation selected by workspace lifecycle
/// transitions. `output_capture_hash` is explicitly null for ordinary
/// workspaces and for the initial generation of an admitted output partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGenerationPair {
    pub snapshot_hash: String,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub output_capture_hash: Option<String>,
}

impl WorkspaceGenerationPair {
    pub fn validate(&self) -> anyhow::Result<()> {
        super::thread_snapshot::validate_canonical_hash(
            "workspace generation snapshot",
            &self.snapshot_hash,
        )?;
        if let Some(hash) = &self.output_capture_hash {
            super::thread_snapshot::validate_canonical_hash(
                "workspace generation output capture",
                hash,
            )?;
        }
        Ok(())
    }
}

/// Signed authored root before node ceilings are intersected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOutputRootDeclaration {
    pub name: String,
    pub path: String,
    pub storage: ProductStorage,
    pub bounds: ProductBounds,
}

impl WorkspaceOutputRootDeclaration {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_name(&self.name)?;
        validate_output_path(&self.path)?;
        self.bounds.validate()?;
        ensure_storage_bounds(self.storage, &self.bounds)
    }

    pub(crate) fn contains(&self, path: &str) -> bool {
        self.path == path || path_contains(&self.path, path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOutputRoot {
    pub name: String,
    pub path: String,
    pub storage: ProductStorage,
    pub declared_bounds: ProductBounds,
    pub effective_bounds: ProductBounds,
}

impl WorkspaceOutputRoot {
    pub fn admit(
        declaration: &WorkspaceOutputRootDeclaration,
        node_bounds: &crate::external_content::LargeContentCaptureBounds,
    ) -> anyhow::Result<Self> {
        declaration.validate()?;
        let effective = declaration.bounds.intersect(node_bounds)?;
        let result = Self {
            name: declaration.name.clone(),
            path: declaration.path.clone(),
            storage: declaration.storage,
            declared_bounds: declaration.bounds.clone(),
            effective_bounds: ProductBounds {
                maximum_entries: effective.max_entries,
                maximum_depth: effective.max_depth,
                maximum_file_bytes: effective.max_file_bytes,
                maximum_total_bytes: effective.max_total_bytes,
            },
        };
        result.validate_against(declaration, node_bounds)?;
        Ok(result)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_name(&self.name)?;
        validate_output_path(&self.path)?;
        self.declared_bounds.validate()?;
        self.effective_bounds.validate()?;
        if self.effective_bounds.maximum_entries > self.declared_bounds.maximum_entries
            || self.effective_bounds.maximum_depth > self.declared_bounds.maximum_depth
            || self.effective_bounds.maximum_file_bytes > self.declared_bounds.maximum_file_bytes
            || self.effective_bounds.maximum_total_bytes > self.declared_bounds.maximum_total_bytes
        {
            bail!("workspace output effective bounds widen authored bounds");
        }
        ensure_storage_bounds(self.storage, &self.declared_bounds)?;
        ensure_storage_bounds(self.storage, &self.effective_bounds)
    }

    pub fn validate_against(
        &self,
        declaration: &WorkspaceOutputRootDeclaration,
        node_bounds: &crate::external_content::LargeContentCaptureBounds,
    ) -> anyhow::Result<()> {
        self.validate()?;
        let expected = Self::admitted_value(declaration, node_bounds)?;
        if self != &expected {
            bail!("workspace output root is not the exact admitted node intersection");
        }
        Ok(())
    }

    fn admitted_value(
        declaration: &WorkspaceOutputRootDeclaration,
        node_bounds: &crate::external_content::LargeContentCaptureBounds,
    ) -> anyhow::Result<Self> {
        declaration.validate()?;
        let effective = declaration.bounds.intersect(node_bounds)?;
        Ok(Self {
            name: declaration.name.clone(),
            path: declaration.path.clone(),
            storage: declaration.storage,
            declared_bounds: declaration.bounds.clone(),
            effective_bounds: ProductBounds {
                maximum_entries: effective.max_entries,
                maximum_depth: effective.max_depth,
                maximum_file_bytes: effective.max_file_bytes,
                maximum_total_bytes: effective.max_total_bytes,
            },
        })
    }

    pub(crate) fn contains(&self, path: &str) -> bool {
        self.path == path || path_contains(&self.path, path)
    }
}

/// Complete finite output partition admitted with an exact recipe binding.
/// `products` is the exact subset selected as workspace-output products by the
/// eventual required ProductDeclaration source field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOutputPartition {
    pub schema: String,
    pub recipe_binding: String,
    pub recipe_ref: String,
    pub recipe_raw_content_digest: String,
    pub declarations_hash: String,
    pub project_snapshot_policy_hash: String,
    pub roots: Vec<WorkspaceOutputRoot>,
    pub products: Vec<ProductDeclaration>,
    pub partition_identity: String,
    pub capture_policy_digest: String,
}

impl WorkspaceOutputPartition {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != WORKSPACE_OUTPUT_PARTITION_SCHEMA {
            bail!("workspace output partition schema is not current");
        }
        validate_binding_name(&self.recipe_binding)?;
        validate_canonical_unsuffixed_ref("workspace output recipe", &self.recipe_ref)?;
        if !self.recipe_ref.starts_with("config:") {
            bail!("workspace output recipe must be a Config ref");
        }
        for (label, hash) in [
            (
                "workspace output recipe raw content",
                &self.recipe_raw_content_digest,
            ),
            ("workspace output declarations", &self.declarations_hash),
            (
                "workspace output project snapshot policy",
                &self.project_snapshot_policy_hash,
            ),
            ("workspace output partition", &self.partition_identity),
            (
                "workspace output capture policy",
                &self.capture_policy_digest,
            ),
        ] {
            super::thread_snapshot::validate_canonical_hash(label, hash)?;
        }
        if self.roots.is_empty()
            || self.roots.len() > MAX_PRODUCTS
            || self.products.len() > MAX_PRODUCTS
        {
            bail!("workspace output partition has an invalid root or product count");
        }
        let mut root_names = BTreeSet::new();
        let mut root_paths = BTreeSet::new();
        for root in &self.roots {
            root.validate()?;
            if !root_names.insert(root.name.as_str()) || !root_paths.insert(root.path.as_str()) {
                bail!("workspace output partition repeats a root name or path");
            }
        }
        if !ordered(&self.roots, |root| root.name.as_str()) {
            bail!("workspace output roots are not ordered by name");
        }
        for (index, left) in self.roots.iter().enumerate() {
            for right in self.roots.iter().skip(index + 1) {
                if path_contains(&left.path, &right.path) || path_contains(&right.path, &left.path)
                {
                    bail!("workspace output roots overlap");
                }
            }
        }
        let mut names = BTreeSet::new();
        for product in &self.products {
            product.validate()?;
            let ProductSource::WorkspaceOutput { root: source_root } = &product.source else {
                bail!("workspace output partition contains a retained-project product");
            };
            if !names.insert(product.name.as_str()) {
                bail!("workspace output partition repeats a product name");
            }
            let roots = self
                .roots
                .iter()
                .filter(|root| root.contains(&product.path))
                .collect::<Vec<_>>();
            if roots.len() != 1 {
                bail!("workspace output product is not beneath exactly one output root");
            }
            if roots[0].name != *source_root {
                bail!("workspace output product source names a different output root");
            }
            if roots[0].storage != product.storage {
                bail!("workspace output product storage differs from its output root");
            }
        }
        if !ordered(&self.products, |product| product.name.as_str()) {
            bail!("workspace output products are not ordered by name");
        }
        if self.partition_identity != self.derived_partition_identity()? {
            bail!("workspace output partition identity contradicts its static contract");
        }
        if lillux::canonical_json(&serde_json::to_value(self)?)?.len()
            > MAX_WORKSPACE_OUTPUT_PARTITION_BYTES
        {
            bail!("workspace output partition exceeds the serialized byte bound");
        }
        Ok(())
    }

    /// Commits every static field except itself and capture_policy_digest.
    pub fn derived_partition_identity(&self) -> anyhow::Result<String> {
        crate::objects::canonical_value_digest(&serde_json::json!({
            "schema": self.schema,
            "recipe_binding": self.recipe_binding,
            "recipe_ref": self.recipe_ref,
            "recipe_raw_content_digest": self.recipe_raw_content_digest,
            "declarations_hash": self.declarations_hash,
            "project_snapshot_policy_hash": self.project_snapshot_policy_hash,
            "roots": self.roots,
            "products": self.products,
        }))
    }

    /// Commits the behavior-bearing fields of the exact snapshot policy and
    /// each root's effective node-narrowed capture contract.
    pub fn derived_capture_policy_digest(
        &self,
        policy: &super::ProjectSnapshotPolicy,
    ) -> anyhow::Result<String> {
        policy.validate()?;
        let roots = self
            .roots
            .iter()
            .map(|root| {
                serde_json::json!({
                    "name": root.name,
                    "path": root.path,
                    "storage": root.storage,
                    "effective_bounds": root.effective_bounds,
                })
            })
            .collect::<Vec<_>>();
        crate::objects::canonical_value_digest(&serde_json::json!({
            "schema": WORKSPACE_OUTPUT_CAPTURE_POLICY_SCHEMA,
            "language_version": policy.language_version,
            "ryeos_floor_version": policy.ryeos_floor_version,
            "ryeos_floor_rules": policy.ryeos_floor_rules,
            "node_patterns": policy.node_patterns,
            "roots": roots,
        }))
    }

    pub fn validate_source_output_pair(
        &self,
        base: &super::ProjectSnapshot,
        result: &super::ProjectSnapshot,
        policy: &super::ProjectSnapshotPolicy,
    ) -> anyhow::Result<()> {
        super::ProjectSnapshot::from_value(&base.to_value())?;
        self.validate_result_source_policy(result, policy)?;
        if base.effective_policy_hash != self.project_snapshot_policy_hash {
            bail!("workspace source/result snapshots do not share the admitted exact policy");
        }
        Ok(())
    }

    /// Validate the retained result generation without reopening the
    /// historical base snapshot. Workspace output captures intentionally own
    /// their result snapshot but keep the base hash as non-owning testimony,
    /// so cold recovery must remain valid after history GC removes that base.
    pub fn validate_result_source_policy(
        &self,
        result: &super::ProjectSnapshot,
        policy: &super::ProjectSnapshotPolicy,
    ) -> anyhow::Result<()> {
        self.validate()?;
        super::ProjectSnapshot::from_value(&result.to_value())?;
        policy.validate()?;
        let policy_hash = crate::objects::canonical_value_digest(&policy.to_value())?;
        if self.project_snapshot_policy_hash != policy_hash
            || result.effective_policy_hash != policy_hash
        {
            bail!("workspace result snapshot does not use the admitted exact policy");
        }
        if self.capture_policy_digest != self.derived_capture_policy_digest(policy)? {
            bail!("workspace output capture policy digest contradicts its complete policy values");
        }
        Ok(())
    }

    fn root(&self, name: &str) -> anyhow::Result<&WorkspaceOutputRoot> {
        self.roots
            .iter()
            .find(|root| root.name == name)
            .context("workspace output state names an undeclared root")
    }
}

/// Initial admission carries an explicit null capture, so no capsule commits
/// an object which would own that same capsule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOutputAuthority {
    pub partition: WorkspaceOutputPartition,
    #[serde(deserialize_with = "super::deserialize_required_nullable")]
    pub capture_hash: Option<String>,
}

impl WorkspaceOutputAuthority {
    pub fn initial(partition: WorkspaceOutputPartition) -> anyhow::Result<Self> {
        let result = Self {
            partition,
            capture_hash: None,
        };
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.partition.validate()?;
        if let Some(hash) = &self.capture_hash {
            super::thread_snapshot::validate_canonical_hash(
                "workspace output authority capture",
                hash,
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceOutputCaptureState {
    Captured {
        manifest_kind: String,
        manifest_hash: String,
    },
    Absent,
    EmptyDirectory,
}

impl WorkspaceOutputCaptureState {
    fn validate(&self, root: &WorkspaceOutputRoot) -> anyhow::Result<()> {
        if let Self::Captured {
            manifest_kind,
            manifest_hash,
        } = self
        {
            let expected = match root.storage {
                ProductStorage::Content => super::EXTERNAL_CONTENT_MANIFEST_KIND,
                ProductStorage::LargeContent => super::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
            };
            if manifest_kind != expected {
                bail!("workspace output manifest kind contradicts its root storage tier");
            }
            super::thread_snapshot::validate_canonical_hash(
                "workspace output manifest",
                manifest_hash,
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceOutputCapture {
    pub schema: String,
    pub kind: String,
    pub producer_chain_root_id: String,
    pub producer_thread_id: String,
    pub admitted_launch_capsule_hash: String,
    pub base_project_snapshot_hash: String,
    pub result_project_snapshot_hash: String,
    pub partition: WorkspaceOutputPartition,
    pub outputs: BTreeMap<String, WorkspaceOutputCaptureState>,
}

impl WorkspaceOutputCapture {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        if lillux::canonical_json(value)?.len() > MAX_WORKSPACE_OUTPUT_CAPTURE_BYTES {
            bail!("workspace output capture exceeds the serialized byte bound");
        }
        let result: Self =
            serde_json::from_value(value.clone()).context("decode workspace output capture")?;
        result.validate()?;
        Ok(result)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != WORKSPACE_OUTPUT_CAPTURE_SCHEMA
            || self.kind != WORKSPACE_OUTPUT_CAPTURE_KIND
        {
            bail!("workspace output capture schema or kind is not current");
        }
        validate_coordinate(
            "workspace output producer root",
            &self.producer_chain_root_id,
        )?;
        validate_coordinate("workspace output producer thread", &self.producer_thread_id)?;
        for (label, hash) in [
            (
                "workspace output admitted launch capsule",
                &self.admitted_launch_capsule_hash,
            ),
            (
                "workspace output base project snapshot",
                &self.base_project_snapshot_hash,
            ),
            (
                "workspace output result project snapshot",
                &self.result_project_snapshot_hash,
            ),
        ] {
            super::thread_snapshot::validate_canonical_hash(label, hash)?;
        }
        self.partition.validate()?;
        let expected = self
            .partition
            .roots
            .iter()
            .map(|root| root.name.as_str())
            .collect::<BTreeSet<_>>();
        let observed = self
            .outputs
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if expected != observed {
            bail!("workspace output capture states do not exactly cover the partition roots");
        }
        for (name, state) in &self.outputs {
            state.validate(self.partition.root(name)?)?;
        }
        if lillux::canonical_json(&serde_json::to_value(self)?)?.len()
            > MAX_WORKSPACE_OUTPUT_CAPTURE_BYTES
        {
            bail!("workspace output capture exceeds the serialized byte bound");
        }
        Ok(())
    }
}

fn ensure_storage_bounds(storage: ProductStorage, bounds: &ProductBounds) -> anyhow::Result<()> {
    if storage == ProductStorage::Content
        && (bounds.maximum_entries > crate::external_content::MAX_CAPTURE_ENTRIES
            || bounds.maximum_depth > crate::external_content::MAX_CAPTURE_DEPTH
            || bounds.maximum_file_bytes > crate::external_content::MAX_CAPTURE_FILE_BYTES
            || bounds.maximum_total_bytes > crate::external_content::MAX_CAPTURE_BYTES)
    {
        bail!("ordinary workspace output bounds exceed the content storage contract");
    }
    Ok(())
}

fn validate_output_path(path: &str) -> anyhow::Result<()> {
    super::validate_canonical_project_relative_path(path)?;
    if path.len() > super::MAX_EXTERNAL_CONTENT_PATH_BYTES {
        bail!("workspace output root exceeds the external-content path bound");
    }
    if path == ".ai" || path.starts_with(".ai/") {
        bail!("workspace output root cannot reserve the signed project control tree");
    }
    Ok(())
}

fn validate_coordinate(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 2_048
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        bail!("{label} is not a bounded exact coordinate");
    }
    Ok(())
}

fn ordered<T>(values: &[T], name: impl Fn(&T) -> &str) -> bool {
    values
        .windows(2)
        .all(|pair| name(&pair[0]) < name(&pair[1]))
}

fn path_contains(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_sync::ProjectSyncScope;

    fn bounds(total: u64) -> ProductBounds {
        ProductBounds {
            maximum_entries: 8,
            maximum_depth: 4,
            maximum_file_bytes: total.min(1024),
            maximum_total_bytes: total,
        }
    }

    fn product(name: &str, path: &str) -> ProductDeclaration {
        ProductDeclaration {
            name: name.into(),
            source: crate::external_content::products::ProductSource::WorkspaceOutput {
                root: "runtime".into(),
            },
            path: path.into(),
            shape: crate::external_content::products::ProductShape::Tree,
            storage: ProductStorage::LargeContent,
            required: true,
            bounds: bounds(2048),
            expected_manifest_hash: None,
        }
    }

    fn policy(pattern: &str) -> super::super::ProjectSnapshotPolicy {
        super::super::ProjectSnapshotPolicy::new(
            ProjectSyncScope::FullProject,
            Vec::new(),
            vec![pattern.into()],
            BTreeMap::new(),
        )
        .unwrap()
    }

    fn partition() -> WorkspaceOutputPartition {
        let policy = policy("*.tmp");
        let mut value = WorkspaceOutputPartition {
            schema: WORKSPACE_OUTPUT_PARTITION_SCHEMA.into(),
            recipe_binding: "product_recipe".into(),
            recipe_ref: "config:fixtures/product-recipe".into(),
            recipe_raw_content_digest: "d".repeat(64),
            declarations_hash: "e".repeat(64),
            project_snapshot_policy_hash: crate::objects::canonical_value_digest(
                &policy.to_value(),
            )
            .unwrap(),
            roots: vec![
                WorkspaceOutputRoot {
                    name: "build_scratch".into(),
                    path: "products/build-scratch".into(),
                    storage: ProductStorage::LargeContent,
                    declared_bounds: bounds(8192),
                    effective_bounds: bounds(4096),
                },
                WorkspaceOutputRoot {
                    name: "runtime".into(),
                    path: "products/runtime".into(),
                    storage: ProductStorage::LargeContent,
                    declared_bounds: bounds(8192),
                    effective_bounds: bounds(4096),
                },
            ],
            products: vec![product("runtime_debug", "products/runtime/debug")],
            partition_identity: String::new(),
            capture_policy_digest: String::new(),
        };
        value.capture_policy_digest = value.derived_capture_policy_digest(&policy).unwrap();
        value.partition_identity = value.derived_partition_identity().unwrap();
        value
    }

    fn snapshot(policy_hash: &str) -> super::super::ProjectSnapshot {
        super::super::ProjectSnapshot {
            project_tree_hash: "a".repeat(64),
            effective_policy_hash: policy_hash.into(),
            message: None,
            parent_hashes: Vec::new(),
            created_at: "2026-09-08T00:00:00Z".into(),
            source: "test".into(),
        }
    }

    fn capture() -> WorkspaceOutputCapture {
        WorkspaceOutputCapture {
            schema: WORKSPACE_OUTPUT_CAPTURE_SCHEMA.into(),
            kind: WORKSPACE_OUTPUT_CAPTURE_KIND.into(),
            producer_chain_root_id: "T-root".into(),
            producer_thread_id: "T-producer".into(),
            admitted_launch_capsule_hash: "1".repeat(64),
            base_project_snapshot_hash: "2".repeat(64),
            result_project_snapshot_hash: "3".repeat(64),
            partition: partition(),
            outputs: BTreeMap::from([
                ("build_scratch".into(), WorkspaceOutputCaptureState::Absent),
                (
                    "runtime".into(),
                    WorkspaceOutputCaptureState::Captured {
                        manifest_kind: super::super::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
                        manifest_hash: "4".repeat(64),
                    },
                ),
            ]),
        }
    }

    #[test]
    fn partition_commits_static_values_and_capture_keys_dynamic_roots() {
        let capture = capture();
        capture.validate().unwrap();
        let identity = capture.partition.partition_identity.clone();
        let mut changed = capture.clone();
        changed.outputs.insert(
            "runtime".into(),
            WorkspaceOutputCaptureState::EmptyDirectory,
        );
        assert_eq!(
            changed.partition.derived_partition_identity().unwrap(),
            identity
        );
        changed.outputs.remove("build_scratch");
        assert!(changed.validate().is_err());
    }

    #[test]
    fn roots_are_nonoverlapping_but_products_may_be_nested() {
        let mut value = partition();
        value
            .products
            .push(product("runtime_release", "products/runtime/debug/release"));
        value.partition_identity = value.derived_partition_identity().unwrap();
        value.validate().unwrap();
        value.roots[0].path = "products/runtime/debug".into();
        value.partition_identity = value.derived_partition_identity().unwrap();
        assert!(value.validate().is_err());
    }

    #[test]
    fn partition_products_require_the_exact_workspace_source_root() {
        let mut retained = partition();
        retained.products[0].source = ProductSource::RetainedProject {};
        retained.partition_identity = retained.derived_partition_identity().unwrap();
        assert!(retained.validate().is_err());

        let mut wrong_root = partition();
        wrong_root.products[0].source = ProductSource::WorkspaceOutput {
            root: "build_scratch".into(),
        };
        wrong_root.partition_identity = wrong_root.derived_partition_identity().unwrap();
        assert!(wrong_root.validate().is_err());
    }

    #[test]
    fn source_result_pair_requires_exact_complete_policy() {
        let value = partition();
        let exact = policy("*.tmp");
        let base = snapshot(&value.project_snapshot_policy_hash);
        let result = snapshot(&value.project_snapshot_policy_hash);
        value
            .validate_source_output_pair(&base, &result, &exact)
            .unwrap();
        assert!(
            value
                .validate_source_output_pair(&base, &result, &policy("*.cache"))
                .is_err()
        );
        let mut wrong = result;
        wrong.effective_policy_hash = "9".repeat(64);
        assert!(
            value
                .validate_source_output_pair(&base, &wrong, &exact)
                .is_err()
        );
    }

    #[test]
    fn retained_result_policy_does_not_require_historical_base() {
        let value = partition();
        let exact = policy("*.tmp");
        let result = snapshot(&value.project_snapshot_policy_hash);
        value
            .validate_result_source_policy(&result, &exact)
            .unwrap();

        let mut wrong = result;
        wrong.effective_policy_hash = "9".repeat(64);
        assert!(value.validate_result_source_policy(&wrong, &exact).is_err());
    }

    #[test]
    fn initial_authority_requires_explicit_null_capture() {
        let authority = WorkspaceOutputAuthority::initial(partition()).unwrap();
        let mut value = serde_json::to_value(authority).unwrap();
        assert!(value["capture_hash"].is_null());
        value.as_object_mut().unwrap().remove("capture_hash");
        assert!(serde_json::from_value::<WorkspaceOutputAuthority>(value).is_err());
    }

    #[test]
    fn workspace_generation_pair_is_closed_and_requires_nullable_capture() {
        let pair = WorkspaceGenerationPair {
            snapshot_hash: "a".repeat(64),
            output_capture_hash: None,
        };
        pair.validate().unwrap();
        let mut value = serde_json::to_value(&pair).unwrap();
        assert!(value["output_capture_hash"].is_null());
        value.as_object_mut().unwrap().remove("output_capture_hash");
        assert!(serde_json::from_value::<WorkspaceGenerationPair>(value).is_err());

        let mut value = serde_json::to_value(&pair).unwrap();
        value["unexpected"] = Value::Bool(true);
        assert!(serde_json::from_value::<WorkspaceGenerationPair>(value).is_err());

        let invalid = WorkspaceGenerationPair {
            snapshot_hash: "not-a-hash".into(),
            output_capture_hash: Some("b".repeat(64)),
        };
        assert!(invalid.validate().is_err());
    }
}
