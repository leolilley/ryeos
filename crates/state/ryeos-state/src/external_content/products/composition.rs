//! Bounded signed product allowances and invocation-time selection projections.
//!
//! These types carry identity, not authority. The application authenticates the
//! witness, producer, consumer generation and binding before constructing a
//! resolved selection. State validation keeps the durable projection closed and
//! self-consistent without resolving projects or trusting caller metadata.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::publication::ProductCaptureCoordinate;
use super::{
    ProductBounds, ProductCaptureEvidence, ProductDeclarations, ProductProducerAdmission,
    ProductShape, ProductStorage, validate_binding_name, validate_canonical_unsuffixed_ref,
    validate_hash, validate_name,
};
use crate::objects::{
    EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
    EffectiveSourceClosureProjection, ExternalContentKind, ExternalContentMountRoot,
    MAX_EXTERNAL_CONTENT_PATH_BYTES, validate_canonical_project_relative_path,
};

pub const PRODUCT_RELATIONSHIPS_SCHEMA: &str = "ryeos.product_relationships.v1";
pub const RESOLVED_EXTERNAL_PRODUCT_SELECTION_SCHEMA: &str =
    "ryeos.resolved_external_product_selection.v4";
pub const EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY: &str = "effective_external_product_selections";
pub const MAX_PRODUCT_RELATIONSHIPS: usize = 32;
pub const MAX_PRODUCT_SELECTIONS: usize = 32;
pub const MAX_PRODUCT_RELATIONSHIPS_BYTES: usize = 16 * 1024;
pub const MAX_PRODUCT_SELECTION_INPUTS_BYTES: usize = 16 * 1024;
pub const MAX_RESOLVED_PRODUCT_SELECTIONS_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductRelationshipProducer {
    pub canonical_ref: String,
    pub recipe_binding: String,
    pub product_name: String,
    pub parameters: Value,
}

