//! Named retained products and node testimony over their canonical capture.
//!
//! These are data contracts, not execution or publication capabilities. The
//! application owner resolves declarations from admitted recipe facts and checks
//! successful retained-result authority before signing. Attestation's existing
//! subject edge owns the product manifest; provenance coordinates below do not
//! retain entire producer projects. No interpreter or build language is required.

use std::collections::BTreeSet;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

use crate::objects::{
    Attestation, canonical_value_digest, validate_canonical_project_relative_path,
};
use crate::signer::Signer;

pub mod accepted_result;
pub mod admission;

pub mod composition;

pub mod publication;

pub mod qualification;

pub mod qualification_publication;

pub mod transfer;

pub const PRODUCT_DECLARATIONS_SCHEMA: &str = "ryeos.build_products.v1";
pub const PRODUCT_CAPTURE_POLICY: &str = "ryeos.retained_product_capture.v3";
pub const PRODUCT_CAPTURE_EVIDENCE_SCHEMA: u32 = 3;
pub const PRODUCT_CAPTURE_CLAIM: &str = "retained_product_captured";
pub const MAX_PRODUCTS: usize = 32;
pub const MAX_PRODUCT_DECLARATIONS_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductShape {
    File,
    Tree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductStorage {
    Content,
    LargeContent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProductSource {
    RetainedProject {},
    WorkspaceOutput { root: String },
}

impl ProductSource {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Self::WorkspaceOutput { root } = self {
            validate_name(root)?;
        }
        Ok(())
    }
}

/// Authored ceilings. Node policy can narrow these, never widen them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductBounds {
    pub maximum_entries: usize,
    pub maximum_depth: usize,
    pub maximum_file_bytes: u64,
    pub maximum_total_bytes: u64,
}

impl ProductBounds {
    pub fn validate(&self) -> anyhow::Result<()> {
        super::LargeContentCaptureBounds {
            max_entries: self.maximum_entries,
            max_depth: self.maximum_depth,
            max_file_bytes: self.maximum_file_bytes,
            max_total_bytes: self.maximum_total_bytes,
        }
        .validate()?;
        Ok(())
    }

