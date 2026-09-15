//! Recovery-safe projection of the product recipe admitted with a producer.
//!
//! A capture caller supplies only the binding name. The recipe identity,
//! exact source digest, and declarations are recovered from the producer's
//! admitted launch capsule and checked against its sealed ref-binding record.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

use super::composition::ProductRelationships;
use super::{ProductDeclarations, ProductProducerAdmission, validate_binding_name};
use crate::objects::{AdmittedExecutionClosure, AdmittedLaunchCapsule};

pub const PRODUCT_RECIPE_BINDING_SCHEMA: &str = "ryeos.admitted_product_recipe_binding.v1";
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
        if lillux::canonical_json(&serde_json::to_value(self)?)?.len()
            > MAX_ADMITTED_PRODUCT_RECIPE_BYTES
        {
            bail!("admitted product recipe exceeds the runtime-fact budget");
        }
        Ok(())
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
    let admitted: AdmittedProductRecipeBinding =
        serde_json::from_value(fact).context("decode admitted product recipe binding")?;
    admitted.validate()?;
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
            declarations_hash: declarations.content_hash().unwrap(),
            declarations,
            relationships: ProductRelationships::empty(),
        }
    }

    fn prepared(admitted: &AdmittedProductRecipeBinding) -> serde_json::Value {
        json!({
            "runtime_facts": {
                "product_recipe": serde_json::to_value(admitted).unwrap(),
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
