//! Recovery-safe projection of the product recipe admitted with a producer.
//!
//! A capture caller supplies only the binding name. The recipe identity,
//! exact source digest, and declarations are recovered from the producer's
//! admitted launch capsule and checked against its sealed ref-binding record.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

use super::composition::{
    ProductRelationship, ProductRelationshipConsumer, ProductRelationshipProducer,
    ProductRelationshipQualification, ProductRelationshipRequiredProduct, ProductRelationships,
};
use super::{
    ProductDeclarations, ProductProducerAdmission, ProductRecipePurpose, validate_binding_name,
};
use crate::objects::{AdmittedExecutionClosure, AdmittedLaunchCapsule};

pub const PRODUCT_RECIPE_BINDING_SCHEMA: &str = "ryeos.admitted_product_recipe_binding.v2";
/// The runtime-fact wire format interns repeated producer parameter values.
/// It expands to `PRODUCT_RECIPE_BINDING_SCHEMA` before any product testimony
/// or operator-side validation sees the recipe.
pub const PRODUCT_RECIPE_BINDING_FACT_SCHEMA: &str = "ryeos.admitted_product_recipe_fact.v1";
pub const MAX_ADMITTED_PRODUCT_RECIPE_BYTES: usize = 16 * 1024;

/// Derive the compact producer projection exclusively from one validated
/// admitted capsule. Raw invocation parameters never enter product testimony;
/// the application additionally compares this digest with its canonical
/// sealed-request projection before publication.
pub fn admitted_product_producer(
    capsule: &AdmittedLaunchCapsule,
) -> anyhow::Result<ProductProducerAdmission> {
    capsule.validate()?;
    let parameters = capsule
        .sealed_invocation
        .get("parameters")
        .context("producer capsule has no admitted parameters")?;
    let observed_parameters_digest = crate::objects::canonical_value_digest(parameters)?;
    let canonical_ref = capsule
        .exact_program
        .get("item_ref")
        .and_then(serde_json::Value::as_str)
        .context("producer exact program has no canonical ref")?;
    let resolution_ref = capsule
        .exact_program
        .pointer("/resolution_output/root/resolved_ref")
        .and_then(serde_json::Value::as_str)
        .context("producer exact program has no resolved root ref")?;
    if canonical_ref != resolution_ref {
        bail!("producer exact program root ref changed");
    }
    let effective_definition_digest = capsule
        .exact_program
        .get("effective_definition_digest")
        .and_then(serde_json::Value::as_str)
        .context("producer exact program has no effective definition digest")?;
    let producer_project_snapshot_hash = capsule
        .project_authority
        .operational_snapshot_projection()
        .context("product producer does not have a pinned project generation")?;
    let result = ProductProducerAdmission {
        canonical_ref: canonical_ref.to_owned(),
        effective_definition_digest: effective_definition_digest.to_owned(),
        exact_program_hash: capsule.exact_program_hash.clone(),
        producer_project_snapshot_hash: producer_project_snapshot_hash.to_owned(),
        launch_authority_digest: capsule.launch_authority_digest()?,
        admitted_parameters_digest: observed_parameters_digest,
    };
    result.validate()?;
    Ok(result)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedProductRecipeBinding {
    pub schema: String,
    pub binding_name: String,
    pub recipe_ref: String,
    pub recipe_raw_content_digest: String,
    pub purpose: ProductRecipePurpose,
    pub declarations: ProductDeclarations,
    pub declarations_hash: String,
    pub relationships: ProductRelationships,
}

impl AdmittedProductRecipeBinding {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PRODUCT_RECIPE_BINDING_SCHEMA {
            bail!("unsupported admitted product recipe binding schema");
        }
        validate_binding_name(&self.binding_name)?;
        super::validate_canonical_unsuffixed_ref("admitted product recipe", &self.recipe_ref)?;
        if !self.recipe_ref.starts_with("config:") {
            bail!("admitted product recipe must be an exact Config ref");
        }
        if !lillux::valid_hash(&self.recipe_raw_content_digest)
            || self
                .recipe_raw_content_digest
                .bytes()
                .any(|byte| byte.is_ascii_uppercase())
        {
            bail!("admitted product recipe source digest is not canonical");
        }
        if self.declarations.content_hash()? != self.declarations_hash {
            bail!("admitted product declarations hash changed");
        }
        self.relationships
            .validate_against(&self.declarations, &self.binding_name)?;
        Ok(())
    }

    /// Encode this complete recipe as the bounded launch runtime fact. Equal
    /// producer-parameter values are stored once only when shared by multiple
    /// relationships; one-off values remain inline. Decoding reconstructs
    /// this full type before product evidence or consumer policy code runs.
    pub fn runtime_fact_value(&self) -> anyhow::Result<serde_json::Value> {
        self.validate()?;
        let fact = CompactAdmittedProductRecipeFact::from_binding(self)?;
        let value = serde_json::to_value(fact)?;
        let bytes = lillux::canonical_json(&value)?.len();
        if bytes > MAX_ADMITTED_PRODUCT_RECIPE_BYTES {
            bail!(
                "admitted product recipe exceeds the runtime-fact budget: encoded size is {bytes} bytes; limit is {MAX_ADMITTED_PRODUCT_RECIPE_BYTES} bytes"
            );
        }
        Ok(value)
    }
}