    pub fn intersect(
        &self,
        node: &super::LargeContentCaptureBounds,
    ) -> anyhow::Result<super::LargeContentCaptureBounds> {
        self.validate()?;
        node.validate()?;
        let max_total_bytes = self.maximum_total_bytes.min(node.max_total_bytes);
        Ok(super::LargeContentCaptureBounds {
            max_entries: self.maximum_entries.min(node.max_entries),
            max_depth: self.maximum_depth.min(node.max_depth),
            max_file_bytes: self
                .maximum_file_bytes
                .min(node.max_file_bytes)
                .min(max_total_bytes),
            max_total_bytes,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductDeclaration {
    pub name: String,
    pub source: ProductSource,
    pub path: String,
    pub shape: ProductShape,
    pub storage: ProductStorage,
    pub required: bool,
    pub bounds: ProductBounds,
    /// Assertion over the canonical manifest, not an acquisition/archive hash.
    #[serde(default)]
    pub expected_manifest_hash: Option<String>,
}

impl ProductDeclaration {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_name(&self.name)?;
        self.source.validate()?;
        validate_canonical_project_relative_path(&self.path)?;
        self.bounds.validate()?;
        if let Some(hash) = &self.expected_manifest_hash {
            validate_hash("expected product manifest", hash)?;
        }
        if self.storage == ProductStorage::Content
            && (self.bounds.maximum_entries > super::MAX_CAPTURE_ENTRIES
                || self.bounds.maximum_depth > super::MAX_CAPTURE_DEPTH
                || self.bounds.maximum_file_bytes > super::MAX_CAPTURE_FILE_BYTES
                || self.bounds.maximum_total_bytes > super::MAX_CAPTURE_BYTES)
        {
            bail!("ordinary product bounds exceed the content storage contract");
        }
        Ok(())
    }

    /// Select only an immutable retained generation. An optional product may
    /// be absent, but malformed, excluded or over-budget content is not absence.
    /// Publication, terminal ownership and recipe admission remain caller duties.
    pub fn select_retained(
        &self,
        snapshot: &crate::project_materialization::VerifiedProjectSnapshotClosure,
        configured_ignore: &crate::ignore::IgnoreMatcher,
        node_bounds: &super::LargeContentCaptureBounds,
    ) -> anyhow::Result<Option<super::retained_project::RetainedProjectContent>> {
        self.validate()?;
        if !matches!(self.source, ProductSource::RetainedProject {}) {
            bail!("workspace-output products require the partition capture lifecycle");
        }
        let prefix = format!("{}/", self.path);
        let files = snapshot.tree().files();
        let file_exists = files.contains_key(&self.path);
        let descendants_exist = files.keys().any(|path| path.starts_with(&prefix));
        match self.shape {
            ProductShape::File if descendants_exist => {
                bail!("file product selects a retained directory")
            }
            ProductShape::Tree if file_exists => bail!("tree product selects a retained file"),
            _ => {}
        }
        let bounds = self.bounds.intersect(node_bounds)?;
        let policy =
            super::LargeContentCapturePolicy::new(self.path.clone(), configured_ignore, bounds)?;
        if !file_exists && !descendants_exist {
            if self.required {
                bail!("required retained product is missing: {}", self.name);
            }
            return Ok(None);
        }
        let shape = match self.shape {
            ProductShape::File => super::ExternalContentCaptureKind::File,
            ProductShape::Tree => super::ExternalContentCaptureKind::Tree,
        };
        super::retained_project::RetainedProjectContent::select(snapshot, shape, &policy).map(Some)
    }
}

/// Parsed payload of a signed recipe Config. Source parsing remains with the
/// registered parser; this decoder accepts its structured product block only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductDeclarations {
    pub schema: String,
    pub output_roots: Vec<crate::objects::WorkspaceOutputRootDeclaration>,
    pub products: Vec<ProductDeclaration>,
}

impl ProductDeclarations {
    pub fn from_value(value: serde_json::Value) -> anyhow::Result<Self> {
        if lillux::canonical_json(&value)?.len() > MAX_PRODUCT_DECLARATIONS_BYTES {
            bail!("product declarations exceed the bounded contract");
        }
        let result: Self = serde_json::from_value(value).context("decode product declarations")?;
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PRODUCT_DECLARATIONS_SCHEMA
            || (self.products.is_empty() && self.output_roots.is_empty())
            || self.products.len() > MAX_PRODUCTS
            || self.output_roots.len() > MAX_PRODUCTS
        {
            bail!("invalid product declaration schema or count");
        }
        if lillux::canonical_json(&serde_json::to_value(self)?)?.len()
            > MAX_PRODUCT_DECLARATIONS_BYTES
        {
            bail!("product declarations exceed the bounded contract");
        }
        let mut names = BTreeSet::new();
        let mut root_names = BTreeSet::new();
        let mut root_paths = BTreeSet::new();
        for root in &self.output_roots {
            root.validate()?;
            if !root_names.insert(root.name.as_str()) || !root_paths.insert(root.path.as_str()) {
                bail!("duplicate workspace output root name or path");
            }
        }
        if !self
            .output_roots
            .windows(2)
            .all(|pair| pair[0].name < pair[1].name)
        {
            bail!("workspace output roots are not ordered by name");
        }
        for (index, left) in self.output_roots.iter().enumerate() {
            for right in self.output_roots.iter().skip(index + 1) {
                if left.contains(&right.path) || right.contains(&left.path) {
                    bail!("workspace output roots overlap");
                }
            }
        }
        for product in &self.products {
            product.validate()?;
            if !names.insert(&product.name) {
                bail!("duplicate product name");
            }
            if let ProductSource::WorkspaceOutput { root } = &product.source {
                let root = self
                    .output_roots
                    .iter()
                    .find(|candidate| &candidate.name == root)
                    .context("workspace-output product names an undeclared root")?;
                if !root.contains(&product.path) || root.storage != product.storage {
                    bail!("workspace-output product contradicts its declared root");
                }
            }
        }
        for file in self
            .products
            .iter()
            .filter(|product| product.shape == ProductShape::File)
        {
            let prefix = format!("{}/", file.path);
            if self
                .products
                .iter()
                .any(|other| other.source == file.source && other.path.starts_with(&prefix))
            {
                bail!("file product cannot contain another product selection");
            }
        }
        Ok(())
    }

    pub fn select(&self, name: &str) -> anyhow::Result<&ProductDeclaration> {
        self.validate()?;
        self.products
            .iter()
            .find(|product| product.name == name)
            .context("product is not declared by the admitted recipe")
    }

    pub fn content_hash(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::to_value(self)?)
    }

    pub fn requires_workspace_output_capture(&self) -> bool {
        !self.output_roots.is_empty()
            || self
                .products
                .iter()
                .any(|product| matches!(product.source, ProductSource::WorkspaceOutput { .. }))
    }
}

/// Compact, non-secret projection of the exact producer admission checked at
/// capture time.
///
/// The capsule and historical thread remain non-owning coordinates. These
/// existing digests preserve enough producer and invocation identity for a
/// later relationship verifier to reject an unrelated program that admitted
/// the same product recipe, without copying the sealed request or its possibly
/// sensitive parameters into another surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductProducerAdmission {
    pub canonical_ref: String,
    pub effective_definition_digest: String,
    pub exact_program_hash: String,
    pub producer_project_snapshot_hash: String,
    pub launch_authority_digest: String,
    pub admitted_parameters_digest: String,
}

