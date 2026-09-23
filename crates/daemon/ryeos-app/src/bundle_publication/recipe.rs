//! Closed authoring contract for exact per-release build recipes.
//!
//! Callers supply release coordinates, never a Config body, graph reference,
//! filesystem allowance, or signature payload. Signing does not admit execution:
//! the normal resolver and product lifecycle must still admit the returned item.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::admitted_build::AdmittedReleaseInput;

pub type ReleaseChildProductSelections =
    ryeos_state::external_content::products::composition::ProductSelectionInputs;

pub fn validate_child_product_selections(
    selections: &ReleaseChildProductSelections,
) -> anyhow::Result<()> {
    use ryeos_state::external_content::products::composition::{
        ProductSelectionTarget, canonicalize_product_selection_inputs,
    };
    anyhow::ensure!(
        selections
            .iter()
            .all(|input| input.target == ProductSelectionTarget::Root {}),
        "release child selectors must name admitted root inputs"
    );
    anyhow::ensure!(
        canonicalize_product_selection_inputs(selections.clone())? == *selections,
        "release child selectors must be canonical"
    );
    Ok(())
}

pub const BUILD_GRAPH: &str = "graph:ryeos/bundle-release/native-build";
pub const PORTABLE_BUILD_GRAPH: &str = "graph:ryeos/bundle-release/portable-build";
pub const CAPTURE_GRAPH: &str = "graph:ryeos/bundle-release/signed-capture";
pub const PORTABLE_CAPTURE_GRAPH: &str = "graph:ryeos/bundle-release/portable-signed-capture";
pub const BUILD_RECIPE_REF: &str = "config:bundle-release/native-build-products";
pub const PORTABLE_BUILD_RECIPE_REF: &str = "config:bundle-release/portable-build-products";
pub const CAPTURE_RECIPE_REF: &str = "config:bundle-release/signed-capture-products";
pub const PORTABLE_CAPTURE_RECIPE_REF: &str =
    "config:bundle-release/portable-signed-capture-products";
pub const PORTABLE_QUALIFIER: &str = "tool:ryeos/bundle-release/portable-qualify";
pub const SUBSTRATE_BUILD_RECIPE_REF: &str = "config:bundle-release/substrate-build-products";
pub const SUBSTRATE_BUILD_GRAPH: &str = "graph:ryeos/bundle-release/substrate-build";
pub const SUBSTRATE_QUALIFIER: &str = "tool:ryeos/bundle-release/substrate-qualify";

/// The publisher may authorize only this receipt-shaped substrate producer.
/// No Config body, graph reference, output path, or signing payload is caller supplied.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizeSubstrateBuildRecipeRequest {
    pub catalog_namespace: String,
    pub bundle_publication_policy_section_digest: String,
    pub trust_epoch: u64,
    pub receipt: ryeos_bundle_publication_contract::SubstrateBuildReceipt,
    pub child_product_selections: ReleaseChildProductSelections,
}

impl AuthorizeSubstrateBuildRecipeRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.catalog_namespace.is_empty()
                && self.catalog_namespace.len() <= 64
                && self
                    .catalog_namespace
                    .bytes()
                    .all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || matches!(b, b'-' | b'_')),
            "invalid substrate recipe catalog namespace"
        );
        require_hash(&self.bundle_publication_policy_section_digest)?;
        anyhow::ensure!(
            self.trust_epoch > 0,
            "substrate recipe trust epoch must be nonzero"
        );
        self.receipt.validate()?;
        validate_child_product_selections(&self.child_product_selections)?;
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() <= 16 * 1024,
            "substrate recipe request exceeds bound"
        );
        Ok(())
    }

    pub fn require_policy(&self, namespace: &str, digest: &str, epoch: u64) -> anyhow::Result<()> {
        self.validate()?;
        anyhow::ensure!(
            self.catalog_namespace == namespace
                && self.bundle_publication_policy_section_digest == digest
                && self.trust_epoch == epoch,
            "substrate recipe request violates pinned publisher policy"
        );
        Ok(())
    }

    pub fn parameters(&self) -> Value {
        json!({"receipt": self.receipt,
            "child_product_selections": self.child_product_selections})
    }

    pub fn config_body(&self) -> anyhow::Result<String> {
        self.validate()?;
        let bounds = json!({"maximum_entries":2,"maximum_depth":2,
            "maximum_file_bytes":65536,"maximum_total_bytes":65536});
        let body = json!({
            "category":"bundle-release", "version":"1.0.0",
            "description":"Receipt-only captured product contract for one measured RyeOS substrate release.",
            "recipe_purpose":"bundle_release_v1",
            "release_recipe_authorization": {
                "catalog_namespace":self.catalog_namespace,
                "bundle_publication_policy_section_digest":self.bundle_publication_policy_section_digest,
                "trust_epoch":self.trust_epoch
            },
            "build_products": {
                "schema":"ryeos.build_products.v1",
                "output_roots":[{"name":"substrate_release","path":"products/substrate-release",
                    "storage":"content","bounds":bounds}],
                "products":[{"name":"substrate_release","source":{"kind":"workspace_output","root":"substrate_release"},
                    "path":"products/substrate-release","shape":"tree","storage":"content",
                    "required":true,"bounds":bounds}]
            },
            "product_relationships": {
                "schema":"ryeos.product_relationships.v1",
                "relationships":[{"name":"substrate_release_to_qualification",
                    "producer":{"canonical_ref":SUBSTRATE_BUILD_GRAPH,"recipe_binding":"product_recipe",
                        "product_name":"substrate_release","parameters":self.parameters()},
                    "consumer":{"canonical_ref":SUBSTRATE_QUALIFIER,"declaration_id":"subject"},
                    "required_product":{"shape":"tree","storage":"content","bounds":bounds},
                    "qualification":{"policy_ref":"config:bundle-release/substrate-qualification",
                        "required_claims":["substrate_release_checks_v1"]}}]
            }
        });
        let relationships: ryeos_state::external_content::products::composition::ProductRelationships =
            serde_json::from_value(body["product_relationships"].clone())?;
        relationships.validate()?;
        let declarations =
            ryeos_state::external_content::products::ProductDeclarations::from_value(
                body["build_products"].clone(),
            )?;
        relationships.validate_against(&declarations, "product_recipe")?;
        Ok(format!("{}\n", lillux::canonical_json(&body)?))
    }

    /// Bind dynamic authorization to the independently signed, pinned source
    /// template. The only permitted changes are the policy fence and exact
    /// producer parameters; all product/consumer structure stays identical.
    pub fn validate_source_template(&self, signed_source: &str) -> anyhow::Result<()> {
        let mut expected: Value = serde_json::from_str(&self.config_body()?)?;
        expected
            .as_object_mut()
            .context("substrate recipe is not an object")?
            .remove("release_recipe_authorization");
        expected["product_relationships"]["relationships"][0]["producer"]["parameters"] = json!({});
        let actual: Value = serde_yaml::from_str(signed_source)?;
        anyhow::ensure!(
            actual == expected,
            "signed substrate source template differs from constrained publisher recipe shape"
        );
        Ok(())
    }

    pub fn validate_response(&self, value: &Value, publisher: &str) -> anyhow::Result<()> {
        let response: RecipeResponse = serde_json::from_value(value.clone())?;
        let body = self.config_body()?;
        let (line, returned_body) = response
            .signed_config
            .split_once('\n')
            .context("substrate recipe signature envelope is absent")?;
        let header = lillux::signature::parse_signature_line(line, "#", None)
            .context("invalid substrate recipe signature envelope")?;
        anyhow::ensure!(
            response.schema == "ryeos.substrate_build_recipe_authorization.v1"
                && response.canonical_ref == SUBSTRATE_BUILD_RECIPE_REF
                && response.publisher_fingerprint == publisher
                && returned_body == body
                && response.body_hash == lillux::signature::content_hash(&body)
                && response.signed_blob_hash
                    == lillux::sha256_hex(response.signed_config.as_bytes())
                && header.content_hash == response.body_hash
                && header.signer_fingerprint == publisher,
            "substrate recipe response differs from exact authorized template"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizeBuildRecipeRequest {
    pub catalog_namespace: String,
    pub bundle_publication_policy_section_digest: String,
    pub trust_epoch: u64,
    pub release_input: Value,
    pub child_product_selections: ReleaseChildProductSelections,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizeCaptureRecipeRequest {
    pub catalog_namespace: String,
    pub bundle_publication_policy_section_digest: String,
    pub trust_epoch: u64,
    pub release_input: Value,
    pub materialization_result_hash: String,
    pub signed_tree_manifest_hash: String,
    pub manifest_item_hash: String,
    pub signed_manifest: String,
    pub child_product_selections: ReleaseChildProductSelections,
}

impl AuthorizeCaptureRecipeRequest {
    pub fn capture_graph(&self) -> anyhow::Result<&'static str> {
        let input = AdmittedReleaseInput::from_value(&self.release_input)?;
        Ok(if input.requires_binary_build {
            CAPTURE_GRAPH
        } else {
            PORTABLE_CAPTURE_GRAPH
        })
    }

    pub fn canonical_ref(&self) -> anyhow::Result<&'static str> {
        let input = AdmittedReleaseInput::from_value(&self.release_input)?;
        Ok(if input.requires_binary_build {
            CAPTURE_RECIPE_REF
        } else {
            PORTABLE_CAPTURE_RECIPE_REF
        })
    }

    pub fn qualifier(&self) -> anyhow::Result<&'static str> {
        let input = AdmittedReleaseInput::from_value(&self.release_input)?;
        Ok(if input.requires_binary_build {
            "tool:ryeos/bundle-release/native-qualify"
        } else {
            PORTABLE_QUALIFIER
        })
    }

    pub fn require_policy(
        &self,
        namespace: &str,
        section_digest: &str,
        trust_epoch: u64,
    ) -> anyhow::Result<()> {
        self.validate()?;
        anyhow::ensure!(
            self.catalog_namespace == namespace
                && self.bundle_publication_policy_section_digest == section_digest
                && self.trust_epoch == trust_epoch,
            "capture recipe request does not match pinned publisher policy"
        );
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() <= 256 * 1024,
            "capture recipe request is too large"
        );
        AuthorizeBuildRecipeRequest {
            catalog_namespace: self.catalog_namespace.clone(),
            bundle_publication_policy_section_digest: self
                .bundle_publication_policy_section_digest
                .clone(),
            trust_epoch: self.trust_epoch,
            release_input: self.release_input.clone(),
            child_product_selections: self.child_product_selections.clone(),
        }
        .validate()?;
        for value in [
            &self.materialization_result_hash,
            &self.signed_tree_manifest_hash,
            &self.manifest_item_hash,
        ] {
            require_hash(value)?;
        }
        anyhow::ensure!(
            self.signed_manifest.len() <= 128 * 1024
                && lillux::sha256_hex(self.signed_manifest.as_bytes()) == self.manifest_item_hash,
            "signed manifest bytes disagree with their admitted identity"
        );
        Ok(())
    }

    pub fn graph_parameters(&self) -> Value {
        json!({
            "release_input": self.release_input,
            "child_product_selections": self.child_product_selections,
            "materialization_result_hash": self.materialization_result_hash,
            "signed_tree_manifest_hash": self.signed_tree_manifest_hash,
            "manifest_item_hash": self.manifest_item_hash,
            "signed_manifest": self.signed_manifest,
        })
    }

    pub fn config_body(&self) -> anyhow::Result<String> {
        self.validate()?;
        let capture_graph = self.capture_graph()?;
        let qualifier = self.qualifier()?;
        let (
            output_root_path,
            product_name,
            product_path,
            relationship_name,
            qualification_policy,
            qualification_claim,
        ) = if AdmittedReleaseInput::from_value(&self.release_input)?.requires_binary_build {
            (
                "products/signed-native-bundle",
                "signed_native_bundle",
                "products/signed-native-bundle/tree",
                "signed_native_bundle_to_release_qualification",
                "config:bundle-release/native-qualification",
                "native_bundle_release_checks_v1",
            )
        } else {
            (
                "products/signed-native-bundle",
                "signed_portable_bundle",
                "products/signed-native-bundle/tree",
                "signed_portable_bundle_to_release_qualification",
                "config:bundle-release/portable-qualification",
                "portable_bundle_release_checks_v1",
            )
        };
        let bounds = json!({"maximum_entries":4096,"maximum_depth":32,
            "maximum_file_bytes":ryeos_state::external_content::MAX_CAPTURE_FILE_BYTES,
            "maximum_total_bytes":ryeos_state::external_content::MAX_CAPTURE_BYTES});
        let body = json!({
            "category":"bundle-release", "version":"1.0.0",
            "description":"Exact admitted signed-tree capture output recipe.",
            "recipe_purpose":"bundle_release_v1",
            "release_recipe_authorization": {
                "catalog_namespace":self.catalog_namespace,
                "bundle_publication_policy_section_digest":self.bundle_publication_policy_section_digest,
                "trust_epoch":self.trust_epoch
            },
            "build_products": {
                "schema":"ryeos.build_products.v1",
                "output_roots":[{"name":"signed_bundle_tree","path":output_root_path,"storage":"content","bounds":bounds}],
                "products":[{"name":product_name,"source":{"kind":"workspace_output","root":"signed_bundle_tree"},
                    "path":product_path,"shape":"tree","storage":"content","required":true,"bounds":bounds}]
            },
            "product_relationships": {
                "schema":"ryeos.product_relationships.v1",
                "relationships":[{
                    "name":relationship_name,
                    "producer":{"canonical_ref":capture_graph,"recipe_binding":"product_recipe","product_name":product_name, "parameters":self.graph_parameters()},
                    "consumer":{"canonical_ref":qualifier,"declaration_id":"subject"},
                    "required_product":{"shape":"tree","storage":"content","bounds":bounds},
                    "qualification":{"policy_ref":qualification_policy,"required_claims":[qualification_claim]}
                }]
            }
        });
        let relationships: ryeos_state::external_content::products::composition::ProductRelationships =
            serde_json::from_value(body["product_relationships"].clone())?;
        relationships.validate()?;
        let declarations =
            ryeos_state::external_content::products::ProductDeclarations::from_value(
                body["build_products"].clone(),
            )?;
        relationships.validate_against(&declarations, "product_recipe")?;
        Ok(format!("{}\n", lillux::canonical_json(&body)?))
    }
}

