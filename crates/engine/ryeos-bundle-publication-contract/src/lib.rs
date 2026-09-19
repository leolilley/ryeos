//! Dependency-neutral wire contracts for native bundle publication.
//!
//! These decoders prove only current-schema structure. CAS resolution, trust,
//! policy, signature verification, and deployment authority remain with their
//! contextual owners.

use std::collections::BTreeSet;

use anyhow::bail;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub const BUNDLE_GENERATION_SCHEMA: &str = "ryeos.bundle_generation.v1";
pub const BUNDLE_GENERATION_KIND: &str = "bundle_generation";
pub const PUBLISHER_MATERIALIZATION_RESULT_SCHEMA: &str =
    "ryeos.publisher_materialization_result.v1";
pub const PUBLISHER_MATERIALIZATION_RESULT_KIND: &str = "publisher_materialization_result";
pub const BUNDLE_SET_SCHEMA: &str = "ryeos.bundle_set.v1";
pub const BUNDLE_SET_KIND: &str = "bundle_set";
pub const NODE_BUNDLE_SELECTION_SCHEMA: &str = "ryeos.node_bundle_selection.v1";
pub const NODE_BUNDLE_SELECTION_KIND: &str = "node_bundle_selection";
pub const BUNDLE_CATALOG_SNAPSHOT_SCHEMA: &str = "ryeos.bundle_catalog_snapshot.v1";
pub const BUNDLE_CATALOG_SNAPSHOT_KIND: &str = "bundle_catalog_snapshot";
pub const BUNDLE_CATALOG_PUBLICATION_SCHEMA: &str = "ryeos.bundle_catalog_publication.v1";
pub const BUNDLE_CATALOG_PUBLICATION_KIND: &str = "bundle_catalog_publication";
pub const BUNDLE_PAYLOAD_OWNERSHIP_SCHEMA: &str = "ryeos.bundle_payload_ownership.v1";
pub const BUNDLE_PAYLOAD_OWNERSHIP_KIND: &str = "bundle_payload_ownership";

