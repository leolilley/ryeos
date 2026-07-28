//! Current-policy checks over immutable admitted program authority.
//!
//! Recovery never resolves the recorded names. It inspects only the signer,
//! source-space, and trust posture sealed with the exact program/plan, then
//! lets current node/project trust stores narrow that authority.

use anyhow::{Context, Result, anyhow, bail};
use ryeos_engine::contracts::{ItemSpace, PlanTrustAuthority};
use ryeos_engine::resolution::{
    AsLaunchedResolutionDigest, ResolutionDigestNode, ResolutionOutput, ResolvedAncestor,
    TrustClass,
};
use ryeos_engine::trust::TrustStore;
use ryeos_handler_protocol::{ItemSpaceWire, LaunchConfigContributorWire, TrustClassWire};

use super::launch_preparation::PreparedRuntimeLaunch;

struct CurrentTrust<'a> {
    node: &'a TrustStore,
    project: TrustStore,
}

impl<'a> CurrentTrust<'a> {
    fn load(
        engine: &'a ryeos_engine::engine::Engine,
        effective_project_root: Option<&std::path::Path>,
    ) -> Result<Self> {
        let project = engine
            .effective_request_snapshot(effective_project_root)
            .context("load current project trust policy for admitted execution")?
            .trust_store;
        Ok(Self {
            node: &engine.node_trust_store,
            project,
        })
    }

    fn validate(
        &self,
        label: &str,
        source_space: ItemSpace,
        trust_class: TrustClass,
        signer: Option<&str>,
    ) -> Result<()> {
        match (source_space, trust_class, signer) {
            (ItemSpace::Bundle, TrustClass::TrustedBundle, Some(signer)) => {
                validate_signer(label, signer)?;
                if !self.node.is_trusted(signer) {
                    bail!("{label} signer is no longer node-trusted: {signer}");
                }
            }
            (ItemSpace::Project, TrustClass::TrustedProject, Some(signer)) => {
                validate_signer(label, signer)?;
                if !self.project.is_trusted(signer) {
                    bail!("{label} signer is no longer project-trusted: {signer}");
                }
            }
            (
                ItemSpace::Bundle | ItemSpace::Project,
                TrustClass::UntrustedProject,
                Some(signer),
            ) => validate_signer(label, signer)?,
            (ItemSpace::Bundle | ItemSpace::Project, TrustClass::Unsigned, None) => {}
            (_, TrustClass::TrustedBundle | TrustClass::TrustedProject, None) => {
                bail!("{label} was admitted as trusted without a signer");
            }
            (_, TrustClass::UntrustedProject, None) => {
                bail!("{label} was admitted as signed-untrusted without a signer");
            }
            (_, TrustClass::Unsigned, Some(_)) => {
                bail!("{label} was admitted as unsigned but carries a signer");
            }
            (ItemSpace::Project, TrustClass::TrustedBundle, Some(_))
            | (ItemSpace::Bundle, TrustClass::TrustedProject, Some(_)) => {
                bail!("{label} trust class contradicts its admitted source space");
            }
        }
        Ok(())
    }
}

fn validate_signer(label: &str, signer: &str) -> Result<()> {
    if !lillux::valid_hash(signer) || signer.bytes().any(|byte| byte.is_ascii_uppercase()) {
        bail!("{label} carries a non-canonical signer fingerprint");
    }
    Ok(())
}

fn validate_digest_node(policy: &CurrentTrust<'_>, node: &ResolutionDigestNode) -> Result<()> {
    if !lillux::valid_hash(&node.raw_content_digest) {
        bail!(
            "admitted resolution authority `{}` carries an invalid content digest",
            node.resolved_ref
        );
    }
    policy.validate(
        &format!("admitted resolution authority `{}`", node.resolved_ref),
        node.source_space,
        node.trust_class,
        node.signer_fingerprint.as_deref(),
    )
}

fn validate_resolution_digest(
    policy: &CurrentTrust<'_>,
    resolution: &AsLaunchedResolutionDigest,
) -> Result<()> {
    for node in std::iter::once(&resolution.root)
        .chain(resolution.ancestors.iter())
        .chain(resolution.referenced_items.iter())
    {
        validate_digest_node(policy, node)?;
    }
    Ok(())
}

fn validate_resolution_node(policy: &CurrentTrust<'_>, node: &ResolvedAncestor) -> Result<()> {
    if !lillux::valid_hash(&node.raw_content_digest) {
        bail!(
            "admitted resolution authority `{}` carries an invalid content digest",
            node.resolved_ref
        );
    }
    policy.validate(
        &format!("admitted resolution authority `{}`", node.resolved_ref),
        node.source_space,
        node.trust_class,
        node.signer_fingerprint.as_deref(),
    )
}

