//! Exact B product and source byte custody beside one original mechanism fixture.
//!
//! This retains compiled program data and byte custody. A later owner must
//! independently admit native execution; no project or launch grant is made.

use std::path::Path;

use anyhow::{Result, ensure};
use ryeos_app::operator_external_content::product_qualification::{
    CapturedMechanismProbeSource, CurrentBundleProducerRecipe, PreparedMechanismFixtureTransfer,
};
use ryeos_external_execution::guest_inputs::ExternalGuestInputAuthority;
use ryeos_external_execution_contract::{
    EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, ExternalGuestInputProjection, GuestBaseSnapshotInput,
};
use ryeos_state::external_content::qualification_execution::QualificationExecutionPurposeView;

/// Keep materialization leases and descriptors before the fixture transfer and
/// its staged source/product/config publication owner in Rust field-drop order.
#[allow(dead_code)]
pub(crate) struct PreparedMechanismInputCustody {
    program: ryeos_state::external_execution::admission::AdmittedExternalDirectProgram,
    authority: ExternalGuestInputAuthority,
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
    let (program, authority, (products, source)) = captured.with_original_fixture_program_data(
        state,
        purpose,
        selected,
        &fixture,
        |resolution,
         admitted_source,
         _admitted_external,
         measurement,
         base_descriptor,
         environment,
         executable_search,
         workspace_view,
         deadline| {
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
            let mut projection = ExternalGuestInputProjection {
                schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
                workspace_view,
                base_snapshot: GuestBaseSnapshotInput {
                    descriptor: base_descriptor
                        .inherited_descriptor()
                        .map_err(anyhow::Error::msg)?,
                    snapshot_hash: measurement.snapshot_hash.clone(),
                    closure_digest: measurement.closure_digest.clone(),
                    object_count: measurement.object_count,
                    blob_count: measurement.blob_count,
                    total_bytes: measurement.total_bytes,
                },
                workspace_outputs: None,
                inputs: products.inputs.clone(),
                executable_search: executable_search.to_vec(),
                environment: environment.clone(),
            };
            let mut descriptors = products.authorities.clone();
            let mut records = products.manifest_authorities.clone();
            let source_records = if let Some(source) = source.as_ref() {
                projection.inputs.push(source.input.clone());
                descriptors.push(source.directory.clone());
                records.extend(source.content_records.iter().cloned());
                Some(source.records.clone())
            } else {
                None
            };
            ensure!(
                !deadline.has_elapsed(),
                "mechanism byte custody deadline elapsed before input authority binding"
            );
            // The returned private wrapper keeps original leases and source
            // directory custody beside this descriptor authority. No fixture
            // or cache owner is erased into a Send+Sync trait object here.
            let authority = ExternalGuestInputAuthority::new(
                projection,
                base_descriptor.clone(),
                None,
                descriptors,
                records,
                Vec::new(),
            )?;
            Ok((authority, source_records, (products, source)))
        },
    )?;
    Ok(PreparedMechanismInputCustody {
        program,
        authority,
        products,
        source,
        fixture,
    })
}
