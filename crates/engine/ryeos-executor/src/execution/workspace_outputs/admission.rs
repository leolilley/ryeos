//! Derive reservations from an already interpreted signed recipe. The managed
//! launch owner conditions provenance and root admission together before birth.

use super::super::launch_preparation::PreparedRuntimeLaunch;
use anyhow::{Context as _, bail};
use ryeos_app::node_policy::sections::external_content::ExternalContentImportPolicyRecord;
use ryeos_app::state::AppState;
use ryeos_engine::external_content::{
    ExternalContentMountRoot, authored_external_content_shape, declaring_authority,
};
use ryeos_engine::resolution::ResolutionOutput;
use ryeos_state::external_content::products::ProductSource;
use ryeos_state::external_content::products::admission::{
    PRODUCT_RECIPE_BINDING_SCHEMA, admitted_product_recipe_from_prepared,
};
use ryeos_state::objects::workspace_output_capture::{
    WORKSPACE_OUTPUT_PARTITION_SCHEMA, WorkspaceOutputPartition, WorkspaceOutputRoot,
};

/// Ordinary graphs with no output recipe leave existing authority unchanged.
pub(crate) fn derive_initial_partition(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    resolution: &ResolutionOutput,
    prepared: &PreparedRuntimeLaunch,
    snapshot_hash: Option<&str>,
) -> anyhow::Result<Option<WorkspaceOutputPartition>> {
    let mut recipes = prepared.runtime_facts.iter().filter(|(_, fact)| {
        fact.get("schema").and_then(serde_json::Value::as_str)
            == Some(PRODUCT_RECIPE_BINDING_SCHEMA)
    });
    let Some((binding, _)) = recipes.next() else {
        return Ok(None);
    };
    if recipes.next().is_some() {
        bail!("one workspace cannot admit competing product recipe partitions");
    }
    let recipe = admitted_product_recipe_from_prepared(&serde_json::to_value(prepared)?, binding)?;
    if recipe.declarations.output_roots.is_empty() {
        return Ok(None);
    }
    let snapshot_hash =
        snapshot_hash.context("workspace outputs require an admitted pinned generation")?;
    let node = &state
        .node_policy
        .require::<ExternalContentImportPolicyRecord>()?
        .limits;
    let bounds = ryeos_state::LargeContentCaptureBounds {
        max_depth: node.max_depth,
        max_entries: node.max_entries,
        max_file_bytes: node.max_file_bytes,
        max_total_bytes: node.max_total_bytes,
    };
    bounds.validate()?;
    let authority = state.state_store.pinned_state_authority()?;
    let _guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let source = ryeos_state::project_materialization::VerifiedProjectSnapshotClosure::load(
        &cas,
        snapshot_hash,
    )?;
    let policy = source.tree().policy();
    let matcher =
        ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
            patterns: policy.node_patterns.clone(),
        })?;
    let mut roots = Vec::new();
    for declaration in &recipe.declarations.output_roots {
        // Check floor and every ancestor, even when the output is absent.
        ryeos_state::ExternalCapturePolicy::new(declaration.path.clone(), &matcher)?;
        let root = WorkspaceOutputRoot::admit(declaration, &bounds)?;
        for source_path in source.tree().tree().files.keys() {
            if paths_overlap(source_path, &root.path) {
                bail!(
                    "workspace output `{}` overlaps admitted source `{source_path}`",
                    root.name
                );
            }
        }
        roots.push(root);
    }
    let mut partition = WorkspaceOutputPartition {
        schema: WORKSPACE_OUTPUT_PARTITION_SCHEMA.to_owned(),
        recipe_binding: recipe.binding_name,
        recipe_ref: recipe.recipe_ref,
        recipe_raw_content_digest: recipe.recipe_raw_content_digest,
        declarations_hash: recipe.declarations_hash,
        project_snapshot_policy_hash: source.snapshot().effective_policy_hash.clone(),
        roots,
        products: recipe
            .declarations
            .products
            .into_iter()
            .filter(|product| matches!(product.source, ProductSource::WorkspaceOutput { .. }))
            .collect(),
        partition_identity: "0".repeat(64),
        capture_policy_digest: "0".repeat(64),
    };
    partition
        .products
        .sort_by(|left, right| left.name.cmp(&right.name));
    partition.partition_identity = partition.derived_partition_identity()?;
    partition.capture_policy_digest = partition.derived_capture_policy_digest(policy)?;
    partition.validate_source_output_pair(source.snapshot(), source.snapshot(), policy)?;
    validate_input_mounts(engine, resolution, prepared, &partition)?;
    Ok(Some(partition))
}