fn validate_primary_resolution(
    policy: &CurrentTrust<'_>,
    resolution: &ResolutionOutput,
) -> Result<()> {
    for node in std::iter::once(&resolution.root)
        .chain(resolution.ancestors.iter())
        .chain(resolution.referenced_items.iter())
    {
        validate_resolution_node(policy, node)?;
    }
    Ok(())
}

fn config_space(space: ItemSpaceWire) -> ItemSpace {
    match space {
        ItemSpaceWire::Bundle => ItemSpace::Bundle,
        ItemSpaceWire::Project => ItemSpace::Project,
    }
}

fn config_trust(trust: TrustClassWire) -> TrustClass {
    match trust {
        TrustClassWire::TrustedBundle => TrustClass::TrustedBundle,
        TrustClassWire::TrustedProject => TrustClass::TrustedProject,
        TrustClassWire::UntrustedProject => TrustClass::UntrustedProject,
        TrustClassWire::Unsigned => TrustClass::Unsigned,
    }
}

fn validate_config_contributor(
    policy: &CurrentTrust<'_>,
    contributor: &LaunchConfigContributorWire,
) -> Result<()> {
    if !lillux::valid_hash(&contributor.content_digest) {
        bail!(
            "admitted launch config `{}` carries an invalid content digest",
            contributor.canonical_id
        );
    }
    policy.validate(
        &format!("admitted launch config `{}`", contributor.canonical_id),
        config_space(contributor.space),
        config_trust(contributor.trust_class),
        Some(&contributor.signer_fingerprint),
    )
}

pub(crate) fn validate_managed_current_trust(
    engine: &ryeos_engine::engine::Engine,
    effective_project_root: Option<&std::path::Path>,
    primary: &ResolutionOutput,
    prepared: &PreparedRuntimeLaunch,
) -> Result<()> {
    let policy = CurrentTrust::load(engine, effective_project_root)?;
    validate_primary_resolution(&policy, primary)?;
    for (name, binding) in &prepared.binding_records {
        validate_resolution_digest(&policy, &binding.resolution)
            .with_context(|| format!("validate admitted ref binding `{name}`"))?;
    }
    for contributor in &prepared.config_contributors {
        validate_config_contributor(&policy, contributor)?;
    }
    Ok(())
}

pub(crate) fn validate_direct_current_trust(
    engine: &ryeos_engine::engine::Engine,
    effective_project_root: Option<&std::path::Path>,
    primary: &ResolutionOutput,
    plan: &ryeos_engine::contracts::ExecutionPlan,
    capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
) -> Result<()> {
    let policy = CurrentTrust::load(engine, effective_project_root)?;
    validate_primary_resolution(&policy, primary)?;
    validate_direct_plan_closure(primary, plan, capsule)?;
    for authority in &plan.executor_authorities {
        validate_plan_authority(&policy, authority)?;
    }
    Ok(())
}

fn validate_direct_plan_closure(
    primary: &ResolutionOutput,
    plan: &ryeos_engine::contracts::ExecutionPlan,
    capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
) -> Result<()> {
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executor_item_content_hash,
        executor_item_signer_fingerprint,
        wrapper_source_identity,
        runtime_identity,
        ..
    } = &capsule.artifact_identity
    else {
        bail!("direct trust validation found a non-direct artifact identity");
    };
    if plan.root_ref != primary.root.resolved_ref
        || plan.item_kind
            != ryeos_engine::canonical_ref::CanonicalRef::parse(&primary.root.resolved_ref)
                .context("decode admitted direct root ref")?
                .kind
        || primary.root.source_content_digest != *executor_item_content_hash
        || primary.root.signer_fingerprint != *executor_item_signer_fingerprint
    {
        bail!("admitted direct plan/artifact identity contradicts its exact root resolution");
    }
    match (primary.root.source_space, wrapper_source_identity) {
        (ItemSpace::Project, ryeos_state::objects::DirectWrapperSourceIdentity::Project)
        | (ItemSpace::Bundle, ryeos_state::objects::DirectWrapperSourceIdentity::Bundle { .. }) => {
        }
        _ => bail!("admitted direct wrapper source contradicts its exact root resolution"),
    }
    if plan.executor_chain.first() != Some(&plan.root_ref)
        || plan.executor_chain.len() != plan.executor_authorities.len() + 1
        || plan
            .executor_authorities
            .iter()
            .zip(plan.executor_chain.iter().skip(1))
            .any(|(authority, chain_id)| authority.requested_id != *chain_id)
    {
        bail!("admitted direct executor authority list does not match its executor chain");
    }
    let first = plan
        .executor_authorities
        .first()
        .ok_or_else(|| anyhow!("admitted direct plan has no executor authority"))?;
    let runtime_space = match runtime_identity.runtime_source_space {
        ryeos_state::objects::DirectRuntimeSourceSpace::Project => ItemSpace::Project,
        ryeos_state::objects::DirectRuntimeSourceSpace::Bundle => ItemSpace::Bundle,
    };
    if first.canonical_ref != runtime_identity.runtime_ref
        || first.source_space != runtime_space
        || first.content_hash != runtime_identity.runtime_content_hash
        || first.signer_fingerprint.as_deref()
            != Some(runtime_identity.runtime_signer_fingerprint.as_str())
    {
        bail!("admitted direct runtime identity does not match its executor authority");
    }
    Ok(())
}

