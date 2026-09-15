//! Pure launch preparation for optional graph-owned product recipes.

use std::collections::BTreeMap;

use ryeos_handler_protocol::{
    ExternalEffectAuthorityDeclWire, ExternalEffectAuthorityResultWire, FinancialAuthorityDeclWire,
    FinancialAuthorityResultWire, HandlerResponse, ItemSpaceWire, LaunchDiagnosticScalarWire,
    LaunchPrepareError, LaunchPrepareErrorClass, LaunchPrepareRequest, LaunchPrepareResponse,
    LaunchPrepareSuccess, RefBindingSourceWire, RuntimeFactKindWire, TrustClassWire,
    ValidateLaunchPreparerConfigRequest, ValidateLaunchPreparerConfigResponse,
    ValidateLaunchPreparerConfigSuccess,
};
use ryeos_state::external_content::products::ProductDeclarations;
use ryeos_state::external_content::products::admission::{
    AdmittedProductRecipeBinding, PRODUCT_RECIPE_BINDING_SCHEMA,
};
use ryeos_state::external_content::products::composition::ProductRelationships;

pub const PRODUCT_RECIPE_BINDING: &str = "product_recipe";
const MAX_PRODUCT_RECIPE_FACT_BYTES: u32 = ryeos_engine::runtime_registry::MAX_LAUNCH_FACT_BYTES;

pub fn prepare(request: LaunchPrepareRequest) -> HandlerResponse {
    HandlerResponse::LaunchPrepare {
        response: match prepare_inner(request) {
            Ok(result) => LaunchPrepareResponse::Success { result },
            Err(error) => LaunchPrepareResponse::Error { error },
        },
    }
}

pub fn validate(request: ValidateLaunchPreparerConfigRequest) -> HandlerResponse {
    let response = match validate_contract(&request) {
        Ok(()) => ValidateLaunchPreparerConfigResponse::Valid {
            result: ValidateLaunchPreparerConfigSuccess {},
        },
        Err(message) => ValidateLaunchPreparerConfigResponse::Invalid {
            code: "graph_launch_contract_invalid".to_owned(),
            message,
        },
    };
    HandlerResponse::ValidateLaunchPreparerConfig { response }
}

pub fn wrong_request() -> HandlerResponse {
    HandlerResponse::LaunchPrepare {
        response: LaunchPrepareResponse::Error {
            error: wire_error(
                "graph_launch_protocol_mismatch",
                "graph launch preparer accepts only launch preparation requests",
                LaunchPrepareErrorClass::Internal,
            ),
        },
    }
}