/// A child cannot mount an input over an inherited writable output root.
pub(crate) fn validate_input_mounts(
    engine: &ryeos_engine::engine::Engine,
    resolution: &ResolutionOutput,
    prepared: &PreparedRuntimeLaunch,
    partition: &WorkspaceOutputPartition,
) -> anyhow::Result<()> {
    partition.validate()?;
    for attachment in &prepared.evidence_attachments {
        attachment.validate()?;
        for output in &partition.roots {
            if paths_overlap(&output.path, &attachment.destination_path) {
                bail!(
                    "workspace output `{}` overlaps admitted evidence `{}`",
                    output.name,
                    attachment.destination_path
                );
            }
        }
    }
    let root_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(&resolution.root.resolved_ref)?;
    let kind = engine
        .kinds
        .get(&root_ref.kind)
        .context("output-bearing launch kind is absent")?;
    validate_resolution_mounts(resolution, kind.external_content_contract(), partition)?;
    for dependency in prepared.execution_dependencies.values() {
        let reference = ryeos_engine::canonical_ref::CanonicalRef::parse(
            &dependency.resolution.root.resolved_ref,
        )?;
        let kind = engine
            .kinds
            .get(&reference.kind)
            .context("output-bearing launch dependency kind is absent")?;
        validate_resolution_mounts(
            &dependency.resolution,
            kind.external_content_contract(),
            partition,
        )?;
    }
    for dependency in prepared.content_dependencies.values() {
        validate_resolution_mounts(
            &dependency.resolution.restore(),
            Some(&dependency.external_content_policy.declaration_contract()),
            partition,
        )?;
    }
    Ok(())
}

/// The authoritative preparation pass may not change the recipe after output
/// authority was conditioned. Descendants can inherit without their own recipe;
/// fresh producers must still carry the exact fact which reserved their roots.
pub(crate) fn verify_prepared_partition(
    prepared: &PreparedRuntimeLaunch,
    partition: &WorkspaceOutputPartition,
    require_own_recipe: bool,
) -> anyhow::Result<()> {
    partition.validate()?;
    let names = prepared
        .runtime_facts
        .iter()
        .filter_map(|(name, fact)| {
            (fact.get("schema").and_then(serde_json::Value::as_str)
                == Some(PRODUCT_RECIPE_BINDING_SCHEMA))
            .then_some(name)
        })
        .collect::<Vec<_>>();
    if names.is_empty() && !require_own_recipe {
        return Ok(());
    }
    if names.len() != 1 || names[0] != &partition.recipe_binding {
        bail!("prepared output recipe disappeared or changed after admission");
    }
    let recipe = admitted_product_recipe_from_prepared(
        &serde_json::to_value(prepared)?,
        &partition.recipe_binding,
    )?;
    let mut products = recipe
        .declarations
        .products
        .iter()
        .filter(|product| matches!(product.source, ProductSource::WorkspaceOutput { .. }))
        .cloned()
        .collect::<Vec<_>>();
    products.sort_by(|left, right| left.name.cmp(&right.name));
    if recipe.recipe_ref != partition.recipe_ref
        || recipe.recipe_raw_content_digest != partition.recipe_raw_content_digest
        || recipe.declarations_hash != partition.declarations_hash
        || products != partition.products
        || recipe.declarations.output_roots.len() != partition.roots.len()
        || recipe
            .declarations
            .output_roots
            .iter()
            .zip(&partition.roots)
            .any(|(declared, admitted)| {
                declared.name != admitted.name
                    || declared.path != admitted.path
                    || declared.storage != admitted.storage
                    || declared.bounds != admitted.declared_bounds
            })
    {
        bail!("prepared recipe contradicts the admitted workspace output partition");
    }
    Ok(())
}

