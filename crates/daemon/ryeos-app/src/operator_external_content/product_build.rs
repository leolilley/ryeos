//! Accept completed output products using existing operator capture authority.
//! No caller-authored evidence, handler identity, implicit qualification, or
//! additional result head is introduced here. The effect publication caller
//! holds the shared CAS guard until its accepted object becomes reachable.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use ryeos_state::CasMutationGuard;
use ryeos_state::external_content::products::accepted_result::{
    ProductBuildAcceptance, ProductBuildAcceptedResult,
};
use ryeos_state::external_content::products::admission::{
    AdmittedProductRecipeBinding, admitted_product_producer, admitted_product_recipe,
};
use ryeos_state::external_content::products::publication::VerifiedProductWitness;
use ryeos_state::external_content::products::{
    ProductBounds, ProductProducerAdmission, ProductSource,
};
use ryeos_state::objects::AdmittedLaunchCapsule;
use serde_json::Value;

use crate::handler_context::HandlerContext;
use crate::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use crate::state::AppState;
use crate::thread_lifecycle::SealedRootExecutionRequest;

/// Capture every present output product from this exact terminal placement.
/// The root identity comes from retained execution, never a synthetic client.
pub fn accept_terminal(
    state: &AppState,
    original_capsule: &AdmittedLaunchCapsule,
    producer_thread_id: &str,
    guard: &CasMutationGuard,
) -> anyhow::Result<ProductBuildAcceptedResult> {
    ryeos_runtime::validate_runtime_thread_id(producer_thread_id).map_err(anyhow::Error::msg)?;
    let authority = state.state_store.pinned_state_authority()?;
    authority.ensure_guard(guard)?;
    let (chain_root_id, capsule_hash, capsule) = state
        .state_store
        .admitted_launch_capsule_with_coordinates(producer_thread_id)?
        .context("accepted producer has no admitted capsule")?;
    let root = state
        .state_store
        .get_authoritative_root_thread_snapshot(&chain_root_id)?
        .context("accepted producer has no authoritative root")?;
    let (terminal, continued, _) = state
        .state_store
        .get_authoritative_thread_snapshot_with_continuation_presence(
            &chain_root_id,
            producer_thread_id,
        )?
        .context("accepted producer terminal is absent")?;
    if continued {
        bail!("accepted producer placement has a continuation successor");
    }
    let operator = root
        .requested_by
        .as_deref()
        .context("accepted producer has no operator owner")?;
    let context = current_operator_context(state, operator, &root.origin_site_id)?;
    let snapshot_hash = terminal
        .result_project_snapshot_hash
        .as_deref()
        .context("accepted producer has no terminal retained generation")?;
    super::retained_result::authorize_terminal_result(
        &root,
        &terminal,
        &chain_root_id,
        producer_thread_id,
        snapshot_hash,
        operator,
    )?;
    if terminal.admitted_launch_capsule_hash.as_deref() != Some(&capsule_hash)
        || terminal.project_authority != capsule.project_authority
        || terminal.origin_site_id != root.origin_site_id
    {
        bail!("accepted producer terminal contradicts its admitted execution authority");
    }
    let sealed = SealedRootExecutionRequest::decode_from_admitted_capsule(&capsule)?;
    if sealed.requested_by() != Some(operator) || sealed.origin_site_id() != root.origin_site_id {
        bail!("accepted producer capsule contradicts its root operator authority");
    }
    sealed.validate_current_operator_authority(state)?;
    let partition = capsule
        .project_authority
        .workspace_outputs()
        .context("accepted producer has no output partition")?;
    let recipe = admitted_product_recipe(&capsule, &partition.partition.recipe_binding)?;
    let (root_producer, root_capsule) = verified_root_producer(
        state,
        &chain_root_id,
        producer_thread_id,
        &recipe.binding_name,
        guard,
    )?;
    if root_capsule.content_hash()? != original_capsule.content_hash()? {
        bail!("accepted producer root differs from the original admitted operation");
    }
    validate_recipe_lane(&recipe)?;
    if recipe.recipe_ref != partition.partition.recipe_ref
        || recipe.recipe_raw_content_digest != partition.partition.recipe_raw_content_digest
        || recipe.declarations_hash != partition.partition.declarations_hash
    {
        bail!("accepted producer recipe contradicts its admitted output partition");
    }
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let requests = recipe
        .declarations
        .products
        .iter()
        .map(|declaration| super::products::ProductRequest {
            chain_root_id: chain_root_id.clone(),
            thread_id: producer_thread_id.into(),
            recipe_binding: recipe.binding_name.clone(),
            product_name: declaration.name.clone(),
        })
        .collect();
    let responses =
        super::products::capture_batch_blocking(Arc::new(state.clone()), context, requests, guard)?;
    let mut witnesses = Vec::new();
    for (declaration, response) in recipe.declarations.products.iter().zip(responses) {
        match response {
            super::products::ProductResponse::Captured { witness_hash, .. } => {
                witnesses.push(super::retained_product::load_current_product(
                    state,
                    &authority,
                    guard,
                    limits,
                    operator,
                    &witness_hash,
                )?);
            }
            super::products::ProductResponse::AbsentOptional { .. } if !declaration.required => {}
            _ => bail!("accepted producer did not publish a required product"),
        }
    }
    let inputs = inputs(&witnesses);
    let result = ProductBuildAcceptedResult::from_authenticated_products(
        operator,
        state.identity.verifying_key(),
        &inputs,
    )?;
    verify_expected_producer(
        &result,
        &root_producer,
        &partition.partition.partition_identity,
    )?;
    Ok(result)
}

