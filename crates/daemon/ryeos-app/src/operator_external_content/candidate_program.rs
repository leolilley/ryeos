//! Read-only emission of the admitted external candidate program and the
//! runtime content qualification binding that selects its proof.
//!
//! A signed placement binding must carry a program that is bit-identical to the
//! one a launch later re-derives, and a selection of the published content
//! qualification that launch will re-authenticate. Both are derived here from
//! the same current published testimony and consumer inputs launch uses, and
//! emitted as data. Nothing is published or written: the staged consumer
//! inputs are dropped unpublished.

use std::sync::Arc;

use anyhow::{Context as _, bail, ensure};
use ryeos_state::external_content::qualification_evidence::ContentQualificationEvidence;
use ryeos_state::external_content::qualification_purpose::QualificationSubject;
use ryeos_state::external_execution::admission::{
    AdmittedExternalCandidateProgram, ExternalCandidateQualificationUse,
    ExternalCandidateRequirement, QUALIFICATION_CONTEXT_PARAMETER,
};
use serde::{Deserialize, Serialize};

use super::content_qualification::{load_current_qualified_content, prepare_source};
use super::product_qualification::prepare_activated_bundle_consumer_content_inputs;
use crate::handler_context::HandlerContext;
use crate::managed_external_content_operation::AcquisitionMode;
use crate::node_config::sections::external_execution::ExternalRuntimeContentQualificationBinding;
use crate::state::AppState;

/// Inputs name the exact published content qualification, the manifest of the
/// runtime it qualifies, and the claims the binding will require of it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateProgramRequest {
    pub activation_ref: String,
    pub coordinate_id: String,
    pub attestation_hash: String,
    pub runtime_manifest_hash: String,
    pub required_claims: Vec<String>,
}

impl CandidateProgramRequest {
    fn validate(&self) -> anyhow::Result<()> {
        if self.activation_ref.is_empty() {
            bail!("activation_ref must not be empty");
        }
        for (field, value) in [
            ("coordinate_id", &self.coordinate_id),
            ("attestation_hash", &self.attestation_hash),
            ("runtime_manifest_hash", &self.runtime_manifest_hash),
        ] {
            if !lillux::valid_hash(value) {
                bail!("{field} must be a 64-character lowercase hex digest");
            }
        }
        ensure!(
            !self.required_claims.is_empty()
                && self
                    .required_claims
                    .windows(2)
                    .all(|pair| pair[0] < pair[1]),
            "required_claims must be non-empty, sorted and unique"
        );
        for claim in &self.required_claims {
            ryeos_state::external_content::products::validate_name(claim)?;
        }
        Ok(())
    }
}

/// The emitted document: the program `compose` reads and the matching binding
/// selection of the content qualification.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateProgramResponse {
    pub program: AdmittedExternalCandidateProgram,
    pub runtime_content_qualification: serde_json::Value,
}

pub async fn admit(
    state: Arc<AppState>,
    context: HandlerContext,
    request: CandidateProgramRequest,
) -> anyhow::Result<CandidateProgramResponse> {
    let fingerprint = crate::operator_authority::require_admitted_operator(&state, &context)?;
    request.validate()?;
    let owner_principal = format!("fp:{fingerprint}");
    tokio::task::spawn_blocking(move || admit_blocking(&state, &context, &request, owner_principal))
        .await
        .context("candidate program admission task stopped")?
}

fn admit_blocking(
    state: &AppState,
    context: &HandlerContext,
    request: &CandidateProgramRequest,
    owner_principal: String,
) -> anyhow::Result<CandidateProgramResponse> {
    let published = load_current_qualified_content(
        state,
        context,
        &request.activation_ref,
        &request.coordinate_id,
        &request.attestation_hash,
        &request.runtime_manifest_hash,
    )?;
    ensure!(
        published.coordinate_id == request.coordinate_id
            && published.attestation_hash == request.attestation_hash,
        "published content qualification differs from the requested coordinate"
    );
    let QualificationSubject::ActivatedContent { content } = &published.evidence.purpose.subject
    else {
        bail!("content qualification has no acquired subject");
    };
    let source = prepare_source(
        state,
        context,
        &request.activation_ref,
        AcquisitionMode::Offline,
        &content.realization,
    )?;
    // The staged inputs are dropped unpublished: node state is untouched.
    let inputs = prepare_activated_bundle_consumer_content_inputs(
        state,
        context,
        &source,
        AcquisitionMode::Offline,
    )?;
    let requirement = inputs
        .worker_source
        .profile
        .external_candidate_requirement()?
        .context("activated consumer has no external candidate requirement")?;
    let qualification_use = ExternalCandidateQualificationUse::from_value(
        published
            .evidence
            .purpose
            .policy_source
            .policy
            .verifier_parameters
            .get(QUALIFICATION_CONTEXT_PARAMETER)
            .context("content qualification policy has no sealed use context")?,
    )?;
    assemble(
        &requirement,
        &qualification_use,
        &published.evidence,
        request,
        owner_principal,
    )
}