/// Detect output requirements through the admitted recipe owner, not by
/// interpreting authored configuration fields in generic launch code.
pub(crate) fn requires_output_partition(prepared: &PreparedRuntimeLaunch) -> anyhow::Result<bool> {
    let value = serde_json::to_value(prepared)?;
    for (binding, fact) in &prepared.runtime_facts {
        if fact.get("schema").and_then(serde_json::Value::as_str)
            == Some(PRODUCT_RECIPE_BINDING_SCHEMA)
            && admitted_product_recipe_from_prepared(&value, binding)?
                .declarations
                .requires_workspace_output_capture()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Current policy may refuse retained authority; it must not silently recompute
/// the partition's capture contract on restart or continuation.
pub(crate) fn validate_current_bounds(
    state: &AppState,
    partition: &WorkspaceOutputPartition,
) -> anyhow::Result<()> {
    partition.validate()?;
    let limits = &state
        .node_policy
        .require::<ExternalContentImportPolicyRecord>()?
        .limits;
    for root in &partition.roots {
        let bounds = &root.effective_bounds;
        if bounds.maximum_entries > limits.max_entries
            || bounds.maximum_depth > limits.max_depth
            || bounds.maximum_file_bytes > limits.max_file_bytes
            || bounds.maximum_total_bytes > limits.max_total_bytes
        {
            bail!(
                "current node policy refuses retained workspace output bounds for `{}`",
                root.name
            );
        }
    }
    Ok(())
}

fn validate_resolution_mounts(
    resolution: &ResolutionOutput,
    contract: Option<&ryeos_engine::kind_registry::KindExternalContentDecl>,
    partition: &WorkspaceOutputPartition,
) -> anyhow::Result<()> {
    for mount in super::super::external_content::admitted_realization_mounts(resolution)? {
        for output in &partition.roots {
            if paths_overlap(&output.path, &mount) {
                bail!(
                    "workspace output `{}` overlaps realized input `{mount}`",
                    output.name
                );
            }
        }
    }
    let Some(shape) = authored_external_content_shape(
        &resolution.composed.composed,
        contract,
        declaring_authority(resolution)?,
    )?
    else {
        return Ok(());
    };
    // Literal and deferred declarations already own their destination. A
    // witness selection cannot choose a different mount.
    for (mount_root, mount) in shape
        .literal_declarations
        .iter()
        .map(|entry| (entry.mount_root, entry.mount.as_str()))
        .chain(
            shape
                .product_slots
                .iter()
                .map(|slot| (slot.mount_root, slot.mount.as_str())),
        )
    {
        if mount_root == ExternalContentMountRoot::Project {
            for output in &partition.roots {
                if paths_overlap(&output.path, mount) {
                    bail!(
                        "workspace output `{}` overlaps admitted input mount `{mount}`",
                        output.name
                    );
                }
            }
        }
    }
    Ok(())
}

fn paths_overlap(left: &str, right: &str) -> bool {
    let left = std::path::Path::new(left);
    let right = std::path::Path::new(right);
    left.starts_with(right) || right.starts_with(left)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use ryeos_engine::contracts::{ItemSourceRoot, ItemSpace};
    use ryeos_engine::resolution::{
        KindComposedView, ResolutionDigestNode, ResolutionOutput, ResolutionStepName,
        ResolvedAncestor, TrustClass,
    };
    use ryeos_state::external_content::products::admission::AdmittedProductRecipeBinding;
    use ryeos_state::external_content::products::composition::ProductRelationships;
    use ryeos_state::external_content::products::{
        PRODUCT_DECLARATIONS_SCHEMA, ProductBounds, ProductDeclaration, ProductDeclarations,
        ProductShape, ProductStorage,
    };
    use ryeos_state::objects::WorkspaceOutputRootDeclaration;

    fn bounds() -> ProductBounds {
        ProductBounds {
            maximum_entries: 16,
            maximum_depth: 6,
            maximum_file_bytes: 4_096,
            maximum_total_bytes: 16_384,
        }
    }

    fn declarations() -> ProductDeclarations {
        ProductDeclarations {
            schema: PRODUCT_DECLARATIONS_SCHEMA.to_owned(),
            output_roots: vec![
                WorkspaceOutputRootDeclaration {
                    name: "distribution_root".to_owned(),
                    path: "products/distribution".to_owned(),
                    storage: ProductStorage::LargeContent,
                    bounds: bounds(),
                },
                WorkspaceOutputRootDeclaration {
                    name: "scratch_root".to_owned(),
                    path: "products/scratch".to_owned(),
                    storage: ProductStorage::LargeContent,
                    bounds: bounds(),
                },
            ],
            products: vec![
                ProductDeclaration {
                    name: "distribution".to_owned(),
                    source: ProductSource::WorkspaceOutput {
                        root: "distribution_root".to_owned(),
                    },
                    path: "products/distribution".to_owned(),
                    shape: ProductShape::Tree,
                    storage: ProductStorage::LargeContent,
                    required: true,
                    bounds: bounds(),
                    expected_manifest_hash: None,
                },
                ProductDeclaration {
                    name: "runtime".to_owned(),
                    source: ProductSource::WorkspaceOutput {
                        root: "distribution_root".to_owned(),
                    },
                    path: "products/distribution/runtime".to_owned(),
                    shape: ProductShape::Tree,
                    storage: ProductStorage::LargeContent,
                    required: true,
                    bounds: bounds(),
                    expected_manifest_hash: None,
                },
            ],
        }
    }

    fn admitted_recipe() -> AdmittedProductRecipeBinding {
        let declarations = declarations();
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

    fn binding_record(
        recipe: &AdmittedProductRecipeBinding,
    ) -> super::super::super::launch_preparation::RefBindingLaunchRecord {
        super::super::super::launch_preparation::RefBindingLaunchRecord {
            canonical_ref: recipe.recipe_ref.clone(),
            source_space: ItemSpace::Project,
            effective_trust_class: TrustClass::TrustedProject,
            resolution: ryeos_engine::resolution::AsLaunchedResolutionDigest {
                root: ResolutionDigestNode {
                    requested_id: recipe.recipe_ref.clone(),
                    resolved_ref: recipe.recipe_ref.clone(),
                    source_space: ItemSpace::Project,
                    source_root: ItemSourceRoot::Project,
                    trust_class: TrustClass::TrustedProject,
                    signer_fingerprint: Some("b".repeat(64)),
                    raw_content_digest: recipe.recipe_raw_content_digest.clone(),
                },
                ancestors: Vec::new(),
                referenced_items: Vec::new(),
                effective_trust_class: TrustClass::TrustedProject,
                policy_facts: Default::default(),
            },
        }
    }

    fn prepared(recipe: Option<&AdmittedProductRecipeBinding>) -> PreparedRuntimeLaunch {
        PreparedRuntimeLaunch {
            project_result_requirement:
                ryeos_handler_protocol::ProjectResultRequirement::RetainedGeneration,
            filesystem_authority_ceiling:
                ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
            network_authority_ceiling:
                ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::NodePolicy,
            runtime_data: BTreeMap::new(),
            required_secrets: Vec::new(),
            runtime_facts: recipe
                .map(|recipe| {
                    BTreeMap::from([(
                        recipe.binding_name.clone(),
                        serde_json::to_value(recipe).unwrap(),
                    )])
                })
                .unwrap_or_default(),
            binding_records: recipe
                .map(|recipe| {
                    BTreeMap::from([(recipe.binding_name.clone(), binding_record(recipe))])
                })
                .unwrap_or_default(),
            execution_dependencies: BTreeMap::new(),
            content_dependencies: BTreeMap::new(),
            evidence_attachments: Vec::new(),
            environment_contributions: BTreeMap::new(),
            admitted_sessions: BTreeMap::new(),
            config_contributors: Vec::new(),
            financial_authority: None,
            external_effect_authority: None,
        }
    }

    fn partition(recipe: &AdmittedProductRecipeBinding) -> WorkspaceOutputPartition {
        let roots = recipe
            .declarations
            .output_roots
            .iter()
            .map(|root| WorkspaceOutputRoot {
                name: root.name.clone(),
                path: root.path.clone(),
                storage: root.storage,
                declared_bounds: root.bounds.clone(),
                effective_bounds: root.bounds.clone(),
            })
            .collect();
        let mut result = WorkspaceOutputPartition {
            schema: WORKSPACE_OUTPUT_PARTITION_SCHEMA.to_owned(),
            recipe_binding: recipe.binding_name.clone(),
            recipe_ref: recipe.recipe_ref.clone(),
            recipe_raw_content_digest: recipe.recipe_raw_content_digest.clone(),
            declarations_hash: recipe.declarations_hash.clone(),
            project_snapshot_policy_hash: "c".repeat(64),
            roots,
            products: recipe.declarations.products.clone(),
            partition_identity: String::new(),
            capture_policy_digest: "d".repeat(64),
        };
        result.partition_identity = result.derived_partition_identity().unwrap();
        result.validate().unwrap();
        result
    }

    fn resolution_with_content(composed: serde_json::Value) -> ResolutionOutput {
        ResolutionOutput {
            root: ResolvedAncestor {
                requested_id: "config:test/consumer".to_owned(),
                resolved_ref: "config:test/consumer".to_owned(),
                source_path: "/fixture/consumer.yaml".into(),
                source_space: ItemSpace::Project,
                source_root: ItemSourceRoot::Project,
                trust_class: TrustClass::TrustedProject,
                signer_fingerprint: Some("e".repeat(64)),
                alias_resolution: None,
                added_by: ResolutionStepName::PipelineInit,
                raw_content: "fixture".to_owned(),
                source_content_digest: "f".repeat(64),
                raw_content_digest: "1".repeat(64),
            },
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: Default::default(),
            effective_trust_class: TrustClass::TrustedProject,
            composed: KindComposedView {
                composed,
                derived: Default::default(),
                policy_facts: Default::default(),
            },
        }
    }

    fn content_contract() -> ryeos_engine::kind_registry::KindExternalContentDecl {
        ryeos_engine::kind_registry::KindExternalContentDecl {
            realization_derived: ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY
                .to_owned(),
            allowed_roots: vec!["project_files".to_owned()],
            allowed_mount_roots: vec![ExternalContentMountRoot::Project],
            max_declarations: 8,
            large_content: None,
        }
    }

    #[test]
    fn exact_recipe_survives_revalidation_and_inherited_child_may_omit_it() {
        let recipe = admitted_recipe();
        let partition = partition(&recipe);
        verify_prepared_partition(&prepared(Some(&recipe)), &partition, true).unwrap();

        let mut disappeared_after_preparation = prepared(Some(&recipe));
        disappeared_after_preparation.runtime_facts.clear();
        assert!(
            verify_prepared_partition(&disappeared_after_preparation, &partition, true).is_err()
        );

        let inherited = prepared(None);
        verify_prepared_partition(&inherited, &partition, false).unwrap();
        assert!(verify_prepared_partition(&inherited, &partition, true).is_err());
        assert!(requires_output_partition(&prepared(Some(&recipe))).unwrap());
        assert!(!requires_output_partition(&inherited).unwrap());
    }

    #[test]
    fn revalidation_refuses_binding_ref_declaration_and_order_drift() {
        let recipe = admitted_recipe();
        let partition = partition(&recipe);

        let mut changed = prepared(Some(&recipe));
        let fact = changed.runtime_facts.remove("product_recipe").unwrap();
        changed
            .runtime_facts
            .insert("other_recipe".to_owned(), fact);
        assert!(verify_prepared_partition(&changed, &partition, true).is_err());

        let mut changed = prepared(Some(&recipe));
        changed
            .binding_records
            .get_mut("product_recipe")
            .unwrap()
            .canonical_ref = "config:test/other-products".to_owned();
        assert!(verify_prepared_partition(&changed, &partition, true).is_err());

        let mut changed = prepared(Some(&recipe));
        changed
            .binding_records
            .get_mut("product_recipe")
            .unwrap()
            .resolution
            .root
            .raw_content_digest = "9".repeat(64);
        assert!(verify_prepared_partition(&changed, &partition, true).is_err());

        let mut changed_recipe = recipe.clone();
        changed_recipe.declarations.products[1].required = false;
        changed_recipe.declarations_hash = changed_recipe.declarations.content_hash().unwrap();
        changed_recipe.validate().unwrap();
        let changed = prepared(Some(&changed_recipe));
        assert!(verify_prepared_partition(&changed, &partition, true).is_err());

        let mut reordered_recipe = serde_json::to_value(&recipe).unwrap();
        reordered_recipe["declarations"]["output_roots"]
            .as_array_mut()
            .unwrap()
            .reverse();
        let mut changed = prepared(Some(&recipe));
        changed
            .runtime_facts
            .insert("product_recipe".to_owned(), reordered_recipe);
        assert!(verify_prepared_partition(&changed, &partition, true).is_err());
    }

    #[test]
    fn literal_and_slot_mounts_cannot_cross_output_components() {
        let recipe = admitted_recipe();
        let partition = partition(&recipe);
        let contract = content_contract();
        for mount in [
            "products",
            "products/distribution",
            "products/distribution/input",
        ] {
            let literal = resolution_with_content(serde_json::json!({
                "external_content": [{
                    "id": "input", "kind": "tree", "mode": "pinned",
                    "digest": "2".repeat(64), "mount_root": "project", "mount": mount
                }]
            }));
            assert!(validate_resolution_mounts(&literal, Some(&contract), &partition).is_err());

            let slot = resolution_with_content(serde_json::json!({
                "external_content": [],
                "external_product_slots": [{
                    "id": "input", "relationship_ref": "config:test/recipe",
                    "relationship": "runtime_to_consumer", "kind": "tree",
                    "mount_root": "project", "mount": mount
                }]
            }));
            assert!(validate_resolution_mounts(&slot, Some(&contract), &partition).is_err());
        }

        let disjoint = resolution_with_content(serde_json::json!({
            "external_content": [{
                "id": "input", "kind": "tree", "mode": "pinned",
                "digest": "2".repeat(64), "mount_root": "project",
                "mount": "products/distribution-other"
            }]
        }));
        validate_resolution_mounts(&disjoint, Some(&contract), &partition).unwrap();
    }

    #[test]
    fn evidence_mounts_cannot_cross_output_components() {
        let recipe = admitted_recipe();
        let partition = partition(&recipe);
        let resolution = resolution_with_content(serde_json::json!({}));
        let engine = ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::dispatcher::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                std::sync::Arc::new(ryeos_engine::handlers::registry::HandlerRegistry::empty()),
            ),
            Vec::new(),
        );
        for destination in [
            "products",
            "products/distribution",
            "products/distribution/evidence",
        ] {
            let mut launch = prepared(Some(&recipe));
            let mut attachment =
                super::super::super::launch_preparation::PreparedEvidenceAttachment {
                    binding_id: "evidence".to_owned(),
                    bundle_id: "bundle".to_owned(),
                    event_kind: "event".to_owned(),
                    chain_id: "chain".to_owned(),
                    event_hash: "3".repeat(64),
                    attachment_name: "attachment".to_owned(),
                    blob_hash: "4".repeat(64),
                    size_bytes: 8,
                    media_type: Some("application/octet-stream".to_owned()),
                    target: "project".to_owned(),
                    destination_path: destination.to_owned(),
                    access: ryeos_handler_protocol::EvidenceAttachmentAccessWire::ReadOnly,
                    manifest_hash: "5".repeat(64),
                    binding_digest: String::new(),
                };
            attachment.binding_digest = attachment.reproduce_binding_digest().unwrap();
            launch.evidence_attachments.push(attachment);
            let error = validate_input_mounts(&engine, &resolution, &launch, &partition)
                .unwrap_err()
                .to_string();
            assert!(error.contains("overlaps admitted evidence"));
        }
    }

    #[test]
    fn reservations_compare_components_not_string_prefixes() {
        assert!(paths_overlap(
            "products/runtime",
            "products/runtime/lib/file"
        ));
        assert!(paths_overlap("products/runtime", "products"));
        assert!(paths_overlap("products/runtime", "products/runtime"));
        assert!(!paths_overlap("products/runtime", "products/runtime-other"));
    }
}
