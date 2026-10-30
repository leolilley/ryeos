//! Read-only emission of the admitted external candidate program for a
//! product-sourced consumer.
//!
//! The emitted program is the same value the executor derives at dispatch: the
//! consumer's selected products are admitted by the ordinary product selection,
//! the qualification use is rebuilt from the consumer's current signed source,
//! environment and runtime product, and the requirement resolves against both.
//! A placement binding composed from it therefore cannot disagree with launch
//! about the runtime manifest or the selection identity. Nothing is published
//! or retained: the staged consumer inputs are dropped unpublished.

use std::sync::Arc;

use anyhow::{Context as _, bail, ensure};
use ryeos_state::external_content::products::composition::{
    MAX_PRODUCT_SELECTIONS, ProductSelection, ResolvedExternalProductSelections,
};
use ryeos_state::external_execution::admission::{
    AdmittedExternalCandidateProgram, ExternalCandidateRuntimeAuthority,
};
use serde::{Deserialize, Serialize};

use super::product_qualification::{
    AdmittedUseInputs, load_runtime_manifest_identity,
    prepare_current_bundle_consumer_content_inputs, realize_runtime_slot,
    require_unguarded_resolution,
};
use crate::handler_context::HandlerContext;
use crate::state::AppState;
use ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy;

/// The installed Worker whose products are selected, and the exact product
/// selections its signed slots require. Policy, relationship and verifier
/// identity all come from signed source and authenticated testimony.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProductProgramRequest {
    pub consumer_ref: String,
    pub product_selections: Vec<ProductSelection>,
}

impl ProductProgramRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        let reference = ryeos_engine::canonical_ref::CanonicalRef::parse(&self.consumer_ref)?;
        ensure!(
            reference.kind == "worker"
                && reference.suffix.is_none()
                && reference.to_string() == self.consumer_ref
                && self.consumer_ref.len() <= 2048,
            "consumer_ref must be an exact canonical worker ref"
        );
        ensure!(
            !self.product_selections.is_empty()
                && self.product_selections.len() <= MAX_PRODUCT_SELECTIONS,
            "product_selections must be a bounded non-empty list"
        );
        for selection in &self.product_selections {
            selection.validate()?;
        }
        let mut ids: Vec<&str> = self
            .product_selections
            .iter()
            .map(|selection| selection.declaration_id.as_str())
            .collect();
        ids.sort_unstable();
        ensure!(
            ids.windows(2).all(|pair| pair[0] != pair[1]),
            "product_selections must not repeat a declaration id"
        );
        Ok(())
    }

    /// The selections in the canonical declaration order the product selection
    /// admission requires. Caller order carries no meaning.
    pub fn canonical_selections(&self) -> Vec<ProductSelection> {
        let mut selections = self.product_selections.clone();
        selections.sort_by(|left, right| left.declaration_id.cmp(&right.declaration_id));
        selections
    }
}

/// The program `compose --program` reads, with the identities a binding copies
/// from it and a digest of the resolved selections it was derived from.
#[derive(Debug, Clone, Serialize)]
pub struct ProductProgramResponse {
    pub program: AdmittedExternalCandidateProgram,
    pub selections_digest: String,
    pub runtime_manifest_hash: String,
    pub selection_identity_digest: String,
}

/// Admit the request's selections for its consumer and emit the program.
/// `select` performs the full product selection admission against the
/// consumer's resolved source (the API layer owns that resolution) and returns
/// what was selected without importing or binding anything.
pub async fn admit<F>(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ProductProgramRequest,
    select: F,
) -> anyhow::Result<ProductProgramResponse>
where
    F: FnOnce(
            &AppState,
            &HandlerContext,
            &ProductProgramRequest,
        ) -> anyhow::Result<ResolvedExternalProductSelections>
        + Send
        + 'static,
{
    crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    tokio::task::spawn_blocking(move || {
        let selections = select(&state, &context, &request)?;
        derive_program(&state, &request, &selections)
    })
    .await
    .context("product program admission task stopped")?
}

