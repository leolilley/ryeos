//! Wiring between API operations and executor-owned pinned snapshot context.

pub(crate) fn resolver(
    state: &ryeos_app::state::AppState,
) -> std::sync::Arc<
    dyn ryeos_app::operator_external_content::product_qualification::QualificationProjectContextResolver,
>{
    ryeos_executor::execution::project_source::qualification_project_context_resolver(state)
}