fn prepare_inner(
    request: LaunchPrepareRequest,
) -> Result<LaunchPrepareSuccess, LaunchPrepareError> {
    if request.handler_config != serde_json::json!({}) {
        return Err(wire_error(
            "graph_launch_config_invalid",
            "graph launch preparer takes no configuration",
            LaunchPrepareErrorClass::Internal,
        ));
    }
    if !request.config_inputs.is_empty() {
        return Err(wire_error(
            "graph_launch_inputs_invalid",
            "graph launch preparer accepts no ambient config inputs",
            LaunchPrepareErrorClass::Internal,
        ));
    }

    let declared = request
        .primary
        .composed
        .composed
        .get(PRODUCT_RECIPE_BINDING)
        .map(|value| {
            value.as_str().ok_or_else(|| {
                wire_error(
                    "product_recipe_invalid",
                    "graph product_recipe must be a Config ref string",
                    LaunchPrepareErrorClass::Configuration,
                )
            })
        })
        .transpose()?;
    let bound = request.ref_bindings.get(PRODUCT_RECIPE_BINDING);
    let mut runtime_facts = BTreeMap::new();
    match (declared, bound) {
        (None, None) => {}
        (Some(_), None) => {
            return Err(wire_error(
                "product_recipe_binding_missing",
                "graph declares product_recipe but launch admission supplied no derived binding",
                LaunchPrepareErrorClass::Internal,
            ));
        }
        (None, Some(_)) => {
            return Err(wire_error(
                "product_recipe_undeclared",
                "launch admission supplied a product recipe not declared by the graph root",
                LaunchPrepareErrorClass::Caller,
            ));
        }
        (Some(declared), Some(bound)) => {
            if declared != bound.canonical_ref {
                return Err(wire_error(
                    "product_recipe_binding_mismatch",
                    "derived product recipe binding differs from the signed graph declaration",
                    LaunchPrepareErrorClass::Caller,
                ));
            }
            let declarations_value = bound
                .composed
                .composed
                .get("build_products")
                .cloned()
                .ok_or_else(|| {
                    wire_error(
                        "product_declarations_missing",
                        "product recipe Config has no build_products block",
                        LaunchPrepareErrorClass::Configuration,
                    )
                })?;
            let declarations =
                ProductDeclarations::from_value(declarations_value).map_err(|error| {
                    wire_error(
                        "product_declarations_invalid",
                        format!("invalid product declarations: {error:#}"),
                        LaunchPrepareErrorClass::Configuration,
                    )
                })?;
            let relationships = bound
                .composed
                .composed
                .get("product_relationships")
                .cloned()
                .map(ProductRelationships::from_value)
                .transpose()
                .map_err(|error| {
                    wire_error(
                        "product_relationships_invalid",
                        format!("invalid product relationships: {error:#}"),
                        LaunchPrepareErrorClass::Configuration,
                    )
                })?
                .unwrap_or_else(ProductRelationships::empty);
            let root = bound
                .resolution_digest
                .get("root")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    wire_error(
                        "product_recipe_resolution_invalid",
                        "product recipe binding has no exact root resolution",
                        LaunchPrepareErrorClass::Internal,
                    )
                })?;
            if root.get("resolved_ref").and_then(serde_json::Value::as_str)
                != Some(bound.canonical_ref.as_str())
            {
                return Err(wire_error(
                    "product_recipe_resolution_invalid",
                    "product recipe binding canonical ref differs from its exact resolution",
                    LaunchPrepareErrorClass::Internal,
                ));
            }
            let recipe_raw_content_digest = root
                .get("raw_content_digest")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    wire_error(
                        "product_recipe_resolution_invalid",
                        "product recipe binding has no exact source digest",
                        LaunchPrepareErrorClass::Internal,
                    )
                })?
                .to_owned();
            let fact = AdmittedProductRecipeBinding {
                schema: PRODUCT_RECIPE_BINDING_SCHEMA.to_owned(),
                binding_name: PRODUCT_RECIPE_BINDING.to_owned(),
                recipe_ref: bound.canonical_ref.clone(),
                recipe_raw_content_digest,
                declarations_hash: declarations.content_hash().map_err(|error| {
                    wire_error(
                        "product_declarations_invalid",
                        format!("hash product declarations: {error:#}"),
                        LaunchPrepareErrorClass::Internal,
                    )
                })?,
                declarations,
                relationships,
            };
            let fact_value = serde_json::to_value(&fact).map_err(|error| {
                wire_error(
                    "product_recipe_admission_invalid",
                    error.to_string(),
                    LaunchPrepareErrorClass::Internal,
                )
            })?;
            let fact_bytes = serde_json::to_vec(&fact_value)
                .map_err(|error| {
                    wire_error(
                        "product_recipe_admission_invalid",
                        error.to_string(),
                        LaunchPrepareErrorClass::Internal,
                    )
                })?
                .len();
            if fact_bytes > MAX_PRODUCT_RECIPE_FACT_BYTES as usize {
                return Err(wire_error(
                    "product_recipe_admission_too_large",
                    "admitted product recipe exceeds the graph runtime-fact budget",
                    LaunchPrepareErrorClass::Configuration,
                ));
            }
            fact.validate().map_err(|error| {
                wire_error(
                    "product_recipe_admission_invalid",
                    format!("invalid admitted product recipe: {error:#}"),
                    LaunchPrepareErrorClass::Internal,
                )
            })?;
            runtime_facts.insert(PRODUCT_RECIPE_BINDING.to_owned(), fact_value);
        }
    }

    Ok(LaunchPrepareSuccess {
        runtime_data: BTreeMap::new(),
        required_secrets: Vec::new(),
        runtime_facts,
        execution_dependencies: BTreeMap::new(),
        content_dependencies: BTreeMap::new(),
        environment_contributions: BTreeMap::new(),
        financial_authority: FinancialAuthorityResultWire::None,
        external_effect_authority: ExternalEffectAuthorityResultWire::None,
    })
}