/// Derive the program from already admitted selections. Synchronous: it
/// resolves signed definitions, which starts handler processes.
pub(crate) fn derive_program(
    state: &AppState,
    request: &ProductProgramRequest,
    selections: &ResolvedExternalProductSelections,
) -> anyhow::Result<ProductProgramResponse> {
    require_unguarded_resolution("product program derivation")?;
    selections.validate()?;
    ensure!(
        selections.len() == request.product_selections.len()
            && request
                .product_selections
                .iter()
                .all(|selector| selections.get(&selector.declaration_id).is_some()),
        "admitted selections differ from the requested selections"
    );
    let worker = crate::source_closure_admission::prepare_bundle_structured_worker_profile(
        state,
        &request.consumer_ref,
    )?;
    let requirement = worker
        .admitted()
        .profile
        .external_candidate_requirement()?
        .context("consumer worker has no external candidate requirement")?;
    if requirement.runtime_authority != ExternalCandidateRuntimeAuthority::CapturedProduct {
        bail!("product program requires a captured-product worker");
    }
    let runtime = selections
        .get(&requirement.runtime_product_declaration_id)
        .context("the selections omit the worker's runtime product")?;
    let qualification = runtime
        .qualification
        .as_ref()
        .context("the selected runtime product has no admitted qualification")?;
    let policy_source = &qualification.evidence.policy_source;
    let signed_context = policy_source
        .policy
        .consumer_execution_context
        .as_ref()
        .context("the runtime qualification policy has no consumer execution context")?;
    ensure!(
        signed_context.worker_ref == request.consumer_ref
            && signed_context.product_declaration_id == requirement.runtime_product_declaration_id,
        "the runtime qualification policy names a different consumer"
    );
    // The Worker, environment and literals are rebuilt from current signed
    // source under the policy's context, exactly as the qualification use is
    // sealed. Dropping the staged content leaves node state untouched.
    let prepared = prepare_current_bundle_consumer_content_inputs(
        state,
        policy_source,
        &runtime.relationship,
        &runtime.relationship_ref,
    )?
    .context("the runtime qualification policy has no consumer execution context")?;
    ensure!(
        prepared
            .worker_source
            .profile
            .external_candidate_requirement()?
            .as_ref()
            == Some(&requirement),
        "consumer worker requirement changed during program derivation"
    );
    let slot = prepared
        .runtime_slot()
        .context("consumer worker declares no runtime product slot")?;
    ensure!(
        slot.id == requirement.runtime_product_declaration_id,
        "consumer runtime slot differs from its requirement"
    );
    let authority = state.state_store.pinned_state_authority()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let (_executable_hash, entry_count, total_bytes) = load_runtime_manifest_identity(
        &authority,
        limits,
        &runtime.manifest_hash,
        &requirement.runtime_recipe.executable_relative_path,
    )?;
    let qualification_use = AdmittedUseInputs {
        requirement: &requirement,
        profile: &prepared.worker_source.profile,
        source: &prepared.worker_source.source,
        worker_literals: &prepared.worker_literals,
        runtime: realize_runtime_slot(slot, &runtime.manifest_hash, entry_count, total_bytes),
        environment: &prepared.environment.realizations,
        executable_search: &prepared.environment.definition.executable_search,
        process_environment: &prepared.environment.definition.process_environment,
    }
    .derive()?;
    let program = requirement.resolve_for_use(Some(selections), &qualification_use)?;
    program.validate()?;
    let selections_digest =
        ryeos_state::objects::canonical_value_digest(&selections.semantic_identity_value()?)?;
    Ok(ProductProgramResponse {
        runtime_manifest_hash: program.runtime_manifest_hash.clone(),
        selection_identity_digest: program.selection_identity_digest.clone(),
        selections_digest,
        program,
    })
}

#[cfg(test)]
mod tests {
    use anyhow::{Result, ensure};
    use ryeos_state::external_content::products::transfer::ProductWitnessSource;

    use super::*;

    fn selection(id: &str) -> ProductSelection {
        ProductSelection {
            declaration_id: id.to_owned(),
            witness_hash: "a".repeat(64),
            witness_source: ProductWitnessSource::LocalCapture {},
            qualification_hash: None,
            remote_testimony: None,
        }
    }

    fn request() -> ProductProgramRequest {
        ProductProgramRequest {
            consumer_ref: "worker:codex/external-hosted-authoring".to_owned(),
            product_selections: vec![selection("guest-runtime")],
        }
    }

    #[test]
    fn a_canonical_worker_with_selections_is_admitted() -> Result<()> {
        request().validate()
    }

    #[test]
    fn unknown_fields_are_refused() -> Result<()> {
        let mut value = serde_json::to_value(request())?;
        value["policy_ref"] = serde_json::json!("config:caller/policy");
        ensure!(
            serde_json::from_value::<ProductProgramRequest>(value).is_err(),
            "a caller-authored field was accepted"
        );
        Ok(())
    }

    #[test]
    fn a_non_worker_or_inexact_consumer_is_refused() {
        for consumer in [
            "config:codex/external-hosted-authoring",
            "worker:codex/external-hosted-authoring@latest",
            "codex/external-hosted-authoring",
            "",
        ] {
            let mut bad = request();
            bad.consumer_ref = consumer.to_owned();
            assert!(bad.validate().is_err(), "{consumer} was admitted");
        }
    }

    #[test]
    fn empty_and_duplicate_selections_are_refused() {
        let mut empty = request();
        empty.product_selections.clear();
        assert!(empty.validate().is_err());
        let mut duplicate = request();
        duplicate.product_selections = vec![selection("guest-runtime"), selection("guest-runtime")];
        assert!(duplicate.validate().is_err());
    }

    #[test]
    fn selections_are_put_in_canonical_order() -> Result<()> {
        let mut unordered = request();
        unordered.product_selections = vec![selection("zeta"), selection("alpha")];
        unordered.validate()?;
        let ids: Vec<_> = unordered
            .canonical_selections()
            .into_iter()
            .map(|selection| selection.declaration_id)
            .collect();
        ensure!(ids == ["alpha", "zeta"], "selections were not ordered");
        Ok(())
    }
}