/// Current replay eligibility. The expected capsule is the already-admitted
/// producer, not an arbitrary cache key or a historical mutable workspace.
pub fn verify_current(
    state: &AppState,
    guard: &CasMutationGuard,
    value: &Value,
    expected_producer_capsule: &AdmittedLaunchCapsule,
) -> anyhow::Result<ProductBuildAcceptedResult> {
    let authority = state.state_store.pinned_state_authority()?;
    authority.ensure_guard(guard)?;
    let cas = authority.cas_store()?;
    expected_producer_capsule.verify_retained_execution_realization(
        &cas,
        &authority.large_object_store()?,
        authority.trust_store(),
    )?;
    let sealed =
        SealedRootExecutionRequest::decode_from_admitted_capsule(expected_producer_capsule)?;
    let owner = sealed
        .requested_by()
        .context("accepted result producer has no operator owner")?;
    current_operator_context(state, owner, sealed.origin_site_id())?;
    sealed.validate_current_operator_authority(state)?;
    let result = ProductBuildAcceptedResult::from_value(value)?;
    if result.owner_principal != owner {
        bail!("accepted result belongs to another operator");
    }
    let outputs = expected_producer_capsule
        .project_authority
        .workspace_outputs()
        .context("accepted result requires an admitted producer output partition")?;
    let recipe =
        admitted_product_recipe(expected_producer_capsule, &outputs.partition.recipe_binding)?;
    validate_recipe_lane(&recipe)?;
    verify_required_product_coverage(&recipe.declarations.products, &result.products)?;
    let expected_producer = admitted_product_producer(expected_producer_capsule)?;
    verify_expected_producer(
        &result,
        &expected_producer,
        &outputs.partition.partition_identity,
    )?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let policy = state.node_policy.require::<crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord>()?;
    let node_bounds = ryeos_state::LargeContentCaptureBounds {
        max_entries: policy.limits.max_entries,
        max_depth: policy.limits.max_depth,
        max_file_bytes: policy.limits.max_file_bytes,
        max_total_bytes: policy.limits.max_total_bytes,
    };
    let mut witnesses = Vec::with_capacity(result.products.len());
    for product in &result.products {
        if product.qualification_hash.is_some() {
            bail!("accepted build lane does not infer independent product qualification");
        }
        let witness = super::retained_product::load_current_product(
            state,
            &authority,
            guard,
            limits,
            owner,
            &product.witness_hash,
        )?;
        if witness.evidence.root_producer != expected_producer
            || witness.evidence.declaration != *recipe.declarations.select(&product.product_name)?
            || witness.evidence.declarations_hash != recipe.declarations_hash
            || witness.evidence.recipe_binding != recipe.binding_name
            || witness.evidence.recipe_ref != recipe.recipe_ref
            || witness.evidence.recipe_raw_content_digest != recipe.recipe_raw_content_digest
            || witness.evidence.relationships != recipe.relationships
        {
            bail!("accepted product differs from the expected admitted producer recipe");
        }
        let bounds = witness
            .evidence
            .declaration
            .bounds
            .intersect(&node_bounds)?;
        ryeos_state::external_content::products::publication::verify_product_manifest_against_bounds(
            &authority, &witness.evidence, &ProductBounds {
                maximum_entries: bounds.max_entries, maximum_depth: bounds.max_depth,
                maximum_file_bytes: bounds.max_file_bytes, maximum_total_bytes: bounds.max_total_bytes,
            }, limits, guard,
        )?;
        witnesses.push(witness);
    }
    result.validate_against_authenticated_products(
        owner,
        state.identity.verifying_key(),
        &inputs(&witnesses),
    )?;
    Ok(result)
}

fn inputs(witnesses: &[VerifiedProductWitness]) -> Vec<ProductBuildAcceptance<'_>> {
    witnesses
        .iter()
        .map(|product| ProductBuildAcceptance {
            product,
            qualification: None,
        })
        .collect()
}