fn validate_contract(request: &ValidateLaunchPreparerConfigRequest) -> Result<(), String> {
    if request.handler_config != serde_json::json!({}) {
        return Err("handler config must be empty".to_owned());
    }
    if request.primary_allowed_kinds != ["graph"] {
        return Err("primary kind must be exactly graph".to_owned());
    }
    if request.primary_allowed_spaces != [ItemSpaceWire::Bundle, ItemSpaceWire::Project]
        || request.primary_allowed_trust
            != [
                TrustClassWire::TrustedBundle,
                TrustClassWire::TrustedProject,
            ]
    {
        return Err("graph primary source/trust contract is wider than supported".to_owned());
    }
    let binding = request
        .ref_bindings
        .get(PRODUCT_RECIPE_BINDING)
        .ok_or_else(|| "product_recipe ref binding is required by the contract".to_owned())?;
    if request.ref_bindings.len() != 1
        || binding.required
        || binding.project_result_requirement
            != ryeos_handler_protocol::ProjectResultRequirement::RetainedGeneration
        || binding.source
            != (RefBindingSourceWire::PrimaryField {
                path: vec![PRODUCT_RECIPE_BINDING.to_owned()],
            })
        || binding.allowed_kinds != ["config"]
        || binding.allowed_spaces != [ItemSpaceWire::Bundle, ItemSpaceWire::Project]
        || binding.allowed_trust
            != [
                TrustClassWire::TrustedBundle,
                TrustClassWire::TrustedProject,
            ]
    {
        return Err("product_recipe ref binding does not match the graph contract".to_owned());
    }
    let fact = request
        .runtime_facts
        .get(PRODUCT_RECIPE_BINDING)
        .ok_or_else(|| "product_recipe runtime fact is required by the contract".to_owned())?;
    if request.runtime_facts.len() != 1
        || fact.required
        || fact.kind != RuntimeFactKindWire::Json
        || fact.max_bytes != MAX_PRODUCT_RECIPE_FACT_BYTES
    {
        return Err("product_recipe runtime fact does not match the graph contract".to_owned());
    }
    if !request.config_inputs.is_empty()
        || request.secret_policy.max_requirements != 0
        || !request.secret_policy.allowed_names.is_empty()
        || !request.required_runtime_data.is_empty()
        || request.execution_dependencies.max_dependencies != 0
        || !request.execution_dependencies.allowed_kinds.is_empty()
        || !request.execution_dependencies.allowed_spaces.is_empty()
        || !request.execution_dependencies.allowed_trust.is_empty()
        || request.content_dependencies.max_dependencies != 0
        || !request.content_dependencies.allowed_bindings.is_empty()
        || request.content_dependencies.max_targets_per_dependency != 0
        || request.content_dependencies.max_executable_search_entries != 0
        || request.content_dependencies.external_content.is_some()
        || request.evidence_attachments.max_attachments != 0
        || request.evidence_attachments.max_total_bytes != 0
        || request.evidence_attachments.target.is_some()
        || request.evidence_attachments.destination_prefix.is_some()
        || !request.evidence_attachments.allowed_access.is_empty()
        || request.environment_contributions.max_contributions != 0
        || request
            .environment_contributions
            .max_targets_per_contribution
            != 0
        || request
            .environment_contributions
            .max_variables_per_contribution
            != 0
        || request.financial_authority != FinancialAuthorityDeclWire::None
        || request.external_effect_authority != ExternalEffectAuthorityDeclWire::None
    {
        return Err("graph launch contract grants undeclared preparation authority".to_owned());
    }
    Ok(())
}

fn wire_error(
    code: impl Into<String>,
    message: impl Into<String>,
    classification: LaunchPrepareErrorClass,
) -> LaunchPrepareError {
    LaunchPrepareError {
        code: code.into(),
        message: message.into(),
        classification,
        binding: Some(PRODUCT_RECIPE_BINDING.to_owned()),
        details: BTreeMap::<String, LaunchDiagnosticScalarWire>::new(),
    }
}

#[cfg(test)]
mod tests {
    use ryeos_handler_protocol::{
        LaunchComposedViewWire, LaunchPrepareResponse, LaunchPreparedItemWire, TrustClassWire,
    };
    use serde_json::json;

    use super::*;