impl ProductRelationshipProducer {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_canonical_unsuffixed_ref("product relationship producer", &self.canonical_ref)?;
        if !self.canonical_ref.starts_with("graph:") {
            bail!("product relationship producer must be an exact Graph ref");
        }
        validate_binding_name(&self.recipe_binding)?;
        validate_name(&self.product_name)?;
        // This is the same canonical value projection used by
        // SealedRootExecutionRequest::admitted_parameters_digest(). It keeps
        // raw parameters only in signed allowance data and emits no new digest.
        self.admitted_parameters_digest()?;
        Ok(())
    }

    pub fn admitted_parameters_digest(&self) -> anyhow::Result<String> {
        crate::objects::canonical_value_digest(&self.parameters)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductRelationshipConsumer {
    pub canonical_ref: String,
    pub declaration_id: String,
}

impl ProductRelationshipConsumer {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_canonical_unsuffixed_ref("product relationship consumer", &self.canonical_ref)?;
        validate_name(&self.declaration_id)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductRelationshipRequiredProduct {
    pub shape: ProductShape,
    pub storage: ProductStorage,
    pub bounds: ProductBounds,
}

impl ProductRelationshipRequiredProduct {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.bounds.validate()
    }

    fn ensure_narrows(&self, allowed: &super::ProductDeclaration) -> anyhow::Result<()> {
        if self.shape != allowed.shape
            || self.storage != allowed.storage
            || self.bounds.maximum_entries > allowed.bounds.maximum_entries
            || self.bounds.maximum_depth > allowed.bounds.maximum_depth
            || self.bounds.maximum_file_bytes > allowed.bounds.maximum_file_bytes
            || self.bounds.maximum_total_bytes > allowed.bounds.maximum_total_bytes
        {
            bail!("product relationship requirement widens or changes its declaration");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductRelationshipQualification {
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub policy_ref: Option<String>,
    pub required_claims: Vec<String>,
}

impl ProductRelationshipQualification {
    pub fn validate(&self) -> anyhow::Result<()> {
        match &self.policy_ref {
            None if self.required_claims.is_empty() => return Ok(()),
            None => bail!("required product claims have no signed qualification policy"),
            Some(reference) => {
                validate_canonical_unsuffixed_ref("product qualification policy", reference)?;
                if !reference.starts_with("config:") {
                    bail!("product qualification policy must be an exact Config ref");
                }
            }
        }
        if self.required_claims.is_empty()
            || self.required_claims.len() > super::qualification::MAX_PRODUCT_QUALIFICATION_CLAIMS
            || self
                .required_claims
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            bail!("required product claims must be a nonempty bounded sorted unique set");
        }
        for claim in &self.required_claims {
            validate_name(claim)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductRelationship {
    pub name: String,
    pub producer: ProductRelationshipProducer,
    pub consumer: ProductRelationshipConsumer,
    pub required_product: ProductRelationshipRequiredProduct,
    pub qualification: ProductRelationshipQualification,
}

impl ProductRelationship {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_name(&self.name)?;
        self.producer.validate()?;
        self.consumer.validate()?;
        self.required_product.validate()?;
        self.qualification.validate()?;
        if lillux::canonical_json(&serde_json::to_value(self)?)?.len()
            > MAX_PRODUCT_RELATIONSHIPS_BYTES
        {
            bail!("product relationship exceeds the bounded contract");
        }
        Ok(())
    }

    fn validate_against(
        &self,
        declarations: &ProductDeclarations,
        recipe_binding: &str,
    ) -> anyhow::Result<()> {
        self.validate()?;
        if self.producer.recipe_binding != recipe_binding {
            bail!("product relationship names a different admitted recipe binding");
        }
        self.required_product
            .ensure_narrows(declarations.select(&self.producer.product_name)?)
    }

    /// Compare one authenticated witness's retained testimony with this signed
    /// allowance. Node/head authentication and consumer authority remain with
    /// the application owner.
    pub fn validate_product_evidence(
        &self,
        evidence: &ProductCaptureEvidence,
    ) -> anyhow::Result<()> {
        self.validate()?;
        evidence.validate()?;
        if evidence.relationships.select(&self.name)? != self {
            bail!("product witness retained a different signed relationship");
        }
        if self.producer.canonical_ref != evidence.root_producer.canonical_ref
            || self.producer.recipe_binding != evidence.recipe_binding
            || self.producer.product_name != evidence.declaration.name
            || self.producer.admitted_parameters_digest()?
                != evidence.root_producer.admitted_parameters_digest
        {
            bail!("product witness contradicts the allowed producer invocation");
        }
        self.required_product
            .ensure_narrows(&evidence.declaration)?;
        let required_kind = match self.required_product.storage {
            ProductStorage::Content => EXTERNAL_CONTENT_MANIFEST_KIND,
            ProductStorage::LargeContent => EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
        };
        if evidence.manifest_kind != required_kind
            || evidence.entry_count > self.required_product.bounds.maximum_entries
            || evidence.total_bytes > self.required_product.bounds.maximum_total_bytes
        {
            bail!("product witness exceeds the relationship requirement");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductRelationships {
    pub schema: String,
    pub relationships: Vec<ProductRelationship>,
}

impl ProductRelationships {
    pub fn empty() -> Self {
        Self {
            schema: PRODUCT_RELATIONSHIPS_SCHEMA.to_owned(),
            relationships: Vec::new(),
        }
    }

    pub fn from_value(value: Value) -> anyhow::Result<Self> {
        if lillux::canonical_json(&value)?.len() > MAX_PRODUCT_RELATIONSHIPS_BYTES {
            bail!("product relationships exceed the bounded contract");
        }
        let result: Self = serde_json::from_value(value).context("decode product relationships")?;
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PRODUCT_RELATIONSHIPS_SCHEMA
            || self.relationships.len() > MAX_PRODUCT_RELATIONSHIPS
        {
            bail!("invalid product relationship schema or count");
        }
        if lillux::canonical_json(&serde_json::to_value(self)?)?.len()
            > MAX_PRODUCT_RELATIONSHIPS_BYTES
        {
            bail!("product relationships exceed the bounded contract");
        }
        let mut names = BTreeSet::new();
        for relationship in &self.relationships {
            relationship.validate()?;
            if !names.insert(&relationship.name) {
                bail!("duplicate product relationship name");
            }
        }
        Ok(())
    }

    pub fn validate_against(
        &self,
        declarations: &ProductDeclarations,
        recipe_binding: &str,
    ) -> anyhow::Result<()> {
        self.validate()?;
        declarations.validate()?;
        validate_binding_name(recipe_binding)?;
        for relationship in &self.relationships {
            relationship.validate_against(declarations, recipe_binding)?;
        }
        Ok(())
    }

    pub fn select(&self, name: &str) -> anyhow::Result<&ProductRelationship> {
        self.validate()?;
        self.relationships
            .iter()
            .find(|relationship| relationship.name == name)
            .context("product relationship is not admitted by the recipe")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalProductSlotDeclaration {
    pub id: String,
    pub relationship_ref: String,
    pub relationship: String,
    pub kind: ExternalContentKind,
    pub mount_root: ExternalContentMountRoot,
    pub mount: String,
}

impl ExternalProductSlotDeclaration {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_name(&self.id)?;
        validate_canonical_unsuffixed_ref("external product relationship", &self.relationship_ref)?;
        if !self.relationship_ref.starts_with("config:") {
            bail!("external product relationship must be an exact Config ref");
        }
        validate_name(&self.relationship)?;
        validate_mount(&self.mount)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProductSelectionTarget {
    Root {},
    ContentDependency {
        binding: String,
    },
    /// Exact target-local product testimony for one operation exposed through
    /// an admitted workload-client grant. The workload never receives or
    /// authors this selector; boot admission converts it to an ordinary root
    /// selection only after resolving the exact signed child operation.
    WorkloadExecution {
        item_ref: String,
    },
}

impl ProductSelectionTarget {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Root {} => Ok(()),
            Self::ContentDependency { binding } => validate_binding_name(binding),
            Self::WorkloadExecution { item_ref } => {
                validate_canonical_unsuffixed_ref("workload execution", item_ref)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductSelection {
    pub declaration_id: String,
    pub witness_hash: String,
    pub witness_source: super::transfer::ProductWitnessSource,
    /// Exact qualification testimony selected by the operator, if required by
    /// the signed relationship. Absence never means "find the latest proof".
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub qualification_hash: Option<String>,
}

impl ProductSelection {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_name(&self.declaration_id)?;
        validate_hash("product selection witness", &self.witness_hash)?;
        self.witness_source.validate()?;
        if let Some(hash) = &self.qualification_hash {
            validate_hash("product selection qualification", hash)?;
        }
        Ok(())
    }

    /// Program identity excludes only the receiving node's redemption proof.
    /// Full serialization/equality continues to bind that proof for recovery.
    pub fn semantic_identity_value(&self) -> anyhow::Result<serde_json::Value> {
        self.validate()?;
        let Self {
            declaration_id,
            witness_hash,
            witness_source: _,
            qualification_hash,
        } = self;
        #[derive(Serialize)]
        struct Identity<'a> {
            declaration_id: &'a str,
            witness_hash: &'a str,
            qualification_hash: &'a Option<String>,
        }
        Ok(serde_json::to_value(Identity {
            declaration_id,
            witness_hash,
            qualification_hash,
        })?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductSelectionInput {
    pub target: ProductSelectionTarget,
    pub selection: ProductSelection,
}

impl ProductSelectionInput {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.target.validate()?;
        self.selection.validate()
    }

    pub fn semantic_identity_value(&self) -> anyhow::Result<serde_json::Value> {
        self.validate()?;
        let Self { target, selection } = self;
        #[derive(Serialize)]
        struct Identity<'a> {
            target: &'a ProductSelectionTarget,
            selection: serde_json::Value,
        }
        Ok(serde_json::to_value(Identity {
            target,
            selection: selection.semantic_identity_value()?,
        })?)
    }
}

pub type ProductSelectionInputs = Vec<ProductSelectionInput>;

pub fn product_selection_inputs_semantic_identity(
    inputs: &[ProductSelectionInput],
) -> anyhow::Result<serde_json::Value> {
    validate_product_selection_inputs(inputs)?;
    Ok(serde_json::Value::Array(
        inputs
            .iter()
            .map(ProductSelectionInput::semantic_identity_value)
            .collect::<anyhow::Result<Vec<_>>>()?,
    ))
}

pub fn validate_product_selections(selections: &[ProductSelection]) -> anyhow::Result<()> {
    if selections.len() > MAX_PRODUCT_SELECTIONS
        || lillux::canonical_json(&serde_json::to_value(selections)?)?.len()
            > MAX_PRODUCT_SELECTION_INPUTS_BYTES
    {
        bail!("product selections exceed the bounded contract");
    }
    for selection in selections {
        selection.validate()?;
    }
    if selections
        .windows(2)
        .any(|pair| pair[0].declaration_id >= pair[1].declaration_id)
    {
        bail!("product selections must have unique declaration IDs in canonical order");
    }
    Ok(())
}

pub fn canonicalize_product_selection_inputs(
    mut inputs: ProductSelectionInputs,
) -> anyhow::Result<ProductSelectionInputs> {
    inputs.sort_by(|left, right| {
        (&left.target, &left.selection.declaration_id)
            .cmp(&(&right.target, &right.selection.declaration_id))
    });
    validate_product_selection_inputs(&inputs)?;
    Ok(inputs)
}

pub fn validate_product_selection_inputs(inputs: &[ProductSelectionInput]) -> anyhow::Result<()> {
    if inputs.len() > MAX_PRODUCT_SELECTIONS
        || lillux::canonical_json(&serde_json::to_value(inputs)?)?.len()
            > MAX_PRODUCT_SELECTION_INPUTS_BYTES
    {
        bail!("product selections exceed the bounded contract");
    }
    for input in inputs {
        input.validate()?;
    }
    if inputs.windows(2).any(|pair| {
        (&pair[0].target, &pair[0].selection.declaration_id)
            >= (&pair[1].target, &pair[1].selection.declaration_id)
    }) {
        bail!("product selections must be in canonical unique target/declaration order");
    }
    Ok(())
}

/// State-neutral effective declaration. The engine derives its locator-free,
/// ordinary pinned declaration mechanically from these exact fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedProductDeclaration {
    pub id: String,
    pub kind: ExternalContentKind,
    pub manifest_hash: String,
    pub mount_root: ExternalContentMountRoot,
    pub mount: String,
}

impl ResolvedProductDeclaration {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_name(&self.id)?;
        validate_hash("resolved product manifest", &self.manifest_hash)?;
        validate_mount(&self.mount)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedExternalProductSelection {
    pub schema: String,
    pub declaration_id: String,
    pub relationship_name: String,
    pub relationship_ref: String,
    pub relationship_raw_content_digest: String,
    pub relationship: ProductRelationship,
    pub witness_hash: String,
    pub witness_source: super::transfer::ProductWitnessSource,
    pub witness_coordinate: ProductCaptureCoordinate,
    #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
    pub qualification: Option<AdmittedProductQualification>,
    pub producer: ProductProducerAdmission,
    pub owner_principal: String,
    pub consumer_source: ResolvedProductConsumerSource,
    pub pre_selection_effective_definition_digest: String,
    pub manifest_hash: String,
    pub manifest_kind: String,
    pub declaration: ResolvedProductDeclaration,
}

/// Bounded source authority that existed before product selection. This is
/// deliberately not an external-content binding authority: a pinned-project
/// binding also contains selected D1 and therefore cannot be embedded in the
/// D0 -> D1 selection projection without a cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolvedProductConsumerSource {
    InstalledBundle {
        consumer_ref: String,
        publisher_fingerprint: String,
    },
    PinnedProject {
        consumer_ref: String,
        publisher_fingerprint: String,
        project_snapshot_hash: String,
        #[serde(deserialize_with = "crate::objects::deserialize_required_nullable")]
        source_closure: Option<EffectiveSourceClosureProjection>,
    },
}

impl ResolvedProductConsumerSource {
    pub fn consumer_ref(&self) -> &str {
        match self {
            Self::InstalledBundle { consumer_ref, .. }
            | Self::PinnedProject { consumer_ref, .. } => consumer_ref,
        }
    }

    pub fn publisher_fingerprint(&self) -> &str {
        match self {
            Self::InstalledBundle {
                publisher_fingerprint,
                ..
            }
            | Self::PinnedProject {
                publisher_fingerprint,
                ..
            } => publisher_fingerprint,
        }
    }

    pub fn source_closure(&self) -> Option<&EffectiveSourceClosureProjection> {
        match self {
            Self::InstalledBundle { .. } => None,
            Self::PinnedProject { source_closure, .. } => source_closure.as_ref(),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_canonical_unsuffixed_ref("resolved product consumer", self.consumer_ref())?;
        validate_hash(
            "resolved product consumer publisher",
            self.publisher_fingerprint(),
        )?;
        if let Self::PinnedProject {
            project_snapshot_hash,
            source_closure,
            ..
        } = self
        {
            validate_hash("resolved product consumer snapshot", project_snapshot_hash)?;
            if let Some(source_closure) = source_closure {
                source_closure.validate()?;
            }
        }
        Ok(())
    }
}

/// Exact proof admitted alongside the product. Retained evidence supports
/// recovery without selecting today's policy or mutable head; the attestation
/// hash remains an owning CAS edge, not an arbitrary hash in returned JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedProductQualification {
    pub attestation_hash: String,
    pub evidence: super::qualification::ProductQualificationEvidence,
}

impl ResolvedExternalProductSelection {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != RESOLVED_EXTERNAL_PRODUCT_SELECTION_SCHEMA {
            bail!("unsupported resolved external product selection schema");
        }
        validate_name(&self.declaration_id)?;
        validate_name(&self.relationship_name)?;
        validate_canonical_unsuffixed_ref("resolved product relationship", &self.relationship_ref)?;
        if !self.relationship_ref.starts_with("config:") {
            bail!("resolved product relationship must be an exact Config ref");
        }
        for (label, hash) in [
            (
                "resolved product relationship source",
                &self.relationship_raw_content_digest,
            ),
            ("resolved product witness", &self.witness_hash),
            (
                "resolved product pre-selection definition",
                &self.pre_selection_effective_definition_digest,
            ),
            ("resolved product manifest", &self.manifest_hash),
        ] {
            validate_hash(label, hash)?;
        }
        let owner = self
            .owner_principal
            .strip_prefix("fp:")
            .context("resolved product owner must be an exact fingerprint principal")?;
        validate_hash("resolved product owner", owner)?;
        self.consumer_source.validate()?;
        self.witness_source.validate()?;
        self.witness_coordinate.validate()?;
        self.producer.validate()?;
        self.relationship.validate()?;
        match (
            &self.relationship.qualification.policy_ref,
            &self.qualification,
        ) {
            (None, None) => {}
            (Some(policy_ref), Some(proof)) => {
                validate_hash("admitted product qualification", &proof.attestation_hash)?;
                proof.evidence.validate()?;
                if proof.evidence.product_witness_hash != self.witness_hash
                    || proof.evidence.witness_source != self.witness_source
                    || proof.evidence.product_coordinate != self.witness_coordinate
                    || &proof.evidence.policy_source.canonical_ref != policy_ref
                    || proof.evidence.result.subject_manifest_hash != self.manifest_hash
                {
                    bail!("admitted qualification contradicts selected product or policy");
                }
                proof.evidence.result.validate_claims_for(
                    &proof.evidence.policy_source.policy,
                    &self.relationship.qualification.required_claims,
                )?;
            }
            _ => bail!("product selection requires exactly its declared qualification proof"),
        }
        self.declaration.validate()?;
        let allowed_parameters_digest = self.relationship.producer.admitted_parameters_digest()?;
        let required_kind = match self.relationship.required_product.storage {
            ProductStorage::Content => EXTERNAL_CONTENT_MANIFEST_KIND,
            ProductStorage::LargeContent => EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
        };
        let required_shape = match self.relationship.required_product.shape {
            ProductShape::File => ExternalContentKind::File,
            ProductShape::Tree => ExternalContentKind::Tree,
        };
        if self.relationship_name != self.relationship.name
            || self.consumer_source.consumer_ref() != self.relationship.consumer.canonical_ref
            || self.declaration_id != self.relationship.consumer.declaration_id
            || self.producer.canonical_ref != self.relationship.producer.canonical_ref
            || self.producer.admitted_parameters_digest != allowed_parameters_digest
            || self.witness_coordinate.owner_principal != self.owner_principal
            || self.witness_coordinate.recipe_binding != self.relationship.producer.recipe_binding
            || self.witness_coordinate.product_name != self.relationship.producer.product_name
            || self.manifest_kind != required_kind
            || self.manifest_hash != self.declaration.manifest_hash
            || self.declaration_id != self.declaration.id
            || self.declaration.kind != required_shape
        {
            bail!("resolved external product selection fields contradict one another");
        }
        Ok(())
    }

    pub fn semantic_identity_value(&self) -> anyhow::Result<serde_json::Value> {
        self.validate()?;
        let Self {
            schema,
            declaration_id,
            relationship_name,
            relationship_ref,
            relationship_raw_content_digest,
            relationship,
            witness_hash,
            witness_source: _,
            witness_coordinate,
            qualification,
            producer,
            owner_principal,
            consumer_source,
            pre_selection_effective_definition_digest,
            manifest_hash,
            manifest_kind,
            declaration,
        } = self;
        #[derive(Serialize)]
        struct Qualification<'a> {
            attestation_hash: &'a str,
            evidence: serde_json::Value,
        }
        #[derive(Serialize)]
        struct Identity<'a> {
            schema: &'a str,
            declaration_id: &'a str,
            relationship_name: &'a str,
            relationship_ref: &'a str,
            relationship_raw_content_digest: &'a str,
            relationship: &'a ProductRelationship,
            witness_hash: &'a str,
            witness_coordinate: &'a ProductCaptureCoordinate,
            qualification: Option<Qualification<'a>>,
            producer: &'a ProductProducerAdmission,
            owner_principal: &'a str,
            consumer_source: &'a ResolvedProductConsumerSource,
            pre_selection_effective_definition_digest: &'a str,
            manifest_hash: &'a str,
            manifest_kind: &'a str,
            declaration: &'a ResolvedProductDeclaration,
        }
        let qualification = qualification
            .as_ref()
            .map(|proof| {
                let AdmittedProductQualification {
                    attestation_hash,
                    evidence,
                } = proof;
                Ok::<_, anyhow::Error>(Qualification {
                    attestation_hash,
                    evidence: evidence.semantic_identity_value()?,
                })
            })
            .transpose()?;
        Ok(serde_json::to_value(Identity {
            schema,
            declaration_id,
            relationship_name,
            relationship_ref,
            relationship_raw_content_digest,
            relationship,
            witness_hash,
            witness_coordinate,
            qualification,
            producer,
            owner_principal,
            consumer_source,
            pre_selection_effective_definition_digest,
            manifest_hash,
            manifest_kind,
            declaration,
        })?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResolvedExternalProductSelections(BTreeMap<String, ResolvedExternalProductSelection>);

impl ResolvedExternalProductSelections {
    pub fn semantic_identity_value(&self) -> anyhow::Result<serde_json::Value> {
        self.validate()?;
        let projected = self
            .0
            .iter()
            .map(|(id, selection)| Ok((id.clone(), selection.semantic_identity_value()?)))
            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
        Ok(serde_json::to_value(projected)?)
    }
    pub fn new(
        selections: BTreeMap<String, ResolvedExternalProductSelection>,
    ) -> anyhow::Result<Self> {
        let result = Self(selections);
        result.validate()?;
        Ok(result)
    }

    pub fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (&String, &ResolvedExternalProductSelection)> {
        self.0.iter()
    }

    pub fn get(&self, declaration_id: &str) -> Option<&ResolvedExternalProductSelection> {
        self.0.get(declaration_id)
    }

    pub fn as_map(&self) -> &BTreeMap<String, ResolvedExternalProductSelection> {
        &self.0
    }

    pub fn into_inner(self) -> BTreeMap<String, ResolvedExternalProductSelection> {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.0.is_empty()
            || self.0.len() > MAX_PRODUCT_SELECTIONS
            || lillux::canonical_json(&serde_json::to_value(self)?)?.len()
                > MAX_RESOLVED_PRODUCT_SELECTIONS_BYTES
        {
            bail!("resolved product selections exceed the bounded contract");
        }
        let mut mounts = BTreeSet::new();
        let mut target_source = None;
        let mut target_d0 = None;
        for (declaration_id, selection) in &self.0 {
            selection.validate()?;
            if declaration_id != &selection.declaration_id {
                bail!("resolved product selection map key differs from its declaration id");
            }
            if !mounts.insert((
                selection.declaration.mount_root,
                &selection.declaration.mount,
            )) {
                bail!("resolved product selections repeat an exact mount");
            }
            if target_source
                .replace(&selection.consumer_source)
                .is_some_and(|source| source != &selection.consumer_source)
                || target_d0
                    .replace(selection.pre_selection_effective_definition_digest.as_str())
                    .is_some_and(|d0| {
                        d0 != selection.pre_selection_effective_definition_digest.as_str()
                    })
            {
                bail!("resolved product selections do not share one exact consumer D0 source");
            }
        }
        Ok(())
    }
}

/// Transform only the typed reserved selection slot in a retained resolution.
/// The caller retains the original resolution for admission, recovery and GC;
/// this clone is exclusively an executable-identity input.
pub fn project_resolution_product_selections_for_identity(
    resolution: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let mut projected = resolution.clone();
    if let Some(value) = resolution
        .get("composed")
        .and_then(|v| v.get("derived"))
        .and_then(|v| v.get(EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY))
    {
        let selections: ResolvedExternalProductSelections = serde_json::from_value(value.clone())?;
        let identity = selections.semantic_identity_value()?;
        projected["composed"]["derived"][EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY] = identity;
    }
    Ok(projected)
}

fn validate_mount(mount: &str) -> anyhow::Result<()> {
    validate_canonical_project_relative_path(mount)?;
    if mount.len() > MAX_EXTERNAL_CONTENT_PATH_BYTES {
        bail!("external product mount exceeds the bounded path contract");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn receipt_source_is_retained_but_not_selected_program_identity() {
        use super::super::transfer::ProductWitnessSource;
        let local = super::ProductSelection {
            declaration_id: "runtime".into(),
            witness_hash: "a".repeat(64),
            witness_source: ProductWitnessSource::LocalCapture {},
            qualification_hash: None,
        };
        let semantic = local.semantic_identity_value().unwrap();
        for hash in ["b".repeat(64), "c".repeat(64)] {
            let mut received = local.clone();
            received.witness_source = ProductWitnessSource::Received {
                acceptance_hash: hash,
            };
            assert_ne!(
                serde_json::to_value(&local).unwrap(),
                serde_json::to_value(&received).unwrap()
            );
            assert_eq!(semantic, received.semantic_identity_value().unwrap());
        }
        let mut changed = local.clone();
        changed.witness_hash = "d".repeat(64);
        assert_ne!(semantic, changed.semantic_identity_value().unwrap());
        changed.witness_source = ProductWitnessSource::Received {
            acceptance_hash: "invalid".into(),
        };
        assert!(changed.semantic_identity_value().is_err());
        let mut missing = serde_json::to_value(&local).unwrap();
        missing.as_object_mut().unwrap().remove("witness_source");
        assert!(serde_json::from_value::<super::ProductSelection>(missing).is_err());

        let evidence = super::super::qualification::tests::dynamic_evidence();
        let original = evidence.verifier_root_selections.unwrap();
        let mut received = original.clone().into_inner();
        for selected in received.values_mut() {
            selected.witness_source = ProductWitnessSource::Received {
                acceptance_hash: "b".repeat(64),
            };
        }
        let received = super::ResolvedExternalProductSelections::new(received).unwrap();
        assert_ne!(original, received);
        assert_eq!(
            original.semantic_identity_value().unwrap(),
            received.semantic_identity_value().unwrap()
        );
    }

    use serde_json::json;

    use super::*;
    use crate::external_content::products::{PRODUCT_DECLARATIONS_SCHEMA, ProductDeclaration};

    fn declarations() -> ProductDeclarations {
        ProductDeclarations {
            schema: PRODUCT_DECLARATIONS_SCHEMA.to_owned(),
            output_roots: Vec::new(),
            products: vec![ProductDeclaration {
                name: "runtime".to_owned(),
                source: crate::external_content::products::ProductSource::RetainedProject {},
                path: "products/runtime".to_owned(),
                shape: ProductShape::Tree,
                storage: ProductStorage::Content,
                required: true,
                bounds: ProductBounds {
                    maximum_entries: 8,
                    maximum_depth: 4,
                    maximum_file_bytes: 1_024,
                    maximum_total_bytes: 4_096,
                },
                expected_manifest_hash: None,
            }],
        }
    }

    fn relationship() -> ProductRelationship {
        ProductRelationship {
            name: "runtime_to_consumer".to_owned(),
            producer: ProductRelationshipProducer {
                canonical_ref: "graph:test/producer".to_owned(),
                recipe_binding: "product_recipe".to_owned(),
                product_name: "runtime".to_owned(),
                parameters: json!({"profile": "release", "target": "test"}),
            },
            consumer: ProductRelationshipConsumer {
                canonical_ref: "config:test/consumer".to_owned(),
                declaration_id: "runtime".to_owned(),
            },
            required_product: ProductRelationshipRequiredProduct {
                shape: ProductShape::Tree,
                storage: ProductStorage::Content,
                bounds: ProductBounds {
                    maximum_entries: 4,
                    maximum_depth: 3,
                    maximum_file_bytes: 512,
                    maximum_total_bytes: 2_048,
                },
            },
            qualification: ProductRelationshipQualification {
                policy_ref: None,
                required_claims: Vec::new(),
            },
        }
    }

    fn evidence(relationship: ProductRelationship) -> ProductCaptureEvidence {
        let declarations = declarations();
        let declaration = declarations.products[0].clone();
        ProductCaptureEvidence {
            schema: crate::external_content::products::PRODUCT_CAPTURE_EVIDENCE_SCHEMA,
            owner_principal: format!("fp:{}", "a".repeat(64)),
            chain_root_id: "T-root".to_owned(),
            thread_id: "T-terminal".to_owned(),
            admitted_launch_capsule_hash: "b".repeat(64),
            root_producer: ProductProducerAdmission {
                canonical_ref: relationship.producer.canonical_ref.clone(),
                effective_definition_digest: "1".repeat(64),
                exact_program_hash: "2".repeat(64),
                producer_project_snapshot_hash: "3".repeat(64),
                launch_authority_digest: "4".repeat(64),
                admitted_parameters_digest: relationship
                    .producer
                    .admitted_parameters_digest()
                    .unwrap(),
            },
            producer: ProductProducerAdmission {
                canonical_ref: relationship.producer.canonical_ref.clone(),
                effective_definition_digest: "1".repeat(64),
                exact_program_hash: "2".repeat(64),
                producer_project_snapshot_hash: "3".repeat(64),
                launch_authority_digest: "4".repeat(64),
                admitted_parameters_digest: relationship
                    .producer
                    .admitted_parameters_digest()
                    .unwrap(),
            },
            result_project_snapshot_hash: "c".repeat(64),
            workspace_output_capture_hash: None,
            producer_partition_identity: None,
            recipe_binding: relationship.producer.recipe_binding.clone(),
            recipe_ref: "config:test/recipe".to_owned(),
            recipe_raw_content_digest: "d".repeat(64),
            declarations_hash: declarations.content_hash().unwrap(),
            declarations,
            relationships: ProductRelationships {
                schema: PRODUCT_RELATIONSHIPS_SCHEMA.to_owned(),
                relationships: vec![relationship],
            },
            declaration,
            capture_policy_digest: "e".repeat(64),
            manifest_hash: "f".repeat(64),
            manifest_kind: EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            entry_count: 1,
            total_bytes: 7,
        }
    }

    #[test]
    fn relationship_is_closed_bounded_and_only_narrows_a_declared_product() {
        let relationships = ProductRelationships {
            schema: PRODUCT_RELATIONSHIPS_SCHEMA.to_owned(),
            relationships: vec![relationship()],
        };
        relationships
            .validate_against(&declarations(), "product_recipe")
            .unwrap();

        let mut widened = relationships.clone();
        widened.relationships[0]
            .required_product
            .bounds
            .maximum_total_bytes = 8_192;
        assert!(
            widened
                .validate_against(&declarations(), "product_recipe")
                .is_err()
        );

        let mut qualified = relationships;
        qualified.relationships[0].qualification.policy_ref =
            Some("policy:test/compatibility".to_owned());
        assert!(qualified.validate().is_err());
    }

    #[test]
    fn relationship_parameter_projection_is_canonical() {
        let mut left = relationship();
        let mut right = left.clone();
        left.producer.parameters = json!({"target": "test", "profile": "release"});
        right.producer.parameters = json!({"profile": "release", "target": "test"});
        assert_eq!(
            left.producer.admitted_parameters_digest().unwrap(),
            right.producer.admitted_parameters_digest().unwrap()
        );
        let value = serde_json::to_value(left).unwrap();
        assert!(value.pointer("/producer/parameters_digest").is_none());
    }

    #[test]
    fn relationship_matches_retained_producer_product_and_parameter_testimony() {
        let relationship = relationship();
        let evidence = evidence(relationship.clone());
        relationship.validate_product_evidence(&evidence).unwrap();

        let mut changed = relationship.clone();
        changed.producer.parameters = json!({"profile": "debug", "target": "test"});
        assert!(changed.validate_product_evidence(&evidence).is_err());

        let mut changed = relationship;
        changed.producer.canonical_ref = "graph:test/other".to_owned();
        assert!(changed.validate_product_evidence(&evidence).is_err());
    }

    #[test]
    fn selector_list_is_typed_canonical_and_closed() {
        let one = ProductSelection {
            declaration_id: "runtime".to_owned(),
            witness_hash: "a".repeat(64),
            witness_source: super::super::transfer::ProductWitnessSource::LocalCapture {},
            qualification_hash: None,
        };
        let root = ProductSelectionInput {
            target: ProductSelectionTarget::Root {},
            selection: one.clone(),
        };
        let dependency = ProductSelectionInput {
            target: ProductSelectionTarget::ContentDependency {
                binding: "environment".to_owned(),
            },
            selection: one,
        };
        let canonical =
            canonicalize_product_selection_inputs(vec![dependency.clone(), root.clone()]).unwrap();
        assert_eq!(canonical, vec![root.clone(), dependency.clone()]);
        validate_product_selection_inputs(&canonical).unwrap();
        assert!(validate_product_selection_inputs(&vec![dependency, root.clone()]).is_err());
        assert!(validate_product_selection_inputs(&vec![root.clone(), root]).is_err());
        assert!(
            serde_json::from_value::<ProductSelectionInput>(json!({
                "target":{"kind":"root"},
                "selection":{
                    "declaration_id": "runtime",
                    "witness_hash": "a".repeat(64),
                    "witness_source": {"kind":"local_capture"},
                    "qualification_hash": null
                },
                "extra": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ProductSelectionInput>(json!({
                "target":{"kind":"root"},
                "selection":{
                    "declaration_id": "runtime",
                    "witness_hash": "a".repeat(64),
                    "witness_source": {"kind":"local_capture"}
                }
            }))
            .is_err()
        );
    }

    #[test]
    fn qualified_relationship_requires_exact_retained_proof() {
        let mut relationship = relationship();
        let mut proof = super::super::qualification::tests::evidence();
        relationship.qualification = ProductRelationshipQualification {
            policy_ref: Some(proof.policy_source.canonical_ref.clone()),
            required_claims: vec!["command_probe".to_owned()],
        };
        let witness = evidence(relationship.clone());
        relationship.validate_product_evidence(&witness).unwrap();
        let coordinate = ProductCaptureCoordinate::from_evidence(&witness).unwrap();
        proof.product_coordinate = coordinate.clone();
        proof.product_witness_hash = "9".repeat(64);
        proof.result.subject_manifest_hash = witness.manifest_hash.clone();
        proof.verifier.subject_manifest_hash = witness.manifest_hash.clone();
        proof.verifier.result_digest = proof.result.digest().unwrap();
        let selection = ResolvedExternalProductSelection {
            schema: RESOLVED_EXTERNAL_PRODUCT_SELECTION_SCHEMA.to_owned(),
            declaration_id: "runtime".to_owned(),
            relationship_name: relationship.name.clone(),
            relationship_ref: witness.recipe_ref.clone(),
            relationship_raw_content_digest: witness.recipe_raw_content_digest.clone(),
            relationship,
            witness_hash: proof.product_witness_hash.clone(),
            witness_source: proof.witness_source.clone(),
            witness_coordinate: coordinate,
            qualification: Some(AdmittedProductQualification {
                attestation_hash: "8".repeat(64),
                evidence: proof,
            }),
            producer: witness.producer.clone(),
            owner_principal: witness.owner_principal.clone(),
            consumer_source: ResolvedProductConsumerSource::PinnedProject {
                consumer_ref: "config:test/consumer".to_owned(),
                publisher_fingerprint: "7".repeat(64),
                project_snapshot_hash: "6".repeat(64),
                source_closure: None,
            },
            pre_selection_effective_definition_digest: "5".repeat(64),
            manifest_hash: witness.manifest_hash.clone(),
            manifest_kind: witness.manifest_kind.clone(),
            declaration: ResolvedProductDeclaration {
                id: "runtime".to_owned(),
                kind: ExternalContentKind::Tree,
                manifest_hash: witness.manifest_hash,
                mount_root: ExternalContentMountRoot::ExecutionRuntime,
                mount: "runtime".to_owned(),
            },
        };
        selection.validate().unwrap();
        let mut missing = selection.clone();
        missing.qualification = None;
        assert!(missing.validate().is_err());
        let mut unrelated = selection.clone();
        unrelated.relationship.qualification = ProductRelationshipQualification {
            policy_ref: None,
            required_claims: Vec::new(),
        };
        assert!(unrelated.validate().is_err());
        for field in [
            "product_witness_hash",
            "policy",
            "subject",
            "claims",
            "owner",
        ] {
            let mut changed = selection.clone();
            let proof = &mut changed.qualification.as_mut().unwrap().evidence;
            match field {
                "product_witness_hash" => proof.product_witness_hash = "4".repeat(64),
                "policy" => proof.policy_source.canonical_ref = "config:test/other".to_owned(),
                "subject" => proof.result.subject_manifest_hash = "3".repeat(64),
                "claims" => proof.result.claims = vec!["extension_probe".to_owned()],
                "owner" => {
                    proof.product_coordinate.owner_principal = format!("fp:{}", "2".repeat(64))
                }
                _ => unreachable!(),
            }
            assert!(changed.validate().is_err(), "{field}");
        }
        let mut wire = serde_json::to_value(&selection).unwrap();
        wire.as_object_mut().unwrap().remove("qualification");
        assert!(serde_json::from_value::<ResolvedExternalProductSelection>(wire).is_err());

        let mut wire = serde_json::to_value(selection).unwrap();
        wire.pointer_mut("/consumer_source")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("source_closure");
        assert!(serde_json::from_value::<ResolvedExternalProductSelection>(wire).is_err());
    }

    #[test]
    fn slot_requires_exact_config_relationship_and_canonical_mount() {
        let slot = ExternalProductSlotDeclaration {
            id: "runtime".to_owned(),
            relationship_ref: "config:test/recipe".to_owned(),
            relationship: "runtime_to_consumer".to_owned(),
            kind: ExternalContentKind::Tree,
            mount_root: ExternalContentMountRoot::ExecutionRuntime,
            mount: "runtime".to_owned(),
        };
        slot.validate().unwrap();
        let mut changed = slot;
        changed.relationship_ref = "graph:test/recipe".to_owned();
        assert!(changed.validate().is_err());
    }

    #[test]
    fn consumer_identity_defers_kind_eligibility_to_signed_admission() {
        let mut consumer = relationship().consumer;
        for reference in [
            "config:test/consumer",
            "tool:test/verify",
            "graph:test/verify",
            "worker:test/consumer",
            "directive:test/consumer",
            "custom_consumer:test/consumer",
        ] {
            consumer.canonical_ref = reference.to_owned();
            consumer.validate().unwrap();
            ResolvedProductConsumerSource::InstalledBundle {
                consumer_ref: reference.to_owned(),
                publisher_fingerprint: "a".repeat(64),
            }
            .validate()
            .unwrap();
        }
        for reference in [
            "worker:test/consumer@latest",
            "custom_consumer:test/*",
            "not a ref",
        ] {
            consumer.canonical_ref = reference.to_owned();
            assert!(consumer.validate().is_err());
            assert!(
                ResolvedProductConsumerSource::InstalledBundle {
                    consumer_ref: reference.to_owned(),
                    publisher_fingerprint: "a".repeat(64),
                }
                .validate()
                .is_err()
            );
        }
    }
}