/// Decode only the current compact fact format. There is deliberately no
/// fallback to expanded/legacy launch facts: new producers must pass the
/// bounded, canonical projection before the complete recipe is recovered.
pub fn admitted_product_recipe_from_runtime_fact(
    value: &serde_json::Value,
) -> anyhow::Result<AdmittedProductRecipeBinding> {
    let encoded_bytes = lillux::canonical_json(value)?.len();
    if encoded_bytes > MAX_ADMITTED_PRODUCT_RECIPE_BYTES {
        bail!(
            "admitted product recipe exceeds the runtime-fact budget: encoded size is {encoded_bytes} bytes; limit is {MAX_ADMITTED_PRODUCT_RECIPE_BYTES} bytes"
        );
    }
    let fact: CompactAdmittedProductRecipeFact = serde_json::from_value(value.clone())
        .context("decode compact admitted product recipe fact")?;
    fact.into_binding()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactAdmittedProductRecipeFact {
    schema: String,
    binding_name: String,
    recipe_ref: String,
    recipe_raw_content_digest: String,
    purpose: ProductRecipePurpose,
    declarations: ProductDeclarations,
    declarations_hash: String,
    relationships: CompactProductRelationships,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactProductRelationships {
    schema: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    parameter_values: Vec<CompactProducerParameters>,
    relationships: Vec<CompactProductRelationship>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactProducerParameters {
    digest: String,
    value: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactProductRelationship {
    name: String,
    producer: CompactProductRelationshipProducer,
    consumer: ProductRelationshipConsumer,
    required_product: ProductRelationshipRequiredProduct,
    qualification: ProductRelationshipQualification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactProductRelationshipProducer {
    canonical_ref: String,
    recipe_binding: String,
    product_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parameters_index: Option<usize>,
}

impl CompactAdmittedProductRecipeFact {
    fn from_binding(binding: &AdmittedProductRecipeBinding) -> anyhow::Result<Self> {
        let mut values = std::collections::BTreeMap::<String, (serde_json::Value, usize)>::new();
        for relationship in &binding.relationships.relationships {
            let value = relationship.producer.parameters.clone();
            let digest = crate::objects::canonical_value_digest(&value)?;
            if let Some((previous, count)) = values.get_mut(&digest) {
                if previous != &value {
                    bail!("producer parameter digest collision in admitted recipe");
                }
                *count += 1;
            } else {
                values.insert(digest, (value, 1));
            }
        }
        let repeated_values = values
            .iter()
            .filter(|(_, (_, count))| *count > 1)
            .map(|(digest, (value, _))| (digest.clone(), value.clone()))
            .collect::<Vec<_>>();
        let parameter_indexes = repeated_values
            .iter()
            .enumerate()
            .map(|(index, (digest, _))| (digest.clone(), index))
            .collect::<std::collections::BTreeMap<_, _>>();
        let parameter_values = repeated_values
            .into_iter()
            .map(|(digest, value)| CompactProducerParameters { digest, value })
            .collect::<Vec<_>>();
        let relationships = binding
            .relationships
            .relationships
            .iter()
            .map(|relationship| {
                let digest =
                    crate::objects::canonical_value_digest(&relationship.producer.parameters)?;
                let parameters_index = parameter_indexes.get(&digest).copied();
                Ok(CompactProductRelationship {
                    name: relationship.name.clone(),
                    producer: CompactProductRelationshipProducer {
                        canonical_ref: relationship.producer.canonical_ref.clone(),
                        recipe_binding: relationship.producer.recipe_binding.clone(),
                        product_name: relationship.producer.product_name.clone(),
                        parameters: parameters_index
                            .is_none()
                            .then(|| relationship.producer.parameters.clone()),
                        parameters_index,
                    },
                    consumer: relationship.consumer.clone(),
                    required_product: relationship.required_product.clone(),
                    qualification: relationship.qualification.clone(),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            schema: PRODUCT_RECIPE_BINDING_FACT_SCHEMA.to_owned(),
            binding_name: binding.binding_name.clone(),
            recipe_ref: binding.recipe_ref.clone(),
            recipe_raw_content_digest: binding.recipe_raw_content_digest.clone(),
            purpose: binding.purpose.clone(),
            declarations: binding.declarations.clone(),
            declarations_hash: binding.declarations_hash.clone(),
            relationships: CompactProductRelationships {
                schema: binding.relationships.schema.clone(),
                parameter_values,
                relationships,
            },
        })
    }

    fn into_binding(self) -> anyhow::Result<AdmittedProductRecipeBinding> {
        if self.schema != PRODUCT_RECIPE_BINDING_FACT_SCHEMA {
            bail!("unsupported compact admitted product recipe fact schema");
        }
        let CompactProductRelationships {
            schema: relationships_schema,
            parameter_values,
            relationships: compact_relationships,
        } = self.relationships;
        let mut values = Vec::with_capacity(parameter_values.len());
        let mut previous_digest: Option<String> = None;
        for entry in parameter_values {
            if !lillux::valid_hash(&entry.digest)
                || entry.digest.bytes().any(|byte| byte.is_ascii_uppercase())
                || previous_digest
                    .as_ref()
                    .is_some_and(|previous| previous >= &entry.digest)
                || crate::objects::canonical_value_digest(&entry.value)? != entry.digest
            {
                bail!("compact producer parameter table is not canonical");
            }
            previous_digest = Some(entry.digest.clone());
            values.push((entry.digest, entry.value));
        }
        let mut indexed_uses = vec![0usize; values.len()];
        let mut inline_uses = std::collections::BTreeMap::<String, usize>::new();
        let mut relationships = Vec::with_capacity(compact_relationships.len());
        for relationship in compact_relationships {
            let parameters = match (
                relationship.producer.parameters,
                relationship.producer.parameters_index,
            ) {
                (Some(parameters), None) => {
                    let digest = crate::objects::canonical_value_digest(&parameters)?;
                    *inline_uses.entry(digest).or_default() += 1;
                    parameters
                }
                (None, Some(index)) => {
                    let (_, parameters) = values
                        .get(index)
                        .context("compact product relationship names an absent parameter value")?;
                    indexed_uses[index] += 1;
                    parameters.clone()
                }
                _ => bail!("compact product relationship must select exactly one parameter value"),
            };
            relationships.push(ProductRelationship {
                name: relationship.name,
                producer: ProductRelationshipProducer {
                    canonical_ref: relationship.producer.canonical_ref,
                    recipe_binding: relationship.producer.recipe_binding,
                    product_name: relationship.producer.product_name,
                    parameters,
                },
                consumer: relationship.consumer,
                required_product: relationship.required_product,
                qualification: relationship.qualification,
            });
        }
        if indexed_uses.iter().any(|count| *count < 2)
            || inline_uses.values().any(|count| *count > 1)
            || inline_uses.keys().any(|digest| {
                values
                    .iter()
                    .any(|(table_digest, _)| table_digest == digest)
            })
        {
            bail!("compact producer parameter table is not canonical");
        }
        let binding = AdmittedProductRecipeBinding {
            schema: PRODUCT_RECIPE_BINDING_SCHEMA.to_owned(),
            binding_name: self.binding_name,
            recipe_ref: self.recipe_ref,
            recipe_raw_content_digest: self.recipe_raw_content_digest,
            purpose: self.purpose,
            declarations: self.declarations,
            declarations_hash: self.declarations_hash,
            relationships: ProductRelationships {
                schema: relationships_schema,
                relationships,
            },
        };
        binding.validate()?;
        Ok(binding)
    }
}

/// Recover the exact product recipe admitted with a managed producer launch.
///
/// The capsule's own validation proves that `binding_records` equals the
/// effective program's resolved ref bindings. This projection additionally
/// proves that the preparer's parsed declaration fact names the same exact
/// Config bytes. No live config lookup or caller-supplied declaration is used.
pub fn admitted_product_recipe(
    capsule: &AdmittedLaunchCapsule,
    binding_name: &str,
) -> anyhow::Result<AdmittedProductRecipeBinding> {
    capsule.validate()?;
    let AdmittedExecutionClosure::ManagedRuntime {
        prepared_runtime_launch,
        ..
    } = &capsule.execution_closure
    else {
        bail!("product recipe admission requires a managed producer launch");
    };
    admitted_product_recipe_from_prepared(prepared_runtime_launch, binding_name)
}

/// Recover the exact recipe fact from a preparer-produced launch value before
/// a capsule exists. The caller must supply only the output of the verified
/// launch preparer; this function validates the closed fact and cross-checks
/// it against that same output's exact binding record.
pub fn admitted_product_recipe_from_prepared(
    prepared: &serde_json::Value,
    binding_name: &str,
) -> anyhow::Result<AdmittedProductRecipeBinding> {
    validate_binding_name(binding_name)?;
    let fact = prepared
        .get("runtime_facts")
        .and_then(|facts| facts.get(binding_name))
        .cloned()
        .with_context(|| {
            format!("producer did not admit product recipe binding `{binding_name}`")
        })?;
    let admitted = admitted_product_recipe_from_runtime_fact(&fact)?;
    if admitted.binding_name != binding_name {
        bail!("admitted product recipe fact belongs to a different binding");
    }

    let binding = prepared
        .get("binding_records")
        .and_then(|records| records.get(binding_name))
        .with_context(|| format!("producer capsule has no binding record `{binding_name}`"))?;
    let canonical_ref = binding
        .get("canonical_ref")
        .and_then(serde_json::Value::as_str)
        .context("admitted product recipe binding has no canonical ref")?;
    let raw_content_digest = binding
        .pointer("/resolution/root/raw_content_digest")
        .and_then(serde_json::Value::as_str)
        .context("admitted product recipe binding has no exact source digest")?;
    if admitted.recipe_ref != canonical_ref
        || admitted.recipe_raw_content_digest != raw_content_digest
    {
        bail!("admitted product recipe fact contradicts its sealed binding record");
    }
    Ok(admitted)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::external_content::products::{
        PRODUCT_DECLARATIONS_SCHEMA, ProductBounds, ProductDeclaration, ProductShape,
        ProductSource, ProductStorage,
    };

    #[test]
    fn admitted_parameter_projection_uses_canonical_digest_without_copying_values() {
        let left = json!({"public": {"target": "example", "profile": "release"}});
        let right = json!({"public": {"profile": "release", "target": "example"}});
        let digest = crate::objects::canonical_value_digest(&left).unwrap();
        assert_eq!(
            crate::objects::canonical_value_digest(&right).unwrap(),
            digest
        );
    }

    fn admitted() -> AdmittedProductRecipeBinding {
        let declarations = ProductDeclarations {
            schema: PRODUCT_DECLARATIONS_SCHEMA.to_owned(),
            output_roots: Vec::new(),
            products: vec![ProductDeclaration {
                name: "runtime".to_owned(),
                source: ProductSource::RetainedProject {},
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
        };
        AdmittedProductRecipeBinding {
            schema: PRODUCT_RECIPE_BINDING_SCHEMA.to_owned(),
            binding_name: "product_recipe".to_owned(),
            recipe_ref: "config:test/two-products".to_owned(),
            recipe_raw_content_digest: "a".repeat(64),
            purpose: ProductRecipePurpose::GeneralProductV1,
            declarations_hash: declarations.content_hash().unwrap(),
            declarations,
            relationships: ProductRelationships::empty(),
        }
    }

    fn prepared(admitted: &AdmittedProductRecipeBinding) -> serde_json::Value {
        json!({
            "runtime_facts": {
                "product_recipe": admitted.runtime_fact_value().unwrap(),
            },
            "binding_records": {
                "product_recipe": {
                    "canonical_ref": admitted.recipe_ref,
                    "source_space": "project",
                    "effective_trust_class": "trusted_project",
                    "resolution": {
                        "root": {
                            "requested_id": admitted.recipe_ref,
                            "resolved_ref": admitted.recipe_ref,
                            "source_space": "project",
                            "source_root": {"kind": "project"},
                            "trust_class": "trusted_project",
                            "signer_fingerprint": "b".repeat(64),
                            "raw_content_digest": admitted.recipe_raw_content_digest,
                        },
                        "ancestors": [],
                        "referenced_items": [],
                        "effective_trust_class": "trusted_project",
                        "policy_facts": {},
                    },
                },
            },
        })
    }

    #[test]
    fn recovers_exact_admitted_recipe_without_live_lookup() {
        let expected = admitted();
        assert_eq!(
            admitted_product_recipe_from_prepared(&prepared(&expected), "product_recipe").unwrap(),
            expected
        );
    }

    fn two_consumer_recipe(parameters: serde_json::Value) -> AdmittedProductRecipeBinding {
        let mut recipe = admitted();
        recipe.relationships = serde_json::from_value(json!({
            "schema": super::super::composition::PRODUCT_RELATIONSHIPS_SCHEMA,
            "relationships": [
                {
                    "name": "to_graph",
                    "producer": {
                        "canonical_ref": "graph:test/producer",
                        "recipe_binding": "product_recipe",
                        "product_name": "runtime",
                        "parameters": parameters.clone()
                    },
                    "consumer": {"canonical_ref": "graph:test/consumer", "declaration_id": "subject"},
                    "required_product": {
                        "shape": "tree", "storage": "content",
                        "bounds": {"maximum_entries":8,"maximum_depth":4,"maximum_file_bytes":1024,"maximum_total_bytes":4096}
                    },
                    "qualification": {"policy_ref": null, "required_claims": []}
                },
                {
                    "name": "to_tool",
                    "producer": {
                        "canonical_ref": "graph:test/producer",
                        "recipe_binding": "product_recipe",
                        "product_name": "runtime",
                        "parameters": parameters
                    },
                    "consumer": {"canonical_ref": "tool:test/consumer", "declaration_id": "subject"},
                    "required_product": {
                        "shape": "tree", "storage": "content",
                        "bounds": {"maximum_entries":8,"maximum_depth":4,"maximum_file_bytes":1024,"maximum_total_bytes":4096}
                    },
                    "qualification": {"policy_ref": null, "required_claims": []}
                }
            ]
        }))
        .unwrap();
        recipe
    }

    #[test]
    fn compact_runtime_fact_interns_parameters_and_round_trips_exact_relationships() {
        let mut selected = None;
        for bytes in (4_000..10_000).step_by(100) {
            let candidate = two_consumer_recipe(json!({"signed_manifest": "x".repeat(bytes)}));
            if candidate.validate().is_ok()
                && lillux::canonical_json(&serde_json::to_value(&candidate).unwrap())
                    .unwrap()
                    .len()
                    > MAX_ADMITTED_PRODUCT_RECIPE_BYTES
            {
                selected = Some(candidate);
                break;
            }
        }
        let expected = selected.expect("fixture must cross only the expanded fact limit");
        let compact = expected.runtime_fact_value().unwrap();
        assert_eq!(compact["schema"], PRODUCT_RECIPE_BINDING_FACT_SCHEMA);
        assert_eq!(
            compact
                .pointer("/relationships/parameter_values")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            lillux::canonical_json(&compact).unwrap().len() <= MAX_ADMITTED_PRODUCT_RECIPE_BYTES
        );
        let decoded = admitted_product_recipe_from_runtime_fact(&compact).unwrap();
        assert_eq!(decoded, expected);
        assert_eq!(
            decoded.relationships.relationships[0]
                .consumer
                .canonical_ref,
            "graph:test/consumer"
        );
        assert_eq!(
            decoded.relationships.relationships[1]
                .consumer
                .canonical_ref,
            "tool:test/consumer"
        );
    }

    #[test]
    fn compact_runtime_fact_keeps_one_off_parameters_inline() {
        let mut expected = two_consumer_recipe(json!({"request":"first"}));
        expected.relationships.relationships[1].producer.parameters = json!({"request":"second"});

        let compact = expected.runtime_fact_value().unwrap();
        let compact_relationships = compact
            .pointer("/relationships/relationships")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(compact.pointer("/relationships/parameter_values").is_none());
        for (relationship, expected_relationship) in compact_relationships
            .iter()
            .zip(&expected.relationships.relationships)
        {
            assert_eq!(
                relationship["producer"]["parameters"],
                expected_relationship.producer.parameters
            );
            assert!(relationship["producer"].get("parameters_index").is_none());
        }
        assert_eq!(
            admitted_product_recipe_from_runtime_fact(&compact).unwrap(),
            expected
        );
    }

    #[test]
    fn compact_runtime_fact_rejects_tampered_or_unused_parameter_entries() {
        let expected = two_consumer_recipe(json!({"request":"exact"}));
        let compact = expected.runtime_fact_value().unwrap();

        let mut wrong_digest = compact.clone();
        wrong_digest["relationships"]["relationships"][0]["producer"]["parameters_index"] =
            json!(usize::MAX);
        assert!(admitted_product_recipe_from_runtime_fact(&wrong_digest).is_err());

        let mut unused = compact.clone();
        let unused_value = json!({"unused": true});
        let unused_entry = json!({
            "digest": crate::objects::canonical_value_digest(&unused_value).unwrap(),
            "value": unused_value,
        });
        let unused_values = unused["relationships"]["parameter_values"]
            .as_array_mut()
            .unwrap();
        unused_values.push(unused_entry);
        unused_values.sort_by(|left, right| left["digest"].as_str().cmp(&right["digest"].as_str()));
        assert!(admitted_product_recipe_from_runtime_fact(&unused).is_err());

        let mut old_expanded = serde_json::to_value(&expected).unwrap();
        old_expanded["schema"] = json!(PRODUCT_RECIPE_BINDING_SCHEMA);
        assert!(admitted_product_recipe_from_runtime_fact(&old_expanded).is_err());
    }

    #[test]
    fn compact_runtime_fact_budget_error_reports_actual_encoded_size() {
        let mut oversized = None;
        for payload_bytes in (4_000..8_000).step_by(100) {
            let mut candidate = two_consumer_recipe(json!({
                "signed_manifest": "x".repeat(payload_bytes)
            }));
            candidate.relationships.relationships[1].producer.parameters = json!({
                "signed_manifest": "y".repeat(payload_bytes)
            });
            if !candidate.validate().is_ok() {
                continue;
            }
            let fact = CompactAdmittedProductRecipeFact::from_binding(&candidate).unwrap();
            let value = serde_json::to_value(fact).unwrap();
            let encoded_bytes = lillux::canonical_json(&value).unwrap().len();
            if encoded_bytes > MAX_ADMITTED_PRODUCT_RECIPE_BYTES {
                oversized = Some((candidate, value, encoded_bytes));
                break;
            }
        }
        let (oversized, value, encoded_bytes) = oversized
            .expect("valid distinct relationship parameters must exceed compact-fact limit");

        let expected_error = format!(
            "admitted product recipe exceeds the runtime-fact budget: encoded size is {encoded_bytes} bytes; limit is {MAX_ADMITTED_PRODUCT_RECIPE_BYTES} bytes"
        );
        assert_eq!(
            oversized.runtime_fact_value().unwrap_err().to_string(),
            expected_error
        );
        assert_eq!(
            admitted_product_recipe_from_runtime_fact(&value)
                .unwrap_err()
                .to_string(),
            expected_error
        );
    }

    #[test]
    fn refuses_missing_wrong_name_or_relabelled_recipe() {
        let expected = admitted();
        let value = prepared(&expected);
        assert!(admitted_product_recipe_from_prepared(&value, "other").is_err());

        let mut changed = value.clone();
        changed["runtime_facts"]["product_recipe"]["binding_name"] = json!("other");
        assert!(admitted_product_recipe_from_prepared(&changed, "product_recipe").is_err());

        let mut changed = value;
        changed["runtime_facts"]["product_recipe"]["recipe_raw_content_digest"] =
            json!("c".repeat(64));
        assert!(admitted_product_recipe_from_prepared(&changed, "product_recipe").is_err());

        let mut changed = prepared(&expected);
        changed["runtime_facts"]["product_recipe"]["recipe_ref"] =
            json!("config:test/build+not-canonical");
        changed["binding_records"]["product_recipe"]["canonical_ref"] =
            json!("config:test/build+not-canonical");
        assert!(admitted_product_recipe_from_prepared(&changed, "product_recipe").is_err());
    }

    #[test]
    fn refuses_mutated_declarations_even_with_a_valid_config_digest() {
        let expected = admitted();
        let mut value = prepared(&expected);
        value["runtime_facts"]["product_recipe"]["declarations"]["products"][0]["path"] =
            json!("products/other");
        assert!(admitted_product_recipe_from_prepared(&value, "product_recipe").is_err());
    }
}
