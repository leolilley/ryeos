//! Authenticated activated-content inputs for the shared qualification owner.
//!
//! This adapter owns source preparation and rechecks only. It does not create
//! a second verifier workflow, reserve launches, publish claims, or turn an
//! activation receipt into product authority.

use anyhow::Result;
use ryeos_state::external_content::{
    products::qualification::ProductQualificationPolicySource,
    qualification_allowance::ContentQualificationAllowance,
    qualification_subject::ContentQualificationSubject,
};
use ryeos_state::objects::ExternalContentRealization;

use crate::managed_external_content_operation::{
    AcquisitionMode, load_current_runtime_qualification_inputs,
    require_current_runtime_qualification_inputs,
};
use crate::{handler_context::HandlerContext, state::AppState};

/// Retained preparation coordinates, not an execution grant. The accepted
/// root must still pin recipes/parameters, match dispatch D1/D2, recheck this
/// source and seal the shared purpose before a callback can start execution.
pub struct PreparedContentQualificationSource {
    pub allowance: ContentQualificationAllowance,
    pub policy: ProductQualificationPolicySource,
    pub subject: ContentQualificationSubject,
    pub verifier_admitted_definition_digest: String,
    pub verifier_realized_definition_digest: String,
}

pub fn prepare_source(
    state: &AppState,
    context: &HandlerContext,
    activation_ref: &str,
    acquisition_mode: AcquisitionMode,
    admitted_realization: &ExternalContentRealization,
) -> Result<PreparedContentQualificationSource> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    let (allowance, policy, records) = load_current_runtime_qualification_inputs(
        state,
        activation_ref,
        acquisition_mode,
        admitted_realization,
    )?;
    let subject = records.retained_subject()?;
    let (d1, d2) = super::product_qualification::resolve_content_fixed_pin_verifier(
        state, context, &policy, &subject,
    )?;
    // Verifier resolution is a separate checked-generation operation. Rejoin
    // the complete signed selection/source after it rather than assuming the
    // first policy lookup stayed current across preparation.
    require_current_runtime_qualification_inputs(
        state,
        acquisition_mode,
        &allowance,
        &policy,
        &subject,
    )?;
    Ok(PreparedContentQualificationSource {
        allowance,
        policy,
        subject,
        verifier_admitted_definition_digest: d1,
        verifier_realized_definition_digest: d2,
    })
}
