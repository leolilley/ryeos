//! Current trust checks over retained source authority; never resolves names.
use anyhow::{Result, anyhow, bail};
use ryeos_engine::contracts::ItemSpace;
use ryeos_engine::resolution::TrustClass;
use ryeos_engine::trust::TrustStore;

/// Verify exact retained signed bytes against their captured identity and the
/// current trust store. The returned body is not launch or cleanup permission.
/// Callers retain their own descriptor decoding, admission and error stages.
pub fn verify_admitted_signed_descriptor_document(
    document: &str,
    expected_content_hash: &str,
    expected_signer: &str,
    trust_store: &TrustStore,
) -> Result<String> {
    let header =
        lillux::signature::parse_signature_line(document.lines().next().unwrap_or(""), "#", None)
            .ok_or_else(|| anyhow!("admitted descriptor has no valid signature header"))?;
    let body = lillux::signature::strip_signature_lines(document);
    let observed_hash = lillux::signature::content_hash(&body);
    if observed_hash != expected_content_hash
        || header.content_hash != expected_content_hash
        || header.signer_fingerprint != expected_signer
    {
        bail!("admitted descriptor document contradicts its sealed identity");
    }
    let signer = trust_store.get(expected_signer).ok_or_else(|| {
        anyhow!("admitted descriptor signer is no longer trusted: {expected_signer}")
    })?;
    if !lillux::signature::verify_signature(
        expected_content_hash,
        &header.signature_b64,
        &signer.verifying_key,
    ) {
        bail!("admitted descriptor signature no longer verifies");
    }
    Ok(body)
}

pub struct CurrentTrust<'a> {
    pub node: &'a TrustStore,
    pub project: &'a TrustStore,
}

impl<'a> CurrentTrust<'a> {
    pub fn from_current_policy(
        engine: &'a ryeos_engine::engine::Engine,
        project_trust: &'a TrustStore,
    ) -> Self {
        Self {
            node: &engine.node_trust_store,
            project: project_trust,
        }
    }

    pub fn validate(
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
            (ItemSpace::Node, TrustClass::TrustedNode, Some(signer)) => {
                validate_signer(label, signer)?;
                if !self.node.is_trusted(signer) {
                    bail!("{label} signer is no longer node-trusted: {signer}");
                }
            }
            (
                ItemSpace::Bundle | ItemSpace::Project | ItemSpace::Node,
                TrustClass::UntrustedProject,
                Some(signer),
            ) => validate_signer(label, signer)?,
            (
                ItemSpace::Bundle | ItemSpace::Project | ItemSpace::Node,
                TrustClass::Unsigned,
                None,
            ) => {}
            (
                _,
                TrustClass::TrustedBundle | TrustClass::TrustedProject | TrustClass::TrustedNode,
                None,
            ) => {
                bail!("{label} was admitted as trusted without a signer");
            }
            (_, TrustClass::UntrustedProject, None) => {
                bail!("{label} was admitted as signed-untrusted without a signer");
            }
            (_, TrustClass::Unsigned, Some(_)) => {
                bail!("{label} was admitted as unsigned but carries a signer");
            }
            (ItemSpace::Project, TrustClass::TrustedBundle, Some(_))
            | (ItemSpace::Project, TrustClass::TrustedNode, Some(_))
            | (ItemSpace::Bundle, TrustClass::TrustedProject, Some(_))
            | (ItemSpace::Bundle, TrustClass::TrustedNode, Some(_))
            | (ItemSpace::Node, TrustClass::TrustedBundle, Some(_))
            | (ItemSpace::Node, TrustClass::TrustedProject, Some(_)) => {
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

pub fn validate_hook_plan_current_trust(
    engine: &ryeos_engine::engine::Engine,
    project_trust: &TrustStore,
    plan: &ryeos_engine::hooks::EffectiveHookPlan,
) -> Result<()> {
    let policy = CurrentTrust::from_current_policy(engine, project_trust);
    validate_hook_plan_trust(&policy, plan)
}

pub fn validate_hook_plan_trust(
    policy: &CurrentTrust<'_>,
    plan: &ryeos_engine::hooks::EffectiveHookPlan,
) -> Result<()> {
    plan.validate().map_err(|error| anyhow!(error))?;
    for source in &plan.sources {
        if !lillux::valid_hash(&source.source_raw_content_digest)
            || source
                .source_raw_content_digest
                .bytes()
                .any(|byte| byte.is_ascii_uppercase())
        {
            bail!(
                "admitted hook source `{}` carries an invalid raw-content digest",
                source.canonical_ref
            );
        }
        policy.validate(
            &format!("admitted hook source `{}`", source.canonical_ref),
            source.source_space,
            source.trust_class,
            Some(&source.signer_fingerprint),
        )?;
    }
    Ok(())
}