/// Derive the program and binding from already authenticated evidence. The
/// claim vocabulary is whatever the signed policy and the testimony carry;
/// nothing is named here.
fn assemble(
    requirement: &ExternalCandidateRequirement,
    qualification_use: &ExternalCandidateQualificationUse,
    evidence: &ContentQualificationEvidence,
    request: &CandidateProgramRequest,
    owner_principal: String,
) -> anyhow::Result<CandidateProgramResponse> {
    evidence.result.validate_claims_for(
        &evidence.purpose.policy_source.policy,
        &request.required_claims,
    )?;
    let program = requirement.resolve_for_content_evidence(
        &request.activation_ref,
        &request.coordinate_id,
        &request.attestation_hash,
        evidence,
        qualification_use,
    )?;
    program.validate()?;
    ensure!(
        program.runtime_manifest_hash == request.runtime_manifest_hash,
        "admitted program runtime differs from the requested runtime manifest"
    );
    let binding = ExternalRuntimeContentQualificationBinding {
        activation_ref: request.activation_ref.clone(),
        coordinate_id: request.coordinate_id.clone(),
        attestation_hash: request.attestation_hash.clone(),
        owner_principal,
        required_claims: request.required_claims.clone(),
    };
    Ok(CandidateProgramResponse {
        program,
        runtime_content_qualification: serde_json::to_value(&binding)?,
    })
}

#[cfg(test)]
mod tests {
    use anyhow::{Result, ensure};
    use ryeos_state::external_execution::admission::test_support;
    use ryeos_state::objects::RetainedExternalRuntimeContentQualification;

    use super::*;

    struct Fixture {
        requirement: ExternalCandidateRequirement,
        qualification_use: ExternalCandidateQualificationUse,
        retained: RetainedExternalRuntimeContentQualification,
        program: AdmittedExternalCandidateProgram,
    }

    fn fixture() -> Result<Fixture> {
        let capsule = test_support::content_candidate_capsule(&"a".repeat(64))?;
        let program = capsule
            .external_candidate
            .context("fixture has no admitted program")?;
        let retained = capsule
            .retained_external_runtime_content_qualification
            .context("fixture has no retained content qualification")?;
        Ok(Fixture {
            requirement: program.requirement.clone(),
            qualification_use: program.qualification_use.clone(),
            retained,
            program,
        })
    }

    fn request(fixture: &Fixture, claims: Vec<String>) -> CandidateProgramRequest {
        CandidateProgramRequest {
            activation_ref: fixture.retained.activation_ref.clone(),
            coordinate_id: fixture.retained.coordinate_id.clone(),
            attestation_hash: fixture.retained.attestation_hash.clone(),
            runtime_manifest_hash: fixture.retained.runtime_manifest_hash.clone(),
            required_claims: claims,
        }
    }

    fn owner() -> String {
        format!("fp:{}", "9".repeat(64))
    }

    #[test]
    fn emitted_program_equals_the_launch_derivation_for_the_same_evidence() -> Result<()> {
        let fixture = fixture()?;
        let claims = fixture.retained.evidence.result.claims.clone();
        let response = assemble(
            &fixture.requirement,
            &fixture.qualification_use,
            &fixture.retained.evidence,
            &request(&fixture, claims.clone()),
            owner(),
        )?;
        let launch = fixture
            .requirement
            .resolve_for_content(&fixture.retained, &fixture.qualification_use)?;
        ensure!(
            response.program == launch && response.program == fixture.program,
            "emitted program differs from the launch derivation"
        );
        let binding = &response.runtime_content_qualification;
        ensure!(
            binding["activation_ref"] == fixture.retained.activation_ref.as_str()
                && binding["coordinate_id"] == fixture.retained.coordinate_id.as_str()
                && binding["attestation_hash"] == fixture.retained.attestation_hash.as_str()
                && binding["owner_principal"] == owner().as_str()
                && binding["required_claims"] == serde_json::json!(claims),
            "binding does not carry the published coordinate and requested claims"
        );
        Ok(())
    }