    fn item(canonical_ref: &str, composed: serde_json::Value) -> LaunchPreparedItemWire {
        LaunchPreparedItemWire {
            canonical_ref: canonical_ref.to_owned(),
            source_space: ItemSpaceWire::Project,
            effective_trust_class: TrustClassWire::TrustedProject,
            composed: LaunchComposedViewWire {
                composed,
                derived: BTreeMap::new(),
                policy_facts: BTreeMap::new(),
            },
            resolution_digest: json!({
                "root": {
                    "resolved_ref": canonical_ref,
                    "raw_content_digest": "a".repeat(64),
                }
            }),
        }
    }

    fn request(with_recipe: bool) -> LaunchPrepareRequest {
        let recipe_ref = "config:test/two-products";
        let graph = if with_recipe {
            json!({"product_recipe": recipe_ref})
        } else {
            json!({})
        };
        let mut ref_bindings = BTreeMap::new();
        if with_recipe {
            ref_bindings.insert(
                PRODUCT_RECIPE_BINDING.to_owned(),
                item(
                    recipe_ref,
                    json!({
                        "category": "test",
                        "name": "two-products",
                        "version": "1.0.0",
                        "build_products": {
                            "schema": "ryeos.build_products.v1",
                            "output_roots": [],
                            "products": [{
                                "name": "runtime",
                                "source": {"kind": "retained_project"},
                                "path": "products/runtime",
                                "shape": "tree",
                                "storage": "content",
                                "required": true,
                                "bounds": {
                                    "maximum_entries": 8,
                                    "maximum_depth": 4,
                                    "maximum_file_bytes": 1024,
                                    "maximum_total_bytes": 4096
                                },
                                "expected_manifest_hash": null
                            }]
                        },
                        "product_relationships": {
                            "schema": "ryeos.product_relationships.v1",
                            "relationships": [{
                                "name": "runtime_to_consumer",
                                "producer": {
                                    "canonical_ref": "graph:test/producer",
                                    "recipe_binding": "product_recipe",
                                    "product_name": "runtime",
                                    "parameters": {"profile": "release", "target": "test"}
                                },
                                "consumer": {
                                    "canonical_ref": "config:test/consumer",
                                    "declaration_id": "runtime"
                                },
                                "required_product": {
                                    "shape": "tree",
                                    "storage": "content",
                                    "bounds": {
                                        "maximum_entries": 4,
                                        "maximum_depth": 3,
                                        "maximum_file_bytes": 512,
                                        "maximum_total_bytes": 2048
                                    }
                                },
                                "qualification": {
                                    "policy_ref": null,
                                    "required_claims": []
                                }
                            }]
                        }
                    }),
                ),
            );
        }
        LaunchPrepareRequest {
            handler_config: json!({}),
            primary: item("graph:test/producer", graph),
            ref_bindings,
            config_inputs: BTreeMap::new(),
        }
    }

    fn success(request: LaunchPrepareRequest) -> LaunchPrepareSuccess {
        let HandlerResponse::LaunchPrepare {
            response: LaunchPrepareResponse::Success { result },
        } = prepare(request)
        else {
            panic!("graph launch preparation must succeed");
        };
        result
    }

    #[test]
    fn ordinary_graph_prepares_without_recipe_facts() {
        assert!(success(request(false)).runtime_facts.is_empty());
    }

    #[test]
    fn product_graph_retains_exact_config_identity_and_declaration_block() {
        let result = success(request(true));
        let fact: AdmittedProductRecipeBinding =
            serde_json::from_value(result.runtime_facts[PRODUCT_RECIPE_BINDING].clone()).unwrap();
        assert_eq!(fact.recipe_ref, "config:test/two-products");
        assert_eq!(fact.recipe_raw_content_digest, "a".repeat(64));
        assert_eq!(fact.declarations.products[0].name, "runtime");
        assert_eq!(
            fact.relationships.relationships[0].name,
            "runtime_to_consumer"
        );
        assert!(fact.validate().is_ok());
    }

    #[test]
    fn absent_relationship_block_is_retained_as_an_explicit_empty_projection() {
        let mut request = request(true);
        request
            .ref_bindings
            .get_mut(PRODUCT_RECIPE_BINDING)
            .unwrap()
            .composed
            .composed
            .as_object_mut()
            .unwrap()
            .remove("product_relationships");
        let result = success(request);
        let fact: AdmittedProductRecipeBinding =
            serde_json::from_value(result.runtime_facts[PRODUCT_RECIPE_BINDING].clone()).unwrap();
        assert!(fact.relationships.relationships.is_empty());
        assert!(fact.validate().is_ok());
    }

