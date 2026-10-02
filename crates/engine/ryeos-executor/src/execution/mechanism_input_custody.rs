//! Exact B product and source byte custody beside one original mechanism fixture.
//!
//! This is neither a project authority nor a native guest program. A later
//! owner must independently admit any protocol, guest input and execution.

use std::path::Path;

use anyhow::{Result, ensure};
use ryeos_app::operator_external_content::product_qualification::{
    CapturedMechanismProbeSource, CurrentBundleProducerRecipe, PreparedMechanismFixtureTransfer,
};
use ryeos_state::external_content::qualification_execution::QualificationExecutionPurposeView;

/// Keep materialization leases and descriptors before the fixture transfer and
/// its staged source/product/config publication owner in Rust field-drop order.
#[allow(dead_code)]
pub(crate) struct PreparedMechanismInputCustody {
    products: super::external_guest_inputs::PreparedProductInputs,
    source: Option<super::source_closure::ExternalSourceDelivery>,
    fixture: PreparedMechanismFixtureTransfer,
}

/// Redeem only the exact finalized B source/product bytes under the App's
/// original root, signed generation, attempt and deadline window. The opaque
/// fixture must already have been prepared by that same App owner.
#[allow(dead_code)]
pub(crate) fn prepare_mechanism_input_custody(
    state: &ryeos_app::state::AppState,
    captured: &CapturedMechanismProbeSource,
    purpose: &QualificationExecutionPurposeView<'_>,
    selected: &CurrentBundleProducerRecipe,
    fixture: PreparedMechanismFixtureTransfer,
) -> Result<PreparedMechanismInputCustody> {
    let (products, source) = captured.with_original_fixture_byte_custody(
        state,
        purpose,
        selected,
        &fixture,
        |resolution, admitted_source, _admitted_external, deadline| {
            ensure!(
                !deadline.has_elapsed(),
                "mechanism byte custody deadline elapsed before product materialization"
            );
            let products = super::external_guest_inputs::prepare_product_inputs(
                state,
                resolution,
                Path::new("/workspace"),
                None,
            )?;
            ensure!(
                !deadline.has_elapsed(),
                "mechanism byte custody deadline elapsed before source materialization"
            );
            let source = admitted_source
                .map(|source| {
                    super::source_closure::bind_external_admitted_source(state, resolution, source)
                })
                .transpose()?;
            Ok((products, source))
        },
    )?;
    Ok(PreparedMechanismInputCustody {
        products,
        source,
        fixture,
    })
}