impl AuthorizeBuildRecipeRequest {
    pub fn graph_parameters(&self) -> Value {
        json!({"release_input":self.release_input,
            "child_product_selections":self.child_product_selections})
    }

    pub fn build_graph(&self) -> anyhow::Result<&'static str> {
        let input = AdmittedReleaseInput::from_value(&self.release_input)?;
        validate_child_product_selections(&self.child_product_selections)?;
        Ok(if input.requires_binary_build {
            BUILD_GRAPH
        } else {
            PORTABLE_BUILD_GRAPH
        })
    }

    pub fn canonical_ref(&self) -> anyhow::Result<&'static str> {
        let input = AdmittedReleaseInput::from_value(&self.release_input)?;
        Ok(if input.requires_binary_build {
            BUILD_RECIPE_REF
        } else {
            PORTABLE_BUILD_RECIPE_REF
        })
    }

    pub fn require_policy(
        &self,
        namespace: &str,
        section_digest: &str,
        trust_epoch: u64,
    ) -> anyhow::Result<()> {
        self.validate()?;
        anyhow::ensure!(
            self.catalog_namespace == namespace
                && self.bundle_publication_policy_section_digest == section_digest
                && self.trust_epoch == trust_epoch,
            "recipe request does not match pinned publisher policy"
        );
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        validate_child_product_selections(&self.child_product_selections)?;
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() <= 128 * 1024,
            "recipe request is too large"
        );
        anyhow::ensure!(
            !self.catalog_namespace.is_empty()
                && self.catalog_namespace.len() <= 64
                && self
                    .catalog_namespace
                    .bytes()
                    .all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || matches!(b, b'-' | b'_')),
            "invalid recipe catalog namespace"
        );
        require_hash(&self.bundle_publication_policy_section_digest)?;
        anyhow::ensure!(self.trust_epoch > 0, "recipe trust epoch must be nonzero");
        let input = AdmittedReleaseInput::from_value(&self.release_input)?;
        require_hash(&input.source_snapshot_hash)?;
        if let Some(hash) = &input.predecessor_generation_hash {
            require_hash(hash)?;
        }
        let path = std::path::Path::new(&input.project_path);
        anyhow::ensure!(
            path.is_absolute()
                && !path
                    .components()
                    .any(|p| matches!(p, std::path::Component::ParentDir)),
            "release project path must be absolute without traversal"
        );
        Ok(())
    }

    /// The only Config body this operation may authorize. JSON is also valid
    /// YAML; use canonical bytes so the exact input yields one body identity.
    pub fn config_body(&self) -> anyhow::Result<String> {
        self.validate()?;
        let input = AdmittedReleaseInput::from_value(&self.release_input)?;
        let build_graph = self.build_graph()?;
        let capture_graph = if input.requires_binary_build {
            CAPTURE_GRAPH
        } else {
            PORTABLE_CAPTURE_GRAPH
        };
        let recipe_ref = self.canonical_ref()?;
        let (output_root_path, product_name, product_path, relationship_name) =
            if input.requires_binary_build {
                (
                    "products/native-bundle",
                    "native_bundle",
                    "products/native-bundle/tree",
                    "native_bundle_to_signed_capture",
                )
            } else {
                (
                    "products/native-bundle",
                    "portable_bundle",
                    "products/native-bundle/tree",
                    "portable_bundle_to_signed_capture",
                )
            };
        let bounds = json!({"maximum_entries":4096,"maximum_depth":32,
            "maximum_file_bytes":ryeos_state::external_content::MAX_CAPTURE_FILE_BYTES,
            "maximum_total_bytes":ryeos_state::external_content::MAX_CAPTURE_BYTES});
        let body = json!({
            "category":"bundle-release", "version":"1.0.0",
            "description":"Exact admitted release build and signed-capture input recipe.",
            "recipe_purpose":"bundle_release_v1",
            "release_recipe_authorization": {
                "catalog_namespace":self.catalog_namespace,
                "bundle_publication_policy_section_digest":self.bundle_publication_policy_section_digest,
                "trust_epoch":self.trust_epoch
            },
            "build_products": {
                "schema":"ryeos.build_products.v1",
                "output_roots":[{"name":"bundle_tree","path":output_root_path,"storage":"content","bounds":bounds}],
                "products":[{"name":product_name,"source":{"kind":"workspace_output","root":"bundle_tree"},
                    "path":product_path,"shape":"tree","storage":"content","required":true,"bounds":bounds}]
            },
            "product_relationships": {
                "schema":"ryeos.product_relationships.v1",
                "relationships":[{
                    "name":relationship_name,
                    "producer":{"canonical_ref":build_graph,"recipe_binding":"product_recipe","product_name":product_name, "parameters":self.graph_parameters()},
                    "consumer":{"canonical_ref":capture_graph,"declaration_id":"unsigned_bundle"},
                    "required_product":{"shape":"tree","storage":"content","bounds":bounds},
                    "qualification":{"policy_ref":null,"required_claims":[]}
                }, {
                    "name":format!("{relationship_name}_tool"),
                    "producer":{"canonical_ref":build_graph,"recipe_binding":"product_recipe","product_name":product_name, "parameters":self.graph_parameters()},
                    "consumer":{"canonical_ref":if input.requires_binary_build {
                        "tool:ryeos/bundle-release/signed-capture"
                    } else {
                        "tool:ryeos/bundle-release/portable-signed-capture"
                    },"declaration_id":"unsigned_bundle"},
                    "required_product":{"shape":"tree","storage":"content","bounds":bounds},
                    "qualification":{"policy_ref":null,"required_claims":[]}
                }]
            }
        });
        // Validate using the same closed wire types that admission consumes.
        let relationships: ryeos_state::external_content::products::composition::ProductRelationships =
            serde_json::from_value(body["product_relationships"].clone()).context("validate authored recipe relationships")?;
        relationships.validate()?;
        let declarations =
            ryeos_state::external_content::products::ProductDeclarations::from_value(
                body["build_products"].clone(),
            )?;
        relationships.validate_against(&declarations, "product_recipe")?;
        let encoded = format!("{}\n", lillux::canonical_json(&body)?);
        // Validate the complete runtime-fact budget, not just relationships.
        // This is a dry validation projection, never emitted as admitted evidence.
        // The real loader will bind the signed item's exact raw bytes.
        ryeos_state::external_content::products::admission::AdmittedProductRecipeBinding {
            schema:
                ryeos_state::external_content::products::admission::PRODUCT_RECIPE_BINDING_SCHEMA
                    .into(),
            binding_name: "product_recipe".into(),
            recipe_ref: recipe_ref.into(),
            recipe_raw_content_digest: lillux::sha256_hex(encoded.as_bytes()),
            purpose: ryeos_state::external_content::products::ProductRecipePurpose::BundleReleaseV1,
            declarations_hash: declarations.content_hash()?,
            declarations,
            relationships,
        }
        .validate()?;
        Ok(encoded)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecipeResponse {
    schema: String,
    canonical_ref: String,
    publisher_fingerprint: String,
    body_hash: String,
    signed_blob_hash: String,
    signed_config: String,
}

/// Check the response's exact bytes, template and declared signer. This is a
/// transport consistency check, not signature/trust admission; normal Config
/// admission must independently verify the envelope with its pinned trust key.
pub fn validate_recipe_response(
    request: &AuthorizeBuildRecipeRequest,
    value: &Value,
    expected_publisher: &str,
) -> anyhow::Result<()> {
    let response: RecipeResponse = serde_json::from_value(value.clone())?;
    let body = request.config_body()?;
    anyhow::ensure!(
        response.schema == "ryeos.bundle_build_recipe_authorization.v1"
            && response.canonical_ref == request.canonical_ref()?
            && response.publisher_fingerprint == expected_publisher,
        "recipe response identity mismatch"
    );
    let (line, returned_body) = response
        .signed_config
        .split_once('\n')
        .context("recipe signature envelope is absent")?;
    let header = lillux::signature::parse_signature_line(line, "#", None)
        .context("invalid recipe signature envelope")?;
    anyhow::ensure!(
        returned_body == body
            && response.body_hash == lillux::signature::content_hash(&body)
            && response.signed_blob_hash == lillux::sha256_hex(response.signed_config.as_bytes())
            && header.content_hash == response.body_hash
            && header.signer_fingerprint == expected_publisher,
        "recipe response bytes differ from exact authorized template"
    );
    Ok(())
}

pub fn validate_capture_recipe_response(
    request: &AuthorizeCaptureRecipeRequest,
    value: &Value,
    expected_publisher: &str,
) -> anyhow::Result<()> {
    let response: RecipeResponse = serde_json::from_value(value.clone())?;
    let body = request.config_body()?;
    anyhow::ensure!(
        response.schema == "ryeos.bundle_capture_recipe_authorization.v1"
            && response.canonical_ref == request.canonical_ref()?
            && response.publisher_fingerprint == expected_publisher,
        "capture recipe response identity mismatch"
    );
    let (line, returned_body) = response
        .signed_config
        .split_once('\n')
        .context("capture recipe signature envelope is absent")?;
    let header = lillux::signature::parse_signature_line(line, "#", None)
        .context("invalid capture recipe signature envelope")?;
    anyhow::ensure!(
        returned_body == body
            && response.body_hash == lillux::signature::content_hash(&body)
            && response.signed_blob_hash == lillux::sha256_hex(response.signed_config.as_bytes())
            && header.content_hash == response.body_hash
            && header.signer_fingerprint == expected_publisher,
        "capture recipe response bytes differ from exact authorized template"
    );
    Ok(())
}

fn require_hash(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "recipe coordinate must be lowercase SHA-256"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AuthorizeBuildRecipeRequest {
        AuthorizeBuildRecipeRequest {
            catalog_namespace: "official".into(),
            bundle_publication_policy_section_digest: "a".repeat(64),
            trust_epoch: 1,
            release_input: json!({
                "schema":"ryeos.bundle_release_input_plan.v1",
                "project_path":"/release/source", "bundle_name":"central-auth",
                "authored_manifest":{"name":"central-auth","version":"0.1.0","provides_kinds":[]},
                "source_snapshot_hash":"b".repeat(64), "predecessor_generation_hash":null,
                "target":{"kind":"portable"}, "build_profile":"release",
                "payload_ownership_item_ref":"config:bundle-release/payload-ownership",
                "payload_ownership_content_hash":"c".repeat(64),
                "payloads":[], "cargo_packages":[], "build_classes":[],
                "requires_binary_build":false,"clean_output_required":true,"ambient_target_reuse_allowed":false
            }),
            child_product_selections: vec![],
        }
    }

    fn capture_request() -> AuthorizeCaptureRecipeRequest {
        let signed_manifest = "# ryeos:signed:test\n{\"name\":\"central-auth\"}\n".to_owned();
        AuthorizeCaptureRecipeRequest {
            catalog_namespace: "official".into(),
            bundle_publication_policy_section_digest: "a".repeat(64),
            trust_epoch: 1,
            release_input: request().release_input,
            materialization_result_hash: "d".repeat(64),
            signed_tree_manifest_hash: "e".repeat(64),
            manifest_item_hash: lillux::sha256_hex(signed_manifest.as_bytes()),
            signed_manifest,
            child_product_selections: vec![],
        }
    }

    fn substrate_request() -> AuthorizeSubstrateBuildRecipeRequest {
        AuthorizeSubstrateBuildRecipeRequest {
            catalog_namespace: "official".into(),
            bundle_publication_policy_section_digest: "a".repeat(64),
            trust_epoch: 1,
            receipt: ryeos_bundle_publication_contract::SubstrateBuildReceipt {
                schema: ryeos_bundle_publication_contract::SUBSTRATE_BUILD_RECEIPT_SCHEMA.into(),
                kind: ryeos_bundle_publication_contract::SUBSTRATE_BUILD_RECEIPT_KIND.into(),
                substrate_image_digest: format!("sha256:{}", "b".repeat(64)),
                substrate_protocol: 1,
                target: ryeos_bundle_publication_contract::BundleTarget::Triple {
                    triple: "x86_64-unknown-linux-gnu".into(),
                },
                core_generation_hash: "c".repeat(64),
            },
            child_product_selections: vec![],
        }
    }

    #[test]
    fn substrate_recipe_is_receipt_exact_and_preserves_signed_source_shape() {
        let request = substrate_request();
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/bundle-release/.ai/config/bundle-release/substrate-build-products.yaml"
        ));
        request.validate_source_template(source).unwrap();
        let body: Value = serde_json::from_str(&request.config_body().unwrap()).unwrap();
        assert_eq!(
            body["product_relationships"]["relationships"][0]["producer"]["parameters"],
            request.parameters()
        );
        let mut changed = request.clone();
        changed.receipt.core_generation_hash = "d".repeat(64);
        assert_ne!(
            request.config_body().unwrap(),
            changed.config_body().unwrap()
        );
        let mut extra: Value = serde_yaml::from_str(source).unwrap();
        extra["unreviewed"] = json!(true);
        assert!(
            request
                .validate_source_template(&serde_yaml::to_string(&extra).unwrap())
                .is_err()
        );
    }

    #[test]
    fn substrate_recipe_response_rejects_other_receipts_and_bodies() {
        let request = substrate_request();
        let key = lillux::crypto::SigningKey::from_bytes(&[53; 32]);
        let publisher = lillux::crypto::fingerprint(&key.verifying_key());
        let body = request.config_body().unwrap();
        let signed = lillux::signature::sign_content(&body, &key, "#", None);
        let response = json!({
            "schema":"ryeos.substrate_build_recipe_authorization.v1",
            "canonical_ref":SUBSTRATE_BUILD_RECIPE_REF,
            "publisher_fingerprint":publisher,
            "body_hash":lillux::signature::content_hash(&body),
            "signed_blob_hash":lillux::sha256_hex(signed.as_bytes()),
            "signed_config":signed,
        });
        request.validate_response(&response, &publisher).unwrap();
        let mut changed_receipt = request.clone();
        changed_receipt.receipt.core_generation_hash = "d".repeat(64);
        assert!(
            changed_receipt
                .validate_response(&response, &publisher)
                .is_err()
        );
        let mut changed_body = response.clone();
        changed_body["signed_config"] =
            json!(format!("{}\n{{}}\n", signed.lines().next().unwrap()));
        assert!(
            request
                .validate_response(&changed_body, &publisher)
                .is_err()
        );
        assert!(
            request
                .validate_response(&response, &"e".repeat(64))
                .is_err()
        );
    }

    #[test]
    fn exact_parameters_and_policy_are_signed_with_fixed_allowances() {
        let request = request();
        let body = request.config_body().unwrap();
        let value: Value = serde_json::from_str(&body).unwrap();
        let relationship = &value["product_relationships"]["relationships"][0];
        assert_eq!(
            relationship["producer"]["parameters"],
            request.graph_parameters()
        );
        assert_eq!(
            relationship["producer"]["canonical_ref"],
            PORTABLE_BUILD_GRAPH
        );
        assert_eq!(
            relationship["consumer"]["canonical_ref"],
            PORTABLE_CAPTURE_GRAPH
        );
        assert_eq!(relationship["name"], "portable_bundle_to_signed_capture");
        assert_eq!(relationship["producer"]["product_name"], "portable_bundle");
        let tool_relationship = &value["product_relationships"]["relationships"][1];
        assert_eq!(
            tool_relationship["producer"]["parameters"],
            request.graph_parameters()
        );
        assert_eq!(
            tool_relationship["consumer"]["canonical_ref"],
            "tool:ryeos/bundle-release/portable-signed-capture"
        );
        assert_eq!(
            relationship["qualification"],
            json!({"policy_ref":null,"required_claims":[]})
        );
        assert_eq!(value["release_recipe_authorization"]["trust_epoch"], 1);
        assert_eq!(body, request.config_body().unwrap());
        let mut different = request.clone();
        different.release_input["source_snapshot_hash"] = json!("d".repeat(64));
        assert_ne!(body, different.config_body().unwrap());
    }

    #[test]
    fn capture_recipe_binds_exact_transformation_and_qualification_lane() {
        let request = capture_request();
        let body: Value = serde_json::from_str(&request.config_body().unwrap()).unwrap();
        let relationship = &body["product_relationships"]["relationships"][0];
        assert_eq!(
            relationship["producer"]["canonical_ref"],
            PORTABLE_CAPTURE_GRAPH
        );
        assert_eq!(
            relationship["producer"]["parameters"],
            request.graph_parameters()
        );
        assert_eq!(
            relationship["consumer"]["canonical_ref"],
            PORTABLE_QUALIFIER
        );
        assert_eq!(
            relationship["qualification"]["policy_ref"],
            "config:bundle-release/portable-qualification"
        );
        assert_eq!(
            relationship["qualification"]["required_claims"],
            json!(["portable_bundle_release_checks_v1"])
        );
        assert_eq!(
            body["build_products"]["products"][0]["name"],
            "signed_portable_bundle"
        );
        assert_eq!(
            relationship["name"],
            "signed_portable_bundle_to_release_qualification"
        );
        let mut changed = request;
        changed.signed_manifest.push('x');
        assert!(changed.validate().is_err());
    }

    #[test]
    fn rejects_wrong_scope_core_and_arbitrary_signing_payload() {
        let request = request();
        request
            .require_policy("official", &"a".repeat(64), 1)
            .unwrap();
        assert!(request.require_policy("other", &"a".repeat(64), 1).is_err());
        assert!(
            request
                .require_policy("official", &"b".repeat(64), 1)
                .is_err()
        );
        assert!(
            request
                .require_policy("official", &"a".repeat(64), 2)
                .is_err()
        );
        let mut unknown = serde_json::to_value(&request).unwrap();
        unknown["config_body"] = json!({"arbitrary":"authority"});
        assert!(serde_json::from_value::<AuthorizeBuildRecipeRequest>(unknown).is_err());
        let mut core = request.clone();
        core.release_input["bundle_name"] = json!("core");
        core.release_input["authored_manifest"]["name"] = json!("core");
        assert!(core.config_body().is_err());
        let mut traversal = request.clone();
        traversal.release_input["project_path"] = json!("/release/../source");
        assert!(traversal.validate().is_err());
        let mut malformed = request;
        malformed.release_input["source_snapshot_hash"] = json!("not-a-hash");
        assert!(malformed.validate().is_err());
    }

    #[test]
    fn rejects_recipes_that_exceed_the_complete_admission_budget() {
        let mut request = request();
        // The request itself fits its HTTP bound, but may not produce an
        // oversized signed allowance that ordinary admission cannot consume.
        request.release_input["authored_manifest"]["description"] = json!("x".repeat(16 * 1024));
        request.validate().unwrap();
        assert!(request.config_body().is_err());
    }

    #[test]
    fn response_requires_exact_template_and_envelope_coordinates() {
        let request = request();
        let temp = tempfile::tempdir().unwrap();
        let identity =
            crate::identity::NodeIdentity::create(&temp.path().join("publisher.pem")).unwrap();
        let body = request.config_body().unwrap();
        let signed = lillux::signature::sign_content(&body, identity.signing_key(), "#", None);
        let response = json!({"schema":"ryeos.bundle_build_recipe_authorization.v1",
            "canonical_ref":PORTABLE_BUILD_RECIPE_REF,
            "publisher_fingerprint":identity.fingerprint(),
            "body_hash":lillux::signature::content_hash(&body),
            "signed_blob_hash":lillux::sha256_hex(signed.as_bytes()), "signed_config":signed});
        validate_recipe_response(&request, &response, identity.fingerprint()).unwrap();
        let header =
            lillux::signature::parse_signature_line(signed.lines().next().unwrap(), "#", None)
                .unwrap();
        assert!(lillux::signature::is_valid_signature_for(
            &header.content_hash,
            &header.signature_b64,
            &header.signer_fingerprint,
            &body,
            identity.verifying_key(),
            identity.fingerprint()
        ));
        assert!(validate_recipe_response(&request, &response, &"f".repeat(64)).is_err());
        let mut changed = request.clone();
        changed.trust_epoch = 2;
        assert!(validate_recipe_response(&changed, &response, identity.fingerprint()).is_err());
        let mut changed = response;
        changed["signed_blob_hash"] = json!("f".repeat(64));
        assert!(validate_recipe_response(&request, &changed, identity.fingerprint()).is_err());
    }
}