    #[test]
    fn claim_absent_from_the_evidence_is_refused() -> Result<()> {
        let fixture = fixture()?;
        let absent = "claim_the_testimony_does_not_carry".to_owned();
        ensure!(
            !fixture.retained.evidence.result.claims.contains(&absent),
            "fixture unexpectedly carries the absent claim"
        );
        ensure!(
            assemble(
                &fixture.requirement,
                &fixture.qualification_use,
                &fixture.retained.evidence,
                &request(&fixture, vec![absent]),
                owner(),
            )
            .is_err(),
            "a claim absent from the evidence was admitted"
        );
        Ok(())
    }

    #[test]
    fn coordinate_that_is_not_the_testimony_is_refused() -> Result<()> {
        let fixture = fixture()?;
        let mut other = request(&fixture, fixture.retained.evidence.result.claims.clone());
        other.coordinate_id = "8".repeat(64);
        ensure!(
            assemble(
                &fixture.requirement,
                &fixture.qualification_use,
                &fixture.retained.evidence,
                &other,
                owner(),
            )
            .is_err(),
            "a coordinate that differs from the testimony was admitted"
        );
        Ok(())
    }

    #[test]
    fn manifest_that_is_not_the_published_runtime_is_refused() -> Result<()> {
        let fixture = fixture()?;
        let mut other = request(&fixture, fixture.retained.evidence.result.claims.clone());
        other.runtime_manifest_hash = "8".repeat(64);
        ensure!(
            assemble(
                &fixture.requirement,
                &fixture.qualification_use,
                &fixture.retained.evidence,
                &other,
                owner(),
            )
            .is_err(),
            "a manifest that differs from the published runtime was admitted"
        );
        Ok(())
    }

    #[test]
    fn request_refuses_malformed_fields() -> Result<()> {
        let fixture = fixture()?;
        let good = request(&fixture, vec!["alpha".into(), "beta".into()]);
        good.validate()?;
        for mutate in [
            (|r: &mut CandidateProgramRequest| r.coordinate_id = "short".into())
                as fn(&mut CandidateProgramRequest),
            |r| r.attestation_hash = "short".into(),
            |r| r.runtime_manifest_hash = "short".into(),
            |r| r.activation_ref.clear(),
            |r| r.required_claims.clear(),
            |r| r.required_claims = vec!["beta".into(), "alpha".into()],
            |r| r.required_claims = vec!["alpha".into(), "alpha".into()],
        ] {
            let mut bad = good.clone();
            mutate(&mut bad);
            ensure!(bad.validate().is_err(), "a malformed request was admitted");
        }
        Ok(())
    }

    #[tokio::test]
    async fn non_operator_is_refused() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (_state_owner, state) = crate::state::test_support::build(root.path())?;
        let fixture = fixture()?;
        let context = HandlerContext::new(format!("fp:{}", "9".repeat(64)), vec![], false);
        let refusal = admit(
            Arc::new(state),
            context,
            request(&fixture, fixture.retained.evidence.result.claims.clone()),
        )
        .await
        .err()
        .context("a non-operator was admitted")?;
        ensure!(!refusal.to_string().is_empty(), "refusal carries no reason");
        Ok(())
    }

    #[tokio::test]
    async fn unpublished_qualification_is_refused() -> Result<()> {
        let root = tempfile::tempdir()?;
        let (_state_owner, state) = crate::state::test_support::build(root.path())?;
        let fixture = fixture()?;
        let context = HandlerContext::new(format!("fp:{}", "9".repeat(64)), vec![], true);
        ensure!(
            admit_blocking(
                &state,
                &context,
                &request(&fixture, fixture.retained.evidence.result.claims.clone()),
                owner(),
            )
            .is_err(),
            "an unpublished or unauthenticated qualification was admitted"
        );
        Ok(())
    }
}
