//! Purpose-owned Core seed production, before an OCI substrate image exists.
//! No image digest or substrate receipt is an input: those are measured only
//! after this exact signed/qualified Core generation is packaged.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const BUILD_GRAPH: &str = "graph:ryeos/bundle-release/core-seed-build";
pub const CAPTURE_GRAPH: &str = "graph:ryeos/bundle-release/core-seed-capture";
pub const BUILD_RECIPE: &str = "config:bundle-release/core-seed-build-products";
pub const CAPTURE_RECIPE: &str = "config:bundle-release/core-seed-capture-products";
pub const QUALIFIER: &str = "tool:ryeos/bundle-release/core-seed-qualify";
pub const QUALIFICATION_POLICY: &str = "config:bundle-release/core-seed-qualification";
pub const QUALIFICATION_CLAIM: &str = "substrate_core_seed_checks_v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreSeedBuildRequest {
    pub catalog_namespace: String,
    pub bundle_publication_policy_section_digest: String,
    pub trust_epoch: u64,
    pub release_input: Value,
    #[serde(default)]
    pub child_product_selections: super::recipe::ReleaseChildProductSelections,
}

impl CoreSeedBuildRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        super::recipe::validate_child_product_selections(&self.child_product_selections)?;
        anyhow::ensure!(
            !self.catalog_namespace.is_empty()
                && self.catalog_namespace.len() <= 64
                && self
                    .catalog_namespace
                    .bytes()
                    .all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || matches!(b, b'-' | b'_')),
            "invalid Core seed catalog namespace"
        );
        require_hash(&self.bundle_publication_policy_section_digest)?;
        anyhow::ensure!(
            self.trust_epoch > 0,
            "Core seed trust epoch must be nonzero"
        );
        let input =
            super::admitted_build::AdmittedReleaseInput::from_core_seed_value(&self.release_input)?;
        require_hash(&input.source_snapshot_hash)?;
        let path = std::path::Path::new(&input.project_path);
        anyhow::ensure!(
            path.is_absolute()
                && !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "Core seed source must be an absolute path without traversal"
        );
        anyhow::ensure!(
            serde_json::to_vec(self)?.len() <= 128 * 1024,
            "Core seed input exceeds bound"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreSeedCaptureRecipeRequest {
    pub build: CoreSeedBuildRequest,
    pub materialization_result_hash: String,
    pub signed_tree_manifest_hash: String,
    pub manifest_item_hash: String,
    pub signed_manifest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoreSeedRecipeRequest {
    Build(CoreSeedBuildRequest),
    Capture(CoreSeedCaptureRecipeRequest),
}

impl CoreSeedRecipeRequest {
    pub fn build(&self) -> &CoreSeedBuildRequest {
        match self {
            Self::Build(build) => build,
            Self::Capture(capture) => &capture.build,
        }
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        self.build().validate()?;
        if let Self::Capture(capture) = self {
            for value in [
                &capture.materialization_result_hash,
                &capture.signed_tree_manifest_hash,
                &capture.manifest_item_hash,
            ] {
                require_hash(value)?;
            }
            anyhow::ensure!(
                capture.signed_manifest.len() <= 128 * 1024
                    && lillux::sha256_hex(capture.signed_manifest.as_bytes())
                        == capture.manifest_item_hash,
                "Core seed signed manifest identity changed"
            );
        }
        Ok(())
    }
    pub fn canonical_ref(&self) -> &'static str {
        match self {
            Self::Build(_) => BUILD_RECIPE,
            Self::Capture(_) => CAPTURE_RECIPE,
        }
    }
    pub fn parameters(&self) -> Value {
        match self {
            Self::Build(build) => json!({"release_input":build.release_input,
                "child_product_selections":build.child_product_selections}),
            Self::Capture(capture) => json!({"release_input":capture.build.release_input,
                "child_product_selections":capture.build.child_product_selections,
                "materialization_result_hash":capture.materialization_result_hash,
                "signed_tree_manifest_hash":capture.signed_tree_manifest_hash,
                "manifest_item_hash":capture.manifest_item_hash,"signed_manifest":capture.signed_manifest}),
        }
    }
    pub fn require_policy(&self, namespace: &str, digest: &str, epoch: u64) -> anyhow::Result<()> {
        self.validate()?;
        let build = self.build();
        anyhow::ensure!(
            build.catalog_namespace == namespace
                && build.bundle_publication_policy_section_digest == digest
                && build.trust_epoch == epoch,
            "Core seed recipe violates pinned publisher policy"
        );
        Ok(())
    }
    pub fn config_body(&self) -> anyhow::Result<String> {
        self.validate()?;
        let (producer, product, path, relationship, consumer, slot, qualification) = match self {
            Self::Build(_) => (
                BUILD_GRAPH,
                "core_seed",
                "products/core-seed",
                "core_seed_to_signed_capture",
                CAPTURE_GRAPH,
                "unsigned_core",
                json!({"policy_ref":null,"required_claims":[]}),
            ),
            Self::Capture(_) => (
                CAPTURE_GRAPH,
                "signed_core_seed",
                "products/signed-core-seed",
                "signed_core_seed_to_qualification",
                QUALIFIER,
                "subject",
                json!({"policy_ref":QUALIFICATION_POLICY,"required_claims":[QUALIFICATION_CLAIM]}),
            ),
        };
        let bounds = json!({"maximum_entries":4096,"maximum_depth":32,
            "maximum_file_bytes":ryeos_state::external_content::MAX_CAPTURE_FILE_BYTES,
            "maximum_total_bytes":ryeos_state::external_content::MAX_CAPTURE_BYTES});
        let build = self.build();
        let mut relationships = vec![json!({
            "name":relationship,"producer":{"canonical_ref":producer,"recipe_binding":"product_recipe","product_name":product,"parameters":self.parameters()},
            "consumer":{"canonical_ref":consumer,"declaration_id":slot},
            "required_product":{"shape":"tree","storage":"content","bounds":bounds},"qualification":qualification
        })];
        if matches!(self, Self::Build(_)) {
            relationships.push(json!({
                "name":"core_seed_to_signed_capture_tool",
                "producer":{"canonical_ref":producer,"recipe_binding":"product_recipe","product_name":product,"parameters":self.parameters()},
                "consumer":{"canonical_ref":"tool:ryeos/bundle-release/core-seed-capture","declaration_id":"unsigned_core"},
                "required_product":{"shape":"tree","storage":"content","bounds":bounds},
                "qualification":{"policy_ref":null,"required_claims":[]}
            }));
        }
        let value = json!({"category":"bundle-release","version":"1.0.0","description":"Exact substrate Core seed producer recipe.",
            "recipe_purpose":"bundle_release_v1",
            "release_recipe_authorization":{"catalog_namespace":build.catalog_namespace,
                "bundle_publication_policy_section_digest":build.bundle_publication_policy_section_digest,"trust_epoch":build.trust_epoch},
            "build_products":{"schema":"ryeos.build_products.v1",
                "output_roots":[{"name":"core_tree","path":path,"storage":"content","bounds":bounds}],
                "products":[{"name":product,"source":{"kind":"workspace_output","root":"core_tree"},
                    "path":format!("{path}/tree"),"shape":"tree","storage":"content","required":true,"bounds":bounds}]},
            "product_relationships":{"schema":"ryeos.product_relationships.v1","relationships":relationships}});
        let declarations =
            ryeos_state::external_content::products::ProductDeclarations::from_value(
                value["build_products"].clone(),
            )?;
        let relationships: ryeos_state::external_content::products::composition::ProductRelationships = serde_json::from_value(value["product_relationships"].clone())?;
        let body = format!("{}\n", lillux::canonical_json(&value)?);
        ryeos_state::external_content::products::admission::AdmittedProductRecipeBinding {
            schema:
                ryeos_state::external_content::products::admission::PRODUCT_RECIPE_BINDING_SCHEMA
                    .into(),
            binding_name: "product_recipe".into(),
            recipe_ref: self.canonical_ref().into(),
            recipe_raw_content_digest: lillux::sha256_hex(body.as_bytes()),
            purpose: ryeos_state::external_content::products::ProductRecipePurpose::BundleReleaseV1,
            declarations_hash: declarations.content_hash()?,
            declarations,
            relationships,
        }
        .validate()?;
        Ok(body)
    }
    pub fn validate_response(&self, value: &Value, publisher: &str) -> anyhow::Result<()> {
        let signed = value["signed_config"]
            .as_str()
            .context("Core seed recipe omitted signed Config")?;
        let (line, body) = signed
            .split_once('\n')
            .context("Core seed recipe has no signature envelope")?;
        let header = lillux::signature::parse_signature_line(line, "#", None)
            .context("invalid Core seed signature envelope")?;
        anyhow::ensure!(
            value["schema"] == "ryeos.core_seed_recipe_authorization.v1"
                && value["canonical_ref"] == self.canonical_ref()
                && value["publisher_fingerprint"] == publisher
                && body == self.config_body()?
                && value["body_hash"] == lillux::sha256_hex(body.as_bytes())
                && value["signed_blob_hash"] == lillux::sha256_hex(signed.as_bytes())
                && header.signer_fingerprint == publisher
                && header.content_hash == lillux::sha256_hex(body.as_bytes()),
            "Core seed recipe response differs from exact authorized template"
        );
        Ok(())
    }
}

fn require_hash(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "Core seed coordinate must be lowercase SHA-256"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build() -> CoreSeedBuildRequest {
        CoreSeedBuildRequest {
            catalog_namespace: "official".into(),
            bundle_publication_policy_section_digest: "a".repeat(64),
            trust_epoch: 1,
            release_input: json!({"schema":"ryeos.bundle_release_input_plan.v1","project_path":"/source","bundle_name":"core",
                "authored_manifest":{"name":"core","version":"1.0.0","provides_kinds":[],"requires_kinds":[]},
                "source_snapshot_hash":"b".repeat(64),"predecessor_generation_hash":null,
                "target":{"kind":"triple","triple":"x86_64-unknown-linux-gnu"},"build_profile":"release",
                "payload_ownership_item_ref":"config:bundle-release/payload-ownership","payload_ownership_content_hash":"c".repeat(64),
                "payloads":[{"bundle":"core","binary":"ryeos-core-tools","cargo_package":"ryeos-core-tools","build_class":"release","bundle_sets":["core"]}],
                "cargo_packages":["ryeos-core-tools"],"build_classes":["release"],"requires_binary_build":true,
                "clean_output_required":true,"ambient_target_reuse_allowed":false}),
            child_product_selections: vec![],
        }
    }

    #[test]
    fn core_seed_is_explicit_and_does_not_require_an_image_or_receipt() {
        let request = build();
        request.validate().unwrap();
        assert!(
            super::super::admitted_build::AdmittedReleaseInput::from_value(&request.release_input)
                .is_err()
        );
        let body: Value = serde_json::from_str(
            &CoreSeedRecipeRequest::Build(request.clone())
                .config_body()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            body["product_relationships"]["relationships"][0]["producer"]["canonical_ref"],
            BUILD_GRAPH
        );
        assert_eq!(
            body["product_relationships"]["relationships"][0]["producer"]["parameters"],
            CoreSeedRecipeRequest::Build(request.clone()).parameters()
        );
        let mut cyclic = serde_json::to_value(request).unwrap();
        cyclic["substrate_image_digest"] = json!(format!("sha256:{}", "d".repeat(64)));
        assert!(serde_json::from_value::<CoreSeedBuildRequest>(cyclic).is_err());
    }

    #[test]
    fn core_seed_refuses_successor_and_noncore_inputs() {
        let mut successor = build();
        successor.release_input["predecessor_generation_hash"] = json!("d".repeat(64));
        assert!(successor.validate().is_err());
        let mut other = build();
        other.release_input["bundle_name"] = json!("web");
        other.release_input["authored_manifest"]["name"] = json!("web");
        assert!(other.validate().is_err());
    }

    #[test]
    fn core_capture_selects_only_distinct_core_qualification() {
        let signed = "# fixed fixture\n{}\n".to_owned();
        let recipe = CoreSeedRecipeRequest::Capture(CoreSeedCaptureRecipeRequest {
            build: build(),
            materialization_result_hash: "d".repeat(64),
            signed_tree_manifest_hash: "e".repeat(64),
            manifest_item_hash: lillux::sha256_hex(signed.as_bytes()),
            signed_manifest: signed,
        });
        let body: Value = serde_json::from_str(&recipe.config_body().unwrap()).unwrap();
        let relation = &body["product_relationships"]["relationships"][0];
        assert_eq!(relation["producer"]["canonical_ref"], CAPTURE_GRAPH);
        assert_eq!(
            relation["qualification"],
            json!({"policy_ref":QUALIFICATION_POLICY,"required_claims":[QUALIFICATION_CLAIM]})
        );
        assert_eq!(relation["producer"]["parameters"], recipe.parameters());
    }
}