pub const MAX_WIRE_BYTES: usize = 64 * 1024;
pub const MAX_EVIDENCE: usize = 64;
pub const MAX_BUNDLES: usize = 1024;
pub const MAX_CHANNELS: usize = 4096;
pub const MAX_OWNED_PAYLOADS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BundleTarget {
    Portable,
    Triple { triple: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleBuildProfile {
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadBuildClass {
    Release,
    Static,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundlePayload {
    pub binary: String,
    pub cargo_package: String,
    pub build_class: PayloadBuildClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundlePayloadOwner {
    pub bundle_name: String,
    pub bundle_sets: Vec<String>,
    pub payloads: Vec<BundlePayload>,
}

/// Publisher-authored ownership of native build products.
///
/// Bundle membership belongs to the bundle, rather than being repeated on
/// every binary record. An absent bundle entry is explicitly data-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundlePayloadOwnership {
    pub schema: String,
    pub kind: String,
    pub bundles: Vec<BundlePayloadOwner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublisherMutationContract {
    RyeosBundleSignV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationRequirement {
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationDecision {
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleGeneration {
    pub schema: String,
    pub kind: String,
    pub bundle_name: String,
    pub authored_version: String,
    pub content_manifest_hash: String,
    pub manifest_item_hash: String,
    pub target: BundleTarget,
    pub build_profile: BundleBuildProfile,
    pub substrate_protocol: u32,
    pub bundle_manifest_format: String,
    pub accepted_product_result_hash: String,
    pub selected_product_identity: String,
    pub selected_product_witness: String,
    pub publisher_materialization_result_hash: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub source_snapshot_hash: Option<String>,
    /// Attestation object hashes. Their subjects retain the qualified objects.
    pub qualification_evidence_hashes: Vec<String>,
    /// An attestation object hash whose subject is the provenance object.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub provenance_hash: Option<String>,
    /// An attestation object hash whose subject is the SBOM object.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub sbom_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherMaterializationResult {
    pub schema: String,
    pub kind: String,
    pub accepted_product_result_hash: String,
    pub selected_product_identity: String,
    pub selected_product_witness: String,
    pub input_content_manifest_hash: String,
    pub output_content_manifest_hash: String,
    pub output_manifest_item_hash: String,
    pub publisher_fingerprint: String,
    pub publisher_tool_effective_definition_digest: String,
    pub publisher_tool_artifact_identity_hash: String,
    pub mutation_contract: PublisherMutationContract,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSetEntry {
    pub bundle_name: String,
    pub generation_hash: String,
    pub publisher_attestation_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSet {
    pub schema: String,
    pub kind: String,
    pub set_name: String,
    pub target: BundleTarget,
    pub substrate_protocol: u32,
    pub entries: Vec<BundleSetEntry>,
    pub migration_requirement: MigrationRequirement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeBundleSelection {
    pub schema: String,
    pub kind: String,
    pub target_node_or_app_root_identity: String,
    pub substrate_image_digest: String,
    pub substrate_protocol: u32,
    pub bundle_set_hash: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub curated_set_attestation_hash: Option<String>,
    pub bundle_publication_policy_section_digest: String,
    pub node_policy_generation_digest: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub expected_active_selection: Option<String>,
    pub migration_decision: MigrationDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleCatalogChannel {
    pub bundle_name: String,
    pub channel: String,
    pub generation_attestation_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetCatalogChannel {
    pub set_name: String,
    pub channel: String,
    pub set_attestation_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleCatalogSnapshot {
    pub schema: String,
    pub kind: String,
    pub publisher: String,
    pub bundle_channels: Vec<BundleCatalogChannel>,
    pub set_channels: Vec<SetCatalogChannel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleCatalogPublication {
    pub schema: String,
    pub kind: String,
    pub catalog_namespace: String,
    pub snapshot_hash: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub previous_publication_attestation_hash: Option<String>,
    pub sequence: u64,
}

macro_rules! wire_impl {
    ($ty:ty) => {
        impl $ty {
            pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
                ensure_wire_bound(value)?;
                let decoded: Self = serde_json::from_value(value.clone())?;
                decoded.validate()?;
                Ok(decoded)
            }

            pub fn to_value(&self) -> anyhow::Result<Value> {
                self.validate()?;
                Ok(serde_json::to_value(self)?)
            }

            pub fn content_hash(&self) -> anyhow::Result<String> {
                let value = self.to_value()?;
                Ok(lillux::cas::sha256_hex(
                    lillux::canonical_json(&value)?.as_bytes(),
                ))
            }
        }
    };
}

wire_impl!(BundleGeneration);
wire_impl!(PublisherMaterializationResult);
wire_impl!(BundleSet);
wire_impl!(NodeBundleSelection);
wire_impl!(BundleCatalogSnapshot);
wire_impl!(BundleCatalogPublication);
wire_impl!(BundlePayloadOwnership);

impl BundlePayloadOwnership {
    pub fn validate(&self) -> anyhow::Result<()> {
        exact(
            &self.schema,
            BUNDLE_PAYLOAD_OWNERSHIP_SCHEMA,
            "bundle payload ownership schema",
        )?;
        exact(
            &self.kind,
            BUNDLE_PAYLOAD_OWNERSHIP_KIND,
            "bundle payload ownership kind",
        )?;
        if self.bundles.len() > MAX_BUNDLES {
            bail!("bundle payload ownership exceeds bundle bound");
        }
        let mut previous_bundle = None;
        let mut owned_binaries = BTreeSet::new();
        let mut payload_count = 0usize;
        for owner in &self.bundles {
            name(&owner.bundle_name, "bundle name")?;
            strictly_after(
                &mut previous_bundle,
                &owner.bundle_name,
                "payload-owner bundles",
            )?;
            if owner.payloads.is_empty() {
                bail!("data-only bundles must be represented by absence, not an empty owner");
            }
            let mut previous_set = None;
            for set_name in &owner.bundle_sets {
                name(set_name, "bundle set name")?;
                strictly_after(&mut previous_set, set_name, "bundle-set membership")?;
            }
            if owner.bundle_sets.is_empty() {
                bail!("native payload owner requires explicit bundle-set membership");
            }
            let mut previous_owned_binary = None;
            for payload in &owner.payloads {
                name(&payload.binary, "binary name")?;
                name(&payload.cargo_package, "cargo package")?;
                strictly_after(
                    &mut previous_owned_binary,
                    &payload.binary,
                    "bundle payloads",
                )?;
                if !owned_binaries.insert(payload.binary.as_str()) {
                    bail!("a binary may have only one bundle owner");
                }
                payload_count = payload_count
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("payload count overflow"))?;
            }
        }
        if payload_count > MAX_OWNED_PAYLOADS {
            bail!("bundle payload ownership exceeds payload bound");
        }
        ensure_encoded_bound(self)
    }

    pub fn owner(&self, bundle_name: &str) -> Option<&BundlePayloadOwner> {
        self.bundles
            .binary_search_by(|owner| owner.bundle_name.as_str().cmp(bundle_name))
            .ok()
            .map(|index| &self.bundles[index])
    }
}

impl BundleGeneration {
    pub fn validate(&self) -> anyhow::Result<()> {
        exact(
            &self.schema,
            BUNDLE_GENERATION_SCHEMA,
            "bundle generation schema",
        )?;
        exact(&self.kind, BUNDLE_GENERATION_KIND, "bundle generation kind")?;
        name(&self.bundle_name, "bundle name")?;
        bounded_token(&self.authored_version, 128, "authored version")?;
        self.target.validate()?;
        if self.substrate_protocol == 0 {
            bail!("substrate protocol must be nonzero");
        }
        bounded_token(&self.bundle_manifest_format, 64, "bundle manifest format")?;
        for (label, hash) in [
            ("content manifest", &self.content_manifest_hash),
            ("manifest item", &self.manifest_item_hash),
            (
                "accepted product result",
                &self.accepted_product_result_hash,
            ),
            ("selected product witness", &self.selected_product_witness),
            (
                "publisher materialization result",
                &self.publisher_materialization_result_hash,
            ),
        ] {
            hash64(hash, label)?;
        }
        name(&self.selected_product_identity, "selected product identity")?;
        optional_hash(&self.source_snapshot_hash, "source snapshot")?;
        sorted_hashes(
            &self.qualification_evidence_hashes,
            "qualification evidence",
        )?;
        optional_hash(&self.provenance_hash, "provenance")?;
        optional_hash(&self.sbom_hash, "SBOM")?;
        ensure_encoded_bound(self)
    }
}

impl PublisherMaterializationResult {
    pub fn validate(&self) -> anyhow::Result<()> {
        exact(
            &self.schema,
            PUBLISHER_MATERIALIZATION_RESULT_SCHEMA,
            "publisher materialization schema",
        )?;
        exact(
            &self.kind,
            PUBLISHER_MATERIALIZATION_RESULT_KIND,
            "publisher materialization kind",
        )?;
        for (label, hash) in [
            (
                "accepted product result",
                &self.accepted_product_result_hash,
            ),
            ("selected product witness", &self.selected_product_witness),
            ("input content manifest", &self.input_content_manifest_hash),
            (
                "output content manifest",
                &self.output_content_manifest_hash,
            ),
            ("output manifest item", &self.output_manifest_item_hash),
            ("publisher fingerprint", &self.publisher_fingerprint),
            (
                "publisher tool definition",
                &self.publisher_tool_effective_definition_digest,
            ),
            (
                "publisher tool artifact identity",
                &self.publisher_tool_artifact_identity_hash,
            ),
        ] {
            hash64(hash, label)?;
        }
        name(&self.selected_product_identity, "selected product identity")?;
        if self.input_content_manifest_hash == self.output_content_manifest_hash {
            bail!("publisher materialization input and output manifests must differ");
        }
        ensure_encoded_bound(self)
    }
}

impl BundleSet {
    pub fn validate(&self) -> anyhow::Result<()> {
        exact(&self.schema, BUNDLE_SET_SCHEMA, "bundle set schema")?;
        exact(&self.kind, BUNDLE_SET_KIND, "bundle set kind")?;
        name(&self.set_name, "set name")?;
        self.target.validate()?;
        if self.substrate_protocol == 0 {
            bail!("substrate protocol must be nonzero");
        }
        if self.entries.is_empty() || self.entries.len() > MAX_BUNDLES {
            bail!("bundle set requires a bounded nonempty entry list");
        }
        let mut previous = None;
        for entry in &self.entries {
            name(&entry.bundle_name, "bundle name")?;
            hash64(&entry.generation_hash, "generation")?;
            hash64(&entry.publisher_attestation_hash, "publisher attestation")?;
            strictly_after(&mut previous, &entry.bundle_name, "bundle set entries")?;
        }
        ensure_encoded_bound(self)
    }
}

impl NodeBundleSelection {
    pub fn validate(&self) -> anyhow::Result<()> {
        exact(
            &self.schema,
            NODE_BUNDLE_SELECTION_SCHEMA,
            "node bundle selection schema",
        )?;
        exact(
            &self.kind,
            NODE_BUNDLE_SELECTION_KIND,
            "node bundle selection kind",
        )?;
        bounded_token(
            &self.target_node_or_app_root_identity,
            512,
            "target identity",
        )?;
        image_digest(&self.substrate_image_digest)?;
        for (label, hash) in [
            ("bundle set", &self.bundle_set_hash),
            (
                "bundle publication policy section",
                &self.bundle_publication_policy_section_digest,
            ),
            (
                "node policy generation",
                &self.node_policy_generation_digest,
            ),
        ] {
            hash64(hash, label)?;
        }
        if self.substrate_protocol == 0 {
            bail!("substrate protocol must be nonzero");
        }
        optional_hash(
            &self.curated_set_attestation_hash,
            "curated set attestation",
        )?;
        optional_hash(&self.expected_active_selection, "expected active selection")?;
        ensure_encoded_bound(self)
    }
}

impl BundleCatalogSnapshot {
    pub fn validate(&self) -> anyhow::Result<()> {
        exact(
            &self.schema,
            BUNDLE_CATALOG_SNAPSHOT_SCHEMA,
            "catalog snapshot schema",
        )?;
        exact(
            &self.kind,
            BUNDLE_CATALOG_SNAPSHOT_KIND,
            "catalog snapshot kind",
        )?;
        fingerprint_principal(&self.publisher)?;
        if self.bundle_channels.len() + self.set_channels.len() > MAX_CHANNELS {
            bail!("catalog snapshot exceeds channel bound");
        }
        let mut previous = None;
        for entry in &self.bundle_channels {
            name(&entry.bundle_name, "bundle name")?;
            name(&entry.channel, "channel")?;
            hash64(&entry.generation_attestation_hash, "generation attestation")?;
            let key = format!("{}\0{}", entry.bundle_name, entry.channel);
            strictly_after(&mut previous, &key, "bundle channels")?;
        }
        let mut previous = None;
        for entry in &self.set_channels {
            name(&entry.set_name, "set name")?;
            name(&entry.channel, "channel")?;
            hash64(&entry.set_attestation_hash, "set attestation")?;
            let key = format!("{}\0{}", entry.set_name, entry.channel);
            strictly_after(&mut previous, &key, "set channels")?;
        }
        ensure_encoded_bound(self)
    }
}

impl BundleCatalogPublication {
    pub fn validate(&self) -> anyhow::Result<()> {
        exact(
            &self.schema,
            BUNDLE_CATALOG_PUBLICATION_SCHEMA,
            "catalog publication schema",
        )?;
        exact(
            &self.kind,
            BUNDLE_CATALOG_PUBLICATION_KIND,
            "catalog publication kind",
        )?;
        name(&self.catalog_namespace, "catalog namespace")?;
        hash64(&self.snapshot_hash, "catalog snapshot")?;
        optional_hash(
            &self.previous_publication_attestation_hash,
            "previous publication attestation",
        )?;
        match (&self.previous_publication_attestation_hash, self.sequence) {
            (None, 0) | (Some(_), 1..) => {}
            _ => bail!(
                "catalog genesis must have sequence zero and successors require a predecessor"
            ),
        }
        ensure_encoded_bound(self)
    }
}

impl BundleTarget {
    fn validate(&self) -> anyhow::Result<()> {
        if let Self::Triple { triple } = self {
            bounded_token(triple, 128, "target triple")?;
            if !triple
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
                || !triple.contains('-')
            {
                bail!("target triple is not canonical");
            }
        }
        Ok(())
    }
}

pub fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn ensure_wire_bound(value: &Value) -> anyhow::Result<()> {
    if lillux::canonical_json(value)?.len() > MAX_WIRE_BYTES {
        bail!("bundle publication object exceeds wire byte bound");
    }
    Ok(())
}

fn ensure_encoded_bound<T: Serialize>(value: &T) -> anyhow::Result<()> {
    ensure_wire_bound(&serde_json::to_value(value)?)
}

fn exact(actual: &str, expected: &str, label: &str) -> anyhow::Result<()> {
    if actual != expected {
        bail!("unsupported {label}");
    }
    Ok(())
}

fn hash64(value: &str, label: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("{label} must be a lowercase 64-hex digest");
    }
    Ok(())
}

fn optional_hash(value: &Option<String>, label: &str) -> anyhow::Result<()> {
    if let Some(value) = value {
        hash64(value, label)?;
    }
    Ok(())
}

fn image_digest(value: &str) -> anyhow::Result<()> {
    let Some(hash) = value.strip_prefix("sha256:") else {
        bail!("substrate image digest must use sha256");
    };
    hash64(hash, "substrate image digest")
}

fn name(value: &str, label: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
    {
        bail!("{label} must be a bounded lowercase identifier");
    }
    Ok(())
}

fn bounded_token(value: &str, max: usize, label: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
        bail!("{label} must be a bounded non-control string");
    }
    Ok(())
}

fn fingerprint_principal(value: &str) -> anyhow::Result<()> {
    let Some(raw) = value.strip_prefix("fp:") else {
        bail!("publisher must be a fingerprint principal");
    };
    hash64(raw, "publisher fingerprint")
}

fn sorted_hashes(values: &[String], label: &str) -> anyhow::Result<()> {
    if values.len() > MAX_EVIDENCE {
        bail!("{label} exceeds evidence bound");
    }
    let mut previous = None;
    for value in values {
        hash64(value, label)?;
        strictly_after(&mut previous, value, label)?;
    }
    Ok(())
}

fn strictly_after(previous: &mut Option<String>, value: &str, label: &str) -> anyhow::Result<()> {
    if previous.as_deref().is_some_and(|p| p >= value) {
        bail!("{label} must be strictly sorted and unique");
    }
    *previous = Some(value.to_owned());
    Ok(())
}

#[cfg(test)]
mod tests;