fn validate_plan_authority(
    policy: &CurrentTrust<'_>,
    authority: &PlanTrustAuthority,
) -> Result<()> {
    if authority.requested_id.trim().is_empty()
        || authority.canonical_ref.trim().is_empty()
        || !lillux::valid_hash(&authority.content_hash)
    {
        return Err(anyhow!(
            "admitted direct executor authority has invalid identity fields"
        ));
    }
    policy.validate(
        &format!(
            "admitted direct executor authority `{}`",
            authority.canonical_ref
        ),
        authority.source_space,
        authority.trust_class,
        authority.signer_fingerprint.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::trust::TrustedSigner;

    fn signer(seed: u8) -> (String, TrustedSigner) {
        let key = lillux::crypto::SigningKey::from_bytes(&[seed; 32]);
        let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
        (
            fingerprint.clone(),
            TrustedSigner {
                fingerprint,
                verifying_key: key.verifying_key(),
                label: None,
            },
        )
    }

    fn policy<'a>(node: &'a TrustStore, project: TrustStore) -> CurrentTrust<'a> {
        CurrentTrust { node, project }
    }

    fn digest_node(
        resolved_ref: &str,
        source_space: ItemSpace,
        trust_class: TrustClass,
        signer_fingerprint: Option<String>,
    ) -> ResolutionDigestNode {
        ResolutionDigestNode {
            requested_id: resolved_ref.to_string(),
            resolved_ref: resolved_ref.to_string(),
            source_space,
            trust_class,
            signer_fingerprint,
            raw_content_digest: "a".repeat(64),
        }
    }

    #[test]
    fn current_policy_revokes_trusted_project_signers() {
        let (fingerprint, admitted_signer) = signer(41);
        let node = TrustStore::empty();
        let admitted_project = TrustStore::from_signers(vec![admitted_signer]);
        policy(&node, admitted_project)
            .validate(
                "project root",
                ItemSpace::Project,
                TrustClass::TrustedProject,
                Some(&fingerprint),
            )
            .unwrap();

        let error = policy(&node, TrustStore::empty())
            .validate(
                "project root",
                ItemSpace::Project,
                TrustClass::TrustedProject,
                Some(&fingerprint),
            )
            .unwrap_err();
        assert!(error.to_string().contains("no longer project-trusted"));
    }

    #[test]
    fn signed_untrusted_authority_does_not_gain_or_require_current_trust() {
        let (fingerprint, _) = signer(42);
        let node = TrustStore::empty();
        policy(&node, TrustStore::empty())
            .validate(
                "signed project root",
                ItemSpace::Project,
                TrustClass::UntrustedProject,
                Some(&fingerprint),
            )
            .unwrap();
    }

    #[test]
    fn referenced_item_revocation_is_transitive() {
        let (root_fingerprint, root_signer) = signer(43);
        let (referenced_fingerprint, _) = signer(44);
        let node = TrustStore::from_signers(vec![root_signer]);
        let digest = AsLaunchedResolutionDigest {
            root: digest_node(
                "directive:test/root",
                ItemSpace::Bundle,
                TrustClass::TrustedBundle,
                Some(root_fingerprint),
            ),
            ancestors: Vec::new(),
            referenced_items: vec![digest_node(
                "config:test/reference",
                ItemSpace::Bundle,
                TrustClass::TrustedBundle,
                Some(referenced_fingerprint),
            )],
            effective_trust_class: TrustClass::TrustedBundle,
            policy_facts: Default::default(),
        };

        let error =
            validate_resolution_digest(&policy(&node, TrustStore::empty()), &digest).unwrap_err();
        assert!(error.to_string().contains("no longer node-trusted"));
        assert!(error.to_string().contains("config:test/reference"));
    }

    #[test]
    fn direct_executor_chain_authority_obeys_current_revocation() {
        let (fingerprint, _) = signer(45);
        let authority = PlanTrustAuthority {
            requested_id: "runtime:test/direct".to_string(),
            canonical_ref: "runtime:test/direct".to_string(),
            source_space: ItemSpace::Bundle,
            trust_class: TrustClass::TrustedBundle,
            signer_fingerprint: Some(fingerprint),
            content_hash: "b".repeat(64),
        };
        let node = TrustStore::empty();

        let error =
            validate_plan_authority(&policy(&node, TrustStore::empty()), &authority).unwrap_err();
        assert!(error.to_string().contains("no longer node-trusted"));
        assert!(error.to_string().contains("runtime:test/direct"));
    }
}