/// Authenticate original input and every actual machine placement before
/// publishing compact root/terminal testimony. No historical object becomes
/// a product owning edge; the node attests this checked lineage at capture.
pub(super) fn verified_root_producer(
    state: &AppState,
    chain_root_id: &str,
    terminal_thread_id: &str,
    recipe_binding: &str,
    guard: &CasMutationGuard,
) -> anyhow::Result<(ProductProducerAdmission, AdmittedLaunchCapsule)> {
    let authority = state.state_store.pinned_state_authority()?;
    authority.ensure_guard(guard)?;
    let (_, lineage) = state
        .state_store
        .get_authoritative_machine_continuation_lineage(chain_root_id, terminal_thread_id, guard)?
        .context("product producer lineage is absent")?;
    let root = lineage
        .first()
        .context("product producer lineage has no root")?;
    let terminal = lineage
        .last()
        .context("product producer lineage has no terminal")?;
    let owner = root
        .requested_by
        .as_deref()
        .context("product producer root has no owner")?;
    current_operator_context(state, owner, &root.origin_site_id)?;
    let result = terminal
        .result_project_snapshot_hash
        .as_deref()
        .context("product producer terminal has no retained result")?;
    super::retained_result::authorize_terminal_result(
        root,
        terminal,
        chain_root_id,
        terminal_thread_id,
        result,
        owner,
    )?;
    let cas = authority.cas_store()?;
    let large_store = authority.large_object_store()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let mut original: Option<(AdmittedLaunchCapsule, AdmittedProductRecipeBinding)> = None;
    for placement in &lineage {
        if placement.requested_by.as_deref() != Some(owner)
            || placement.origin_site_id != root.origin_site_id
            || placement.current_site_id != root.current_site_id
        {
            bail!("product producer continuation changed owner or placement site");
        }
        let hash = placement
            .admitted_launch_capsule_hash
            .as_deref()
            .context("product producer placement has no admitted capsule")?;
        let capsule = AdmittedLaunchCapsule::from_current_value(
            ryeos_state::object_closure::load_exact_cas_object_with_cas(
                &cas,
                hash,
                limits.max_object_bytes,
            )?,
        )?;
        capsule.verify_retained_execution_realization(
            &cas,
            &large_store,
            authority.trust_store(),
        )?;
        if capsule.content_hash()? != hash
            || capsule.project_authority != placement.project_authority
        {
            bail!("product producer placement contradicts its immutable capsule authority");
        }
        let sealed = SealedRootExecutionRequest::decode_from_admitted_capsule(&capsule)?;
        if sealed.item_ref() != placement.item_ref
            || sealed.requested_by() != Some(owner)
            || sealed.origin_site_id() != root.origin_site_id
        {
            bail!("product producer placement contradicts its sealed invocation");
        }
        let recipe = admitted_product_recipe(&capsule, recipe_binding)?;
        if let Some((original_capsule, original_recipe)) = &original {
            if recipe != *original_recipe
                || placement.item_ref != root.item_ref
                || capsule
                    .project_authority
                    .workspace_outputs()
                    .map(|value| &value.partition)
                    != original_capsule
                        .project_authority
                        .workspace_outputs()
                        .map(|value| &value.partition)
            {
                bail!(
                    "product producer continuation changed its admitted recipe or output partition"
                );
            }
        } else {
            original = Some((capsule, recipe));
        }
    }
    let (capsule, _) = original.context("product producer root capsule is absent")?;
    Ok((admitted_product_producer(&capsule)?, capsule))
}

fn current_operator_context(
    state: &AppState,
    owner: &str,
    origin: &str,
) -> anyhow::Result<HandlerContext> {
    let context =
        crate::operator_authority::retained_admitted_operator_authority(state, owner, origin)?
            .handler_context();
    // The existing retained authority owner checks the live grant and exact
    // origin before reconstructing this context; never substitute a local key.
    crate::operator_authority::require_admitted_operator(state, &context)?;
    Ok(context)
}

fn validate_recipe_lane(recipe: &AdmittedProductRecipeBinding) -> anyhow::Result<()> {
    recipe.validate()?;
    if recipe.declarations.products.is_empty()
        || recipe
            .declarations
            .products
            .iter()
            .any(|product| !matches!(product.source, ProductSource::WorkspaceOutput { .. }))
    {
        bail!("accepted build lane requires named workspace-output products only");
    }
    Ok(())
}

fn verify_required_product_coverage(
    declarations: &[ryeos_state::external_content::products::ProductDeclaration],
    products: &[ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedProduct],
) -> anyhow::Result<()> {
    for declaration in declarations
        .iter()
        .filter(|declaration| declaration.required)
    {
        if !products
            .iter()
            .any(|product| product.product_name == declaration.name)
        {
            bail!(
                "accepted build result omits required product `{}`",
                declaration.name
            );
        }
    }
    Ok(())
}