impl ProductProducerAdmission {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_canonical_unsuffixed_ref("product producer", &self.canonical_ref)?;
        for (label, hash) in [
            (
                "product producer effective definition",
                &self.effective_definition_digest,
            ),
            ("product producer exact program", &self.exact_program_hash),
            (
                "product producer project snapshot",
                &self.producer_project_snapshot_hash,
            ),
            (
                "product producer launch authority",
                &self.launch_authority_digest,
            ),
            (
                "product producer admitted parameters",
                &self.admitted_parameters_digest,
            ),
        ] {
            validate_hash(label, hash)?;
        }
        Ok(())
    }
}

/// Compact testimony. Recipe declaration bytes are inline; historical hashes
/// are coordinates, not owning closure edges. The node attests that it checked
/// those historical facts at capture. This is not independent full-run proof,
/// compatibility qualification, reproducibility, or a consumer binding grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductCaptureEvidence {
    pub schema: u32,
    pub owner_principal: String,
    pub chain_root_id: String,
    pub thread_id: String,
    pub admitted_launch_capsule_hash: String,
    pub producer: ProductProducerAdmission,
    /// Original admitted input before any verified machine/follow advancement.
    /// `producer` continues to name the actual terminal placement admission.
    pub root_producer: ProductProducerAdmission,
    pub result_project_snapshot_hash: String,
    /// Exact retained workspace-output capture selected for this product.
    /// Required-null: retained-project products carry null; workspace-output
    /// products carry their terminal generation's capture hash. This is
    /// non-owning testimony; the witness owns only the selected manifest.
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub workspace_output_capture_hash: Option<String>,
    /// Compact partition testimony survives collection of the historical
    /// capture. It is not a new owning edge or caller-selected authority.
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub producer_partition_identity: Option<String>,
    /// Name of the launch-contract binding through which the recipe Config
    /// was admitted. A Config ref found elsewhere in the project is not an
    /// admitted recipe.
    pub recipe_binding: String,
    pub recipe_ref: String,
    pub recipe_raw_content_digest: String,
    /// Bounded admitted declaration block, retained inline so its identity and
    /// selected member remain inspectable after historical source collection.
    pub declarations: ProductDeclarations,
    pub declarations_hash: String,
    /// Exact bounded relationship block admitted from the same recipe Config.
    /// It remains inline because the Config digest is a non-owning historical
    /// coordinate after producer source collection.
    pub relationships: composition::ProductRelationships,
    pub declaration: ProductDeclaration,
    pub capture_policy_digest: String,
    pub manifest_hash: String,
    pub manifest_kind: String,
    pub entry_count: usize,
    pub total_bytes: u64,
}