    #[test]
    fn nonempty_qualification_is_refused() {
        let mut request = request(true);
        request
            .ref_bindings
            .get_mut(PRODUCT_RECIPE_BINDING)
            .unwrap()
            .composed
            .composed["product_relationships"]["relationships"][0]["qualification"]["policy_ref"] =
            json!("policy:test/compatibility");
        assert!(matches!(
            prepare(request),
            HandlerResponse::LaunchPrepare {
                response: LaunchPrepareResponse::Error { error }
            } if error.code == "product_relationships_invalid"
        ));
    }

    #[test]
    fn workspace_output_declarations_retain_the_typed_partition_recipe() {
        let mut request = request(true);
        request
            .ref_bindings
            .get_mut(PRODUCT_RECIPE_BINDING)
            .unwrap()
            .composed
            .composed["build_products"]["output_roots"] = json!([{
            "name": "runtime",
            "path": "products/runtime",
            "storage": "content",
            "bounds": {
                "maximum_entries": 8,
                "maximum_depth": 4,
                "maximum_file_bytes": 1024,
                "maximum_total_bytes": 4096
            }
        }]);
        request
            .ref_bindings
            .get_mut(PRODUCT_RECIPE_BINDING)
            .unwrap()
            .composed
            .composed["build_products"]["products"][0]["source"] =
            json!({"kind": "workspace_output", "root": "runtime"});
        let result = success(request);
        let fact: AdmittedProductRecipeBinding =
            serde_json::from_value(result.runtime_facts[PRODUCT_RECIPE_BINDING].clone()).unwrap();
        assert!(fact.declarations.requires_workspace_output_capture());
        assert_eq!(fact.declarations.output_roots[0].path, "products/runtime");
        assert!(fact.validate().is_ok());
    }

    #[test]
    fn graph_and_bound_recipe_must_be_present_and_equal() {
        let mut missing = request(true);
        missing.ref_bindings.clear();
        assert!(matches!(
            prepare(missing),
            HandlerResponse::LaunchPrepare {
                response: LaunchPrepareResponse::Error { error }
            } if error.code == "product_recipe_binding_missing"
        ));

        let mut contradiction = request(true);
        contradiction
            .ref_bindings
            .get_mut(PRODUCT_RECIPE_BINDING)
            .unwrap()
            .canonical_ref = "config:test/other".to_owned();
        assert!(matches!(
            prepare(contradiction),
            HandlerResponse::LaunchPrepare {
                response: LaunchPrepareResponse::Error { error }
            } if error.code == "product_recipe_binding_mismatch"
        ));
    }

    #[test]
    fn config_metadata_is_not_mistaken_for_the_declaration_schema() {
        let mut malformed = request(true);
        malformed
            .ref_bindings
            .get_mut(PRODUCT_RECIPE_BINDING)
            .unwrap()
            .composed
            .composed
            .as_object_mut()
            .unwrap()
            .remove("build_products");
        assert!(matches!(
            prepare(malformed),
            HandlerResponse::LaunchPrepare {
                response: LaunchPrepareResponse::Error { error }
            } if error.code == "product_declarations_missing"
        ));
    }

    #[test]
    fn product_recipe_fact_cannot_exceed_the_registry_launch_fact_ceiling() {
        let mut oversized = request(true);
        let products = (0..32)
            .map(|index| {
                let segment = format!("product-{index}-{}", "x".repeat(90));
                json!({
                    "name": format!("product-{index}"),
                    "source": {"kind": "retained_project"},
                    "path": format!("products/{segment}/{segment}/{segment}/{segment}/{segment}"),
                    "shape": "tree",
                    "storage": "content",
                    "required": true,
                    "bounds": {
                        "maximum_entries": 8,
                        "maximum_depth": 4,
                        "maximum_file_bytes": 1024,
                        "maximum_total_bytes": 4096
                    },
                    "expected_manifest_hash": null
                })
            })
            .collect::<Vec<_>>();
        oversized
            .ref_bindings
            .get_mut(PRODUCT_RECIPE_BINDING)
            .unwrap()
            .composed
            .composed["build_products"]["products"] = json!(products);
        assert!(matches!(
            prepare(oversized),
            HandlerResponse::LaunchPrepare {
                response: LaunchPrepareResponse::Error { error }
            } if error.code == "product_recipe_admission_too_large"
        ));
    }
}
