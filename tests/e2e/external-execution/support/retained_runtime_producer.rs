//! Disposable fixture source authoring for retained prebuilt runtime inputs.
//!
//! This is not a compiler producer or a product-witness constructor. The caller
//! supplies the exact recipe source; the actual daemon admits the recorded graph,
//! retains its pinned result, and captures the declared product through public
//! services. No qualification, consumer binding, or worker authority is authored
//! here. Host writes below are fixture setup only.

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use lillux::crypto::SigningKey;

pub const PRODUCER_REF: &str = "graph:fixtures/build_runtime";
pub const RECIPE_REF: &str = "config:fixtures/build_recipe";
pub const RECIPE_BINDING: &str = "product_recipe";
pub const PRODUCT_NAME: &str = "auxiliary";
pub const INPUT_PATH: &str = "products/external-runtime/bin/codex";

/// Author the existing fixture producer coordinates over exact prebuilt bytes.
///
/// The caller supplies the exact capture recipe, including `build_products`
/// and its historical relationships. A later consumer may resolve a distinct
/// signed compatible allowance; that does not rewrite this recipe or supply
/// missing qualification. This helper never reconstructs a witness.
pub fn write_retained_runtime_producer(
    project: &Path,
    signer: &SigningKey,
    recipe_source: &str,
    runtime_bytes: &[u8],
) -> anyhow::Result<()> {
    anyhow::ensure!(!runtime_bytes.is_empty(), "fixture runtime is empty");
    let runtime_path = project.join(INPUT_PATH);
    anyhow::ensure!(
        !runtime_path.exists(),
        "fixture input already exists: {}",
        runtime_path.display()
    );
    write_retained_runtime_producer_sources(project, signer, recipe_source)?;
    std::fs::create_dir_all(runtime_path.parent().expect("fixture member parent"))?;
    std::fs::write(&runtime_path, runtime_bytes)?;
    std::fs::set_permissions(&runtime_path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Author only the signed producer and recipe for an independently staged tree.
/// Product members remain exact fixture inputs; public execution and capture
/// still own the retained generation and original product witness.
pub fn write_retained_runtime_producer_sources(
    project: &Path,
    signer: &SigningKey,
    recipe_source: &str,
) -> anyhow::Result<()> {
    use ryeos_state::external_content::products::composition::ProductRelationships;
    use ryeos_state::external_content::products::{
        ProductDeclarations, ProductShape, ProductSource,
    };

    let recipe: serde_json::Value = serde_yaml::from_str(recipe_source)?;
    let declarations = ProductDeclarations::from_value(recipe["build_products"].clone())?;
    let relationships = ProductRelationships::from_value(recipe["product_relationships"].clone())?;
    relationships.validate_against(&declarations, RECIPE_BINDING)?;
    let runtime = declarations.select(PRODUCT_NAME)?;
    anyhow::ensure!(
        matches!(runtime.source, ProductSource::RetainedProject {})
            && runtime.shape == ProductShape::Tree
            && runtime.path == "products/external-runtime",
        "fixture runtime must use its exact retained input declaration"
    );
    let graph_path = project.join(".ai/graphs/fixtures/build_runtime.yaml");
    let recipe_path = project.join(".ai/config/fixtures/build_recipe.yaml");
    for path in [&graph_path, &recipe_path] {
        anyhow::ensure!(
            !path.exists(),
            "fixture input already exists: {}",
            path.display()
        );
        std::fs::create_dir_all(path.parent().expect("fixture member parent"))?;
    }
    let graph = r#"version: "1.0.0"
category: fixtures
description: Retain exact prebuilt fixture inputs without claiming compiler production
effects: recorded
product_recipe: config:fixtures/build_recipe
requires:
  capabilities:
    declared:
      - ryeos.execute.config.fixtures/build_recipe
config:
  start: retained
  max_steps: 1
  on_error: fail
  config_schema:
    type: object
    properties: {}
    additionalProperties: false
  nodes:
    retained:
      node_type: return
      output:
        retained_input: auxiliary
        compiler_produced: false
        consumer_bound: false
"#;
    std::fs::write(
        graph_path,
        lillux::signature::sign_content(graph, signer, "#", None),
    )?;
    std::fs::write(
        recipe_path,
        lillux::signature::sign_content(recipe_source, signer, "#", None),
    )?;
    Ok(())
}