impl ProductCaptureEvidence {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PRODUCT_CAPTURE_EVIDENCE_SCHEMA {
            bail!("unsupported product capture evidence schema");
        }
        let owner = self
            .owner_principal
            .strip_prefix("fp:")
            .context("product owner must be an exact fingerprint principal")?;
        validate_hash("product owner", owner)?;
        for (label, value) in [
            ("product root", &self.chain_root_id),
            ("product thread", &self.thread_id),
            ("product recipe", &self.recipe_ref),
        ] {
            if value.is_empty()
                || value.len() > 2048
                || value.trim() != value
                || value.chars().any(char::is_control)
            {
                bail!("{label} is not a bounded exact coordinate");
            }
        }
        validate_binding_name(&self.recipe_binding)?;
        validate_canonical_unsuffixed_ref("product recipe", &self.recipe_ref)?;
        if !self.recipe_ref.starts_with("config:") {
            bail!("product recipe must be a Config");
        }
        for hash in [
            &self.admitted_launch_capsule_hash,
            &self.result_project_snapshot_hash,
            &self.recipe_raw_content_digest,
            &self.declarations_hash,
            &self.capture_policy_digest,
            &self.manifest_hash,
        ] {
            validate_hash("product evidence", hash)?;
        }
        if let Some(hash) = &self.workspace_output_capture_hash {
            validate_hash("product workspace output capture", hash)?;
        }
        if let Some(hash) = &self.producer_partition_identity {
            validate_hash("product producer partition", hash)?;
        }
        if self.workspace_output_capture_hash.is_some()
            != self.producer_partition_identity.is_some()
        {
            bail!(
                "product partition testimony must accompany exactly its workspace output capture"
            );
        }
        self.producer.validate()?;
        self.root_producer.validate()?;
        if self.root_producer.canonical_ref != self.producer.canonical_ref {
            bail!("product root and terminal producer refs disagree");
        }
        self.declaration.validate()?;
        match (
            &self.declaration.source,
            &self.workspace_output_capture_hash,
        ) {
            (ProductSource::RetainedProject {}, None)
            | (ProductSource::WorkspaceOutput { .. }, Some(_)) => {}
            (ProductSource::RetainedProject {}, Some(_)) => {
                bail!("retained-project testimony cannot name a workspace output capture")
            }
            (ProductSource::WorkspaceOutput { .. }, None) => {
                bail!("workspace-output testimony requires its exact retained capture")
            }
        }
        if self.declarations.content_hash()? != self.declarations_hash
            || self.declarations.select(&self.declaration.name)? != &self.declaration
        {
            bail!("product testimony declaration differs from its admitted declaration block");
        }
        self.relationships
            .validate_against(&self.declarations, &self.recipe_binding)?;
        let kind = match self.declaration.storage {
            ProductStorage::Content => crate::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
            ProductStorage::LargeContent => crate::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
        };
        if self.manifest_kind != kind
            || self.entry_count == 0
            || self.entry_count > self.declaration.bounds.maximum_entries
            || self.total_bytes > self.declaration.bounds.maximum_total_bytes
            || self
                .declaration
                .expected_manifest_hash
                .as_ref()
                .is_some_and(|hash| hash != &self.manifest_hash)
        {
            bail!("captured product contradicts its declaration");
        }
        Ok(())
    }

    pub fn sign_attestation(
        &self,
        signer: &dyn Signer,
        recorded_at: String,
    ) -> anyhow::Result<Attestation> {
        self.validate()?;
        Attestation::unsigned(
            self.manifest_hash.clone(),
            PRODUCT_CAPTURE_CLAIM.to_owned(),
            PRODUCT_CAPTURE_POLICY.to_owned(),
            recorded_at,
            None,
            serde_json::to_value(self)?,
        )
        .sign(signer)
    }

    /// Structural decoding only; the application must additionally verify the
    /// signature with the authorized node key before accepting testimony.
    pub fn from_attestation(attestation: &Attestation) -> anyhow::Result<Self> {
        attestation.validate()?;
        if attestation.claim != PRODUCT_CAPTURE_CLAIM
            || attestation.policy != PRODUCT_CAPTURE_POLICY
            || attestation.expires_at.is_some()
        {
            bail!("attestation is not durable product capture testimony");
        }
        let evidence: Self = serde_json::from_value(attestation.evidence.clone())?;
        evidence.validate()?;
        if evidence.manifest_hash != attestation.subject_hash {
            bail!("product testimony subject mismatch");
        }
        Ok(evidence)
    }

    /// Authenticate exact-node testimony for an already-authorized owner.
    /// Choosing/trusting that node key and authorizing the caller belong to the
    /// application. This does not authorize capture, binding or publication.
    pub fn verify_attestation_for_owner(
        attestation: &Attestation,
        node_key: &lillux::crypto::VerifyingKey,
        owner_principal: &str,
    ) -> anyhow::Result<Self> {
        attestation.verify_with_key(node_key)?;
        let evidence = Self::from_attestation(attestation)?;
        if evidence.owner_principal != owner_principal {
            bail!("product testimony belongs to a different owner");
        }
        Ok(evidence)
    }
}

pub fn validate_name(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        bail!("product name must be a bounded lowercase identifier");
    }
    Ok(())
}

pub fn validate_binding_name(value: &str) -> anyhow::Result<()> {
    let mut segments = value.split('_');
    let valid = !value.is_empty()
        && value.len() <= 64
        && segments.next().is_some_and(|segment| {
            segment
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_lowercase)
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
        && segments.all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        });
    if !valid {
        bail!("product recipe binding must be lower snake case and at most 64 bytes");
    }
    Ok(())
}

fn validate_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if !lillux::valid_hash(value) || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        bail!("{label} must be a canonical digest");
    }
    Ok(())
}

pub(crate) fn validate_canonical_unsuffixed_ref(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() > 2_048
        || value.trim() != value
        || value.chars().any(char::is_control)
        || value.contains('@')
    {
        bail!("{label} ref is not a bounded unsuffixed canonical ref");
    }
    crate::objects::thread_snapshot::validate_canonical_item_ref(value)
        .map_err(|error| anyhow::anyhow!("{label} ref is not canonical: {error}"))
}

#[cfg(test)]
mod tests;