fn verify_expected_producer(
    result: &ProductBuildAcceptedResult,
    producer: &ProductProducerAdmission,
    partition: &str,
) -> anyhow::Result<()> {
    result.validate()?;
    producer.validate()?;
    if result.producer_ref != producer.canonical_ref
        || result.producer_project_snapshot_hash != producer.producer_project_snapshot_hash
        || result.producer_effective_definition_digest != producer.effective_definition_digest
        || result.producer_parameters_digest != producer.admitted_parameters_digest
        || result.producer_partition_identity != partition
    {
        bail!("accepted build result does not match the expected admitted producer");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::external_content::products::accepted_result::{
        PRODUCT_BUILD_ACCEPTED_RESULT_KIND, PRODUCT_BUILD_ACCEPTED_RESULT_SCHEMA,
        ProductBuildAcceptedProduct,
    };

    #[test]
    fn current_result_requires_every_required_product_but_allows_optional_absence() {
        use ryeos_state::external_content::products::{
            ProductDeclaration, ProductShape, ProductStorage,
        };
        let declaration = |name: &str, required| ProductDeclaration {
            name: name.into(),
            source: ProductSource::WorkspaceOutput {
                root: "output".into(),
            },
            path: format!("products/{name}"),
            shape: ProductShape::Tree,
            storage: ProductStorage::Content,
            required,
            bounds: ProductBounds {
                maximum_entries: 8,
                maximum_depth: 4,
                maximum_file_bytes: 1024,
                maximum_total_bytes: 4096,
            },
            expected_manifest_hash: None,
        };
        let declarations = [
            declaration("runtime", true),
            declaration("shell", true),
            declaration("symbols", false),
        ];
        for value in &declarations {
            value.validate().unwrap();
        }
        let product = |name: &str| ProductBuildAcceptedProduct {
            product_name: name.into(),
            witness_hash: "a".repeat(64),
            qualification_hash: None,
        };
        assert!(verify_required_product_coverage(&declarations, &[]).is_err());
        let missing =
            verify_required_product_coverage(&declarations, &[product("runtime")]).unwrap_err();
        assert!(missing.to_string().contains("shell"));
        assert!(
            verify_required_product_coverage(
                &declarations,
                &[product("runtime"), product("symbols")]
            )
            .is_err()
        );
        verify_required_product_coverage(&declarations, &[product("runtime"), product("shell")])
            .unwrap();
        verify_required_product_coverage(
            &declarations,
            &[product("runtime"), product("shell"), product("symbols")],
        )
        .unwrap();
    }

    #[test]
    fn accepted_result_must_match_all_expected_admitted_producer_coordinates() {
        let producer = ProductProducerAdmission {
            canonical_ref: "graph:example/build".into(),
            effective_definition_digest: "1".repeat(64),
            exact_program_hash: "2".repeat(64),
            producer_project_snapshot_hash: "3".repeat(64),
            launch_authority_digest: "4".repeat(64),
            admitted_parameters_digest: "5".repeat(64),
        };
        let result = ProductBuildAcceptedResult {
            kind: PRODUCT_BUILD_ACCEPTED_RESULT_KIND.into(),
            schema: PRODUCT_BUILD_ACCEPTED_RESULT_SCHEMA.into(),
            owner_principal: format!("fp:{}", "6".repeat(64)),
            producer_ref: producer.canonical_ref.clone(),
            producer_effective_definition_digest: producer.effective_definition_digest.clone(),
            producer_parameters_digest: producer.admitted_parameters_digest.clone(),
            producer_project_snapshot_hash: producer.producer_project_snapshot_hash.clone(),
            producer_partition_identity: "7".repeat(64),
            products: vec![ProductBuildAcceptedProduct {
                product_name: "runtime".into(),
                witness_hash: "8".repeat(64),
                qualification_hash: None,
            }],
        };
        verify_expected_producer(&result, &producer, &"7".repeat(64)).unwrap();
        for mutate in [
            (|value: &mut ProductBuildAcceptedResult| {
                value.producer_ref = "graph:example/other".into()
            }) as fn(&mut ProductBuildAcceptedResult),
            |value| value.producer_effective_definition_digest = "a".repeat(64),
            |value| value.producer_parameters_digest = "a".repeat(64),
            |value| value.producer_project_snapshot_hash = "a".repeat(64),
            |value| value.producer_partition_identity = "a".repeat(64),
        ] {
            let mut changed = result.clone();
            mutate(&mut changed);
            assert!(verify_expected_producer(&changed, &producer, &"7".repeat(64)).is_err());
        }
    }
}
