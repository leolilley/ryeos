//! Enclosing program identity from already admitted prepared content.

use anyhow::{Context as _, bail};
use ryeos_engine::content_dependencies::{
    EffectiveContentDependencyIdentities, EffectiveContentDependencyIdentity,
    bind_effective_content_dependency_identities,
};
use ryeos_engine::resolution::ResolutionOutput;

use super::launch_preparation::PreparedRuntimeLaunch;

/// Invoke after persistent-session/content admission, before outer finalization.
/// This is also mandatory on recovery: a captured outer digest must agree with
/// the exact dependency resolutions recovered from its admitted capsule.
pub(crate) fn bind_prepared_content_identity(
    resolution: &mut ResolutionOutput,
    prepared: &PreparedRuntimeLaunch,
    recovered: bool,
) -> anyhow::Result<()> {
    let mut identities = EffectiveContentDependencyIdentities::new();
    for (binding, dependency) in &prepared.content_dependencies {
        dependency.validate()?;
        if binding != &dependency.binding {
            bail!("prepared content dependency map key changed");
        }
        let admitted = dependency.resolution.restore();
        let realized = admitted
            .composed
            .derived
            .get(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY)
            .context("content identity cannot be projected before realization admission")?;
        let realized =
            ryeos_engine::external_realization::RealizedExternalContentSet::from_value(realized)?;
        if realized.is_empty() {
            bail!("prepared content identity requires a nonempty admitted realization");
        }
        let contract = dependency.external_content_policy.declaration_contract();
        let retained_selections =
            ryeos_engine::external_content::resolved_external_product_selections(&admitted)?;
        match (
            dependency.product_selections.is_empty(),
            retained_selections.as_ref(),
        ) {
            (true, None) => {}
            (false, Some(retained))
                if retained.len() == dependency.product_selections.len()
                    && dependency.product_selections.iter().all(|selector| {
                        retained
                            .get(&selector.declaration_id)
                            .is_some_and(|selection| {
                                selection.witness_hash == selector.witness_hash
                                    && selection
                                        .qualification
                                        .as_ref()
                                        .map(|proof| proof.attestation_hash.as_str())
                                        == selector.qualification_hash.as_deref()
                            })
                    }) => {}
            _ => {
                bail!("prepared content identity product selectors contradict admitted selections")
            }
        }
        ryeos_engine::external_content::effective_external_content_declarations(
            &admitted,
            Some(&contract),
            ryeos_engine::external_content::declaring_authority(&admitted)?,
        )?
        .context("prepared content identity has no admitted declarations")?;
        identities.insert(
            binding.clone(),
            EffectiveContentDependencyIdentity {
                canonical_ref: dependency.canonical_ref.clone(),
                effective_definition_digest: admitted
                    .effective_definition_digest()?
                    .as_str()
                    .to_owned(),
                targets: dependency.targets.clone(),
                executable_search: dependency.executable_search.clone(),
            },
        );
    }
    bind_effective_content_dependency_identities(resolution, identities, recovered)
}
