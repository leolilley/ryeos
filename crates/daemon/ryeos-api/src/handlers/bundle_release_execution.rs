//! Internal execution primitives shared by release and authority-calibration flows.
//!
//! These helpers retain the normal pinned-project admission, terminal product
//! acceptance, and qualification-publication gates.  Callers select a purpose-
//! owned Graph and recipes; they cannot inject accepted evidence directly.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use ryeos_app::{handler_context::HandlerContext, state::AppState};
use serde_json::Value;

/// An immutable release source tree backed by RyeOS's verified ProjectSnapshot
/// materialization authority. No VCS serialization or mutable pathname is an
/// identity boundary.
#[derive(Debug)]
pub(crate) struct ReleaseSourceGeneration {
    materialization: Arc<ryeos_state::PinnedProjectMaterialization>,
    project_identity: PathBuf,
    authoritative_root: Arc<tempfile::TempDir>,
}

impl ReleaseSourceGeneration {
    pub(crate) fn from_materialization(
        materialization: ryeos_state::PinnedProjectMaterialization,
        effective_root: &Path,
        project_identity: PathBuf,
        expected_snapshot_hash: &str,
    ) -> anyhow::Result<Self> {
        materialization.ensure_path_binding()?;
        anyhow::ensure!(
            materialization.snapshot_hash() == expected_snapshot_hash,
            "release source materialization names another RyeOS ProjectSnapshot"
        );
        anyhow::ensure!(
            materialization.owns_path(effective_root)?,
            "release source materialization does not own the effective project root"
        );
        let materialization = Arc::new(materialization);
        let authoritative_root = Arc::new(
            tempfile::Builder::new()
                .prefix("ryeos-release-source-")
                .tempdir()?,
        );
        copy_authoritative_tree(&materialization, authoritative_root.path())?;
        Ok(Self {
            materialization,
            project_identity,
            authoritative_root,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        self.authoritative_root.path()
    }

    pub(crate) fn project_identity(&self) -> &Path {
        &self.project_identity
    }

    pub(crate) fn project_content(
        &self,
    ) -> &dyn ryeos_engine::project_content::AuthoritativeProjectContent {
        self.materialization.as_ref()
    }

    pub(crate) fn authority(&self) -> RetainedReleaseSourceAuthority {
        RetainedReleaseSourceAuthority::new(
            Arc::clone(&self.materialization),
            self.project_identity.clone(),
            Arc::clone(&self.authoritative_root),
        )
    }

    pub(crate) fn signed_recipe(&self, relative: &str) -> anyhow::Result<(String, String)> {
        let bytes = self
            .materialization
            .authoritative_file_bounded(relative, 256 * 1024)?
            .context("fixed recipe is absent from immutable source generation")?;
        let signed = String::from_utf8(bytes).context("fixed release recipe is not UTF-8")?;
        let (body, signature) =
            lillux::signature::strip_canonical_signature_with_envelope(&signed, "#", None, false)?;
        anyhow::ensure!(signature.is_some(), "fixed release recipe is unsigned");
        Ok((signed, lillux::signature::content_hash(&body)))
    }

    fn materialize_with_recipe_overlay(
        &self,
        signed_build_recipe: &str,
        build_recipe_filename: &str,
        build_recipe_must_match_source: bool,
        capture_recipe_filename: &str,
        signed_capture_recipe: Option<&str>,
    ) -> anyhow::Result<tempfile::TempDir> {
        let workspace = tempfile::Builder::new()
            .prefix("ryeos-bundle-release-")
            .tempdir()?;
        copy_authoritative_tree(&self.materialization, workspace.path())?;
        let mut directory = workspace.path().to_path_buf();
        for component in [".ai", "config", "bundle-release"] {
            directory.push(component);
            match fs::symlink_metadata(&directory) {
                Ok(metadata) => anyhow::ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "release recipe overlay crosses an unsafe source entry"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&directory)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        for (name, contents) in [
            (build_recipe_filename, Some(signed_build_recipe)),
            (capture_recipe_filename, signed_capture_recipe),
        ] {
            let Some(contents) = contents else { continue };
            anyhow::ensure!(
                Path::new(name)
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))),
                "release recipe overlay filename is unsafe"
            );
            let recipe_path = directory.join(name);
            if build_recipe_must_match_source && name == build_recipe_filename {
                let archived =
                    lillux::read_regular_file_bounded_no_follow(&recipe_path, 256 * 1024)
                        .context("read fixed recipe from immutable source generation")?;
                anyhow::ensure!(
                    archived == contents.as_bytes(),
                    "fixed release recipe differs from the immutable source generation"
                );
            }
            if let Ok(metadata) = fs::symlink_metadata(&recipe_path) {
                anyhow::ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "release recipe overlay collides with a non-regular source entry"
                );
            }
            lillux::atomic_write_private(&recipe_path, contents.as_bytes())?;
        }
        Ok(workspace)
    }
}

#[derive(Debug)]
pub(crate) struct RetainedReleaseSourceAuthority {
    inner: ryeos_app::bundle_publication::admitted_build::PinnedProjectSourceSnapshotAuthority,
    authoritative_root: Arc<tempfile::TempDir>,
}

impl RetainedReleaseSourceAuthority {
    fn new(
        materialization: Arc<ryeos_state::PinnedProjectMaterialization>,
        project_identity: PathBuf,
        authoritative_root: Arc<tempfile::TempDir>,
    ) -> Self {
        Self {
            inner: ryeos_app::bundle_publication::admitted_build::PinnedProjectSourceSnapshotAuthority::new(
                materialization,
                project_identity,
            ),
            authoritative_root,
        }
    }
}

impl ryeos_app::bundle_publication::admitted_build::BundleSourceSnapshotAuthority
    for RetainedReleaseSourceAuthority
{
    fn authoritative_project_root(&self) -> &Path {
        self.authoritative_root.path()
    }

    fn verify_project_snapshot(
        &self,
        project_path: &Path,
        source_snapshot_hash: &str,
    ) -> anyhow::Result<()> {
        self.inner
            .verify_project_snapshot(project_path, source_snapshot_hash)
    }
}

fn copy_authoritative_tree(
    source: &ryeos_state::PinnedProjectMaterialization,
    destination: &Path,
) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    const MAX_ENTRIES: usize = 100_000;
    const MAX_FILE_BYTES: u64 = 268_435_456;
    const MAX_TOTAL_BYTES: u64 = 4_294_967_296;
    let mut total = 0_u64;
    for (relative, file) in source.authoritative_entries(MAX_ENTRIES)? {
        anyhow::ensure!(
            file.size <= MAX_FILE_BYTES,
            "release source member is oversized"
        );
        total = total
            .checked_add(file.size)
            .context("release source size overflow")?;
        anyhow::ensure!(
            total <= MAX_TOTAL_BYTES,
            "release source exceeds its size bound"
        );
        let bytes = source
            .authoritative_file_bounded(&relative, MAX_FILE_BYTES)?
            .context("authoritative release source file disappeared")?;
        let target = destination.join(&relative);
        let parent = target
            .parent()
            .context("release source file has no parent")?;
        fs::create_dir_all(parent)?;
        lillux::atomic_write_private(&target, &bytes)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(file.normalized_mode))?;
    }
    Ok(())
}

pub(crate) struct PinnedGraphExecution {
    pub graph_ref: &'static str,
    pub source_generation: Arc<ReleaseSourceGeneration>,
    pub signed_build_recipe: String,
    pub build_recipe_filename: &'static str,
    pub build_recipe_must_match_source: bool,
    pub capture_recipe_filename: &'static str,
    pub signed_capture_recipe: Option<String>,
    pub recipe_ref: &'static str,
    pub expected_recipe_raw_digest: String,
    pub parameters: Value,
    pub product_selections:
        Vec<ryeos_state::external_content::products::composition::ProductSelectionInput>,
    pub pre_minted_thread_id: Option<String>,
}

pub(crate) fn pinned_release_execution_policy() -> ryeos_app::execution_policy::ExecutionPolicy {
    let mut policy = ryeos_app::execution_policy::ExecutionPolicy::local_pinned_capture(
        ryeos_app::execution_policy::ExecutionResponse::Wait,
    );
    policy.ownership = ryeos_app::execution_policy::ExecutionOwnership::RequestScoped;
    policy.recovery = ryeos_app::execution_policy::ExecutionRecovery::None;
    policy.environment = ryeos_app::execution_policy::ExecutionEnvironmentPolicy::None;
    policy
}

/// Refuse a producer whose captured product would be unselectable by the
/// recipe's own relationship. This is an early consistency check, not recipe
/// signature verification or retained producer admission; both still run.
fn verify_recipe_producer_parameters(
    signed_recipe: &str,
    producer_ref: &str,
    parameters: &Value,
) -> anyhow::Result<bool> {
    let recipe: Value = serde_yaml::from_str(signed_recipe)?;
    let relationships: ryeos_state::external_content::products::composition::ProductRelationships =
        serde_json::from_value(
            recipe
                .get("product_relationships")
                .context("release recipe omits product relationships")?
                .clone(),
        )?;
    relationships.validate()?;
    let parameters_digest = ryeos_state::objects::canonical_value_digest(parameters)?;
    let mut matched = false;
    for relationship in &relationships.relationships {
        if relationship.producer.canonical_ref == producer_ref {
            matched = true;
            anyhow::ensure!(
                relationship.producer.admitted_parameters_digest()? == parameters_digest,
                "release recipe relationship `{}` does not bind the exact producer parameters for {producer_ref}; refusing production before capture/consumer handoff",
                relationship.name,
            );
        }
    }
    Ok(matched)
}

/// Release producer Graphs delegate one exact product-bearing action. Read
/// that action from the verified effective definition instead of inferring a
/// Tool name from the Graph ref or granting its children inherited authority.
fn selected_release_graph_child(graph: &Value) -> anyhow::Result<String> {
    let nodes = graph
        .pointer("/config/nodes")
        .and_then(Value::as_object)
        .context("release Graph has no nodes")?;
    let mut selected = None;
    for node in nodes.values() {
        let Some(action) = node.get("action") else {
            continue;
        };
        let selections = action
            .get("product_selections")
            .context("release Graph action omits child product selections")?;
        anyhow::ensure!(
            selections == "${inputs.child_product_selections}",
            "release Graph child does not bind its exact selected products"
        );
        let child = action
            .get("item_id")
            .and_then(Value::as_str)
            .context("release Graph selected child has no item ref")?;
        anyhow::ensure!(
            child.starts_with("tool:"),
            "release Graph selected child must be a Tool"
        );
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(child)?;
        anyhow::ensure!(
            canonical.to_string() == child && canonical.suffix.is_none(),
            "release Graph selected child must be an exact canonical Tool ref"
        );
        anyhow::ensure!(
            selected.replace(child.to_owned()).is_none(),
            "release Graph has more than one selected child"
        );
    }
    selected.context("release Graph has no selected child Tool")
}

async fn compose_pinned_release_consumer(
    state: Arc<AppState>,
    context: HandlerContext,
    consumer_ref: String,
    snapshot_hash: String,
    selections: Vec<ryeos_state::external_content::products::composition::ProductSelection>,
    maximum_bytes: u64,
) -> anyhow::Result<()> {
    let composition = ryeos_app::operator_external_content::product_composition::ComposeRetainedProductsRequest {
        consumer_ref,
        project_context: Some(
            ryeos_app::operator_external_content::product_composition::ProductCompositionProjectContext {
                snapshot_hash,
            },
        ),
        selections,
        maximum_bytes,
    };
    let preparation_state = Arc::clone(&state);
    let preparation_context = context.clone();
    let preparation_request = composition.clone();
    let (prepared, imported) = tokio::task::spawn_blocking(move || {
        let mut prepared =
            ryeos_executor::execution::project_source::prepare_external_product_consumer(
                &preparation_state,
                &preparation_request,
                &preparation_context,
                &format!("pinned-release-products-{}", uuid::Uuid::new_v4()),
            )?;
        let imported = prepared.select_and_import_products(
            preparation_state,
            preparation_context,
            &preparation_request,
        )?;
        anyhow::Ok((prepared, imported))
    })
    .await
    .context("pinned release product composition worker stopped")??;
    ryeos_app::operator_external_content::product_composition::compose_selected_products(
        state,
        context,
        composition,
        prepared.resolution(),
        imported,
    )
    .await?;
    Ok(())
}

pub(crate) async fn execute_pinned_graph(
    request: PinnedGraphExecution,
    context: HandlerContext,
    state: Arc<AppState>,
) -> anyhow::Result<Value> {
    anyhow::ensure!(
        context.verified,
        "bundle release execution requires an authenticated caller"
    );
    let PinnedGraphExecution {
        graph_ref,
        source_generation,
        signed_build_recipe,
        build_recipe_filename,
        build_recipe_must_match_source,
        capture_recipe_filename,
        signed_capture_recipe,
        recipe_ref,
        expected_recipe_raw_digest,
        mut parameters,
        product_selections,
        pre_minted_thread_id,
    } = request;
    let product_selections =
        ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(
            product_selections,
        )?;
    if graph_ref.starts_with("graph:") {
        let object = parameters
            .as_object_mut()
            .context("release Graph parameters must be an object")?;
        let exact_selections = serde_json::to_value(&product_selections)?;
        if let Some(supplied) = object.get("child_product_selections") {
            anyhow::ensure!(
                supplied == &exact_selections,
                "release Graph child selections differ from exact root selections"
            );
        } else {
            object.insert("child_product_selections".to_owned(), exact_selections);
        }
    }
    let mut producer_bound =
        verify_recipe_producer_parameters(&signed_build_recipe, graph_ref, &parameters)?;
    if let Some(recipe) = &signed_capture_recipe {
        producer_bound |= verify_recipe_producer_parameters(recipe, graph_ref, &parameters)?;
    }
    anyhow::ensure!(
        !graph_ref.starts_with("graph:") || producer_bound,
        "release producer has no exact recipe relationship"
    );
    let workspace = tokio::task::spawn_blocking(move || {
        source_generation.materialize_with_recipe_overlay(
            &signed_build_recipe,
            build_recipe_filename,
            build_recipe_must_match_source,
            capture_recipe_filename,
            signed_capture_recipe.as_deref(),
        )
    })
    .await
    .context("release execution project materializer stopped")??;

    let policy = pinned_release_execution_policy();
    policy.validate()?;
    let project_source =
        ryeos_executor::execution::project_source::ProjectSource::CaptureLiveFullProject;
    let mut project_ctx =
        crate::routes::response_modes::execute_mode::resolve_project_context_off_thread(
            crate::routes::response_modes::execute_mode::ResolveProjectContextRequest {
                state: state.as_ref().clone(),
                source: project_source.clone(),
                project_path: workspace.path().to_path_buf(),
                principal_id: context.fingerprint.clone(),
                checkout_id: format!("bundle-release-{}", uuid::Uuid::new_v4()),
                pinned_realization: Some(
                    ryeos_executor::execution::project_source::PinnedContextRealization::Cow,
                ),
                normalization: crate::routes::response_modes::execute_mode::ProjectRootNormalization::CanonicalizeLive,
                launch_timings: None,
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("capture release execution project: {error}"))?;
    let resolved = crate::routes::response_modes::execute_mode::resolve_execution_contract(
        &policy,
        &project_source,
        &project_ctx,
        None,
        None,
        &context.fingerprint,
        &context.scopes,
        &state,
    )?;
    let effective_path = project_ctx.effective_path.clone();
    let site_id = state.threads.site_id().to_owned();
    let plan_ctx = ryeos_engine::contracts::PlanContext {
        requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: context.fingerprint.clone(),
                scopes: context.scopes.clone(),
            },
        ),
        project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
            path: effective_path.clone(),
        },
        subject_resolution_authority: resolved.provenance.subject_resolution_authority(),
        current_site_id: site_id.clone(),
        origin_site_id: context.execution_origin(&site_id),
        execution_hints: Default::default(),
        scheduled_fire: None,
        validate_only: false,
    };
    let recipe_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(recipe_ref)?;
    let resolved_recipe = project_ctx.request_engine.resolve(&plan_ctx, &recipe_ref)?;
    anyhow::ensure!(
        resolved_recipe.raw_content_digest == expected_recipe_raw_digest,
        "admitted native build recipe differs from publisher-authorized bytes"
    );
    project_ctx
        .request_engine
        .verify(&plan_ctx, resolved_recipe)
        .context("publisher-authorized native build recipe is not trusted")?;
    if !product_selections.is_empty() {
        let child_consumer_ref = if graph_ref.starts_with("graph:") {
            let graph = project_ctx.request_engine.effective_item(
                ryeos_engine::engine::EffectiveItemRequest {
                    item_ref: ryeos_engine::canonical_ref::CanonicalRef::parse(graph_ref)?,
                    expected_kind: None,
                    project_root: Some(effective_path.clone()),
                    subject_resolution_authority: plan_ctx.subject_resolution_authority.clone(),
                },
            )?;
            anyhow::ensure!(graph.trusted, "selected release Graph is not trusted");
            Some(selected_release_graph_child(&graph.composed_value)?)
        } else {
            None
        };
        let mut selections = product_selections
            .iter()
            .map(|input| match &input.target {
                ryeos_state::external_content::products::composition::ProductSelectionTarget::Root {} => {
                    Ok(input.selection.clone())
                }
                _ => anyhow::bail!(
                    "pinned release execution only admits root product selections"
                ),
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        selections.sort_by(|left, right| left.declaration_id.cmp(&right.declaration_id));
        let snapshot_hash = project_ctx
            .snapshot_hash
            .clone()
            .context("pinned release product composition lost its captured snapshot")?;
        let maximum_bytes = state
            .node_policy
            .require::<
                ryeos_app::node_policy::sections::external_content::ExternalContentImportPolicyRecord,
            >()?
            .limits
            .max_total_bytes;
        // A Graph's selected realizations are not execution authority for its
        // child Tool. Bind the signed child independently against this same
        // recipe-overlay generation before either launch can use the bytes.
        if let Some(child) = child_consumer_ref {
            compose_pinned_release_consumer(
                Arc::clone(&state),
                context.clone(),
                child,
                snapshot_hash.clone(),
                selections.clone(),
                maximum_bytes,
            )
            .await?;
        }
        compose_pinned_release_consumer(
            Arc::clone(&state),
            context.clone(),
            graph_ref.to_owned(),
            snapshot_hash,
            selections,
            maximum_bytes,
        )
        .await?;
    }
    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: context.fingerprint.clone(),
        caller_scopes: context.scopes.clone(),
        engine: Arc::clone(&project_ctx.request_engine),
        plan_ctx,
        requested_call: None,
    };
    let acting_principal = context.fingerprint.clone();
    let dispatch_request = ryeos_executor::dispatch::DispatchRequest {
        launch_mode: "wait",
        target_site_id: None,
        validate_only: false,
        params: parameters,
        ref_bindings: Default::default(),
        product_selections,
        acting_principal: &acting_principal,
        project_path: &effective_path,
        provenance: resolved.provenance,
        lifecycle_authority: resolved.lifecycle_authority,
        launch_timings: None,
        original_root_kind: if graph_ref.starts_with("tool:") {
            "tool"
        } else {
            "graph"
        },
        pre_minted_thread_id,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        root_admission: None,
        root_dispatch_evidence: None,
        parent_execution_context: None,
        effect_authority: None,
    };
    let result = ryeos_executor::dispatch::dispatch_with_handler_context(
        graph_ref,
        context,
        &dispatch_request,
        &exec_ctx,
        &state,
    )
    .await
    .map_err(|error| anyhow::anyhow!("{graph_ref} dispatch failed: {error}"))
    .and_then(|envelope| {
        normalize_retained_dispatch_terminal(
            envelope,
            dispatch_request.pre_minted_thread_id.as_deref(),
            |thread_id| {
                let thread = state
                    .threads
                    .get_thread(thread_id)?
                    .context("release dispatch thread is absent from retained state")?;
                Ok(state
                    .threads
                    .build_execute_result(thread_id)?
                    .map(|terminal| (thread.status, terminal)))
            },
        )
    });
    // Captured roots outlive dispatch publication and no longer.
    drop(project_ctx.take_captured_generation());
    drop(workspace);
    result
}

/// Managed runtimes and direct Tools expose different live response payloads.
/// Only the exact dispatched thread's retained terminal is release authority;
/// never infer success from a runtime payload or recursively search its JSON.
fn normalize_retained_dispatch_terminal(
    mut envelope: Value,
    expected_thread_id: Option<&str>,
    load: impl FnOnce(
        &str,
    ) -> anyhow::Result<
        Option<(String, ryeos_app::thread_lifecycle::ExecuteResponseResult)>,
    >,
) -> anyhow::Result<Value> {
    let thread_id = envelope
        .pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .context("release dispatch omitted its exact thread identity")?;
    if let Some(expected) = expected_thread_id {
        anyhow::ensure!(
            thread_id == expected,
            "release dispatch returned another thread identity"
        );
    }
    let (status, terminal) = load(thread_id)?.with_context(|| {
        format!("release execution {thread_id} has no retained terminal result")
    })?;
    let status = ryeos_state::objects::ThreadStatus::from_str_lossy(&status)
        .filter(|status| {
            status.is_terminal() && *status != ryeos_state::objects::ThreadStatus::Continued
        })
        .with_context(|| format!("release execution {thread_id} has no final terminal status"))?;
    anyhow::ensure!(
        terminal.outcome_code.as_deref() != Some("success")
            || (status == ryeos_state::objects::ThreadStatus::Completed
                && terminal.error.as_ref().is_none_or(Value::is_null)),
        "release execution {thread_id} has contradictory retained success authority"
    );
    envelope["result"] = serde_json::to_value(terminal)?;
    Ok(envelope)
}

pub(crate) fn successful_dispatch_result(envelope: &Value) -> anyhow::Result<&Value> {
    let terminal = envelope
        .get("result")
        .context("release execution omitted its terminal result envelope")?;
    let outcome = terminal.get("outcome_code").and_then(Value::as_str);
    if outcome != Some("success") {
        let thread_id = envelope
            .pointer("/thread/thread_id")
            .and_then(Value::as_str)
            .unwrap_or("unreported");
        let diagnostic = terminal
            .get("error")
            .and_then(|error| error.as_str().or_else(|| error.get("message")?.as_str()))
            .or_else(|| terminal.pointer("/result/error").and_then(Value::as_str))
            .unwrap_or("no terminal diagnostic supplied");
        // Preserve the failing producer's explanation without serializing an
        // unbounded result, arbitrary artifacts, or the whole launch envelope.
        let diagnostic: String = diagnostic.chars().take(4096).collect();
        anyhow::bail!(
            "release execution {thread_id} did not complete successfully ({}): {diagnostic}",
            outcome.unwrap_or("unreported outcome")
        );
    }
    terminal
        .get("result")
        .filter(|value| !value.is_null())
        .context("release execution omitted its successful result")
}

pub(crate) fn accept_dispatch_products(
    envelope: &Value,
    state: &AppState,
) -> anyhow::Result<(
    String,
    ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult,
)> {
    // Product acceptance must still verify retained terminal authority. Check
    // the release-owned result first so failed producers keep their real error
    // rather than becoming a misleading missing-success-authority error.
    successful_dispatch_result(envelope)?;
    let thread_id = envelope
        .pointer("/thread/thread_id")
        .and_then(Value::as_str)
        .context("release producer omitted its terminal thread identity")?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let capsule = state
        .state_store
        .admitted_launch_capsule(thread_id)?
        .context("release producer has no admitted launch capsule")?;
    let accepted = ryeos_app::operator_external_content::product_build::accept_terminal(
        state, &capsule, thread_id, &guard,
    )?;
    ryeos_app::operator_external_content::product_build::verify_current(
        state,
        &guard,
        &accepted.to_value()?,
        &capsule,
    )?;
    let hash = authority.cas_store()?.store_object(&accepted.to_value()?)?;
    Ok((hash, accepted))
}

pub(crate) async fn publish_qualification(
    state: Arc<AppState>,
    context: HandlerContext,
    witness_hash: String,
    relationship_name: String,
    verifier_chain_root_id: String,
    verifier_thread_id: String,
) -> anyhow::Result<
    ryeos_app::operator_external_content::product_qualification::ProductQualificationResponse,
> {
    ryeos_app::operator_external_content::product_qualification::qualify(
        state,
        context,
        ryeos_app::operator_external_content::product_qualification::ProductQualificationRequest {
            witness_hash,
            witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
            relationship_name,
            verifier_chain_root_id,
            verifier_thread_id,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_retained_dispatch_terminal, selected_release_graph_child,
        successful_dispatch_result, verify_recipe_producer_parameters,
    };
    use serde_json::json;

    fn retained_terminal(
        outcome: &str,
        result: serde_json::Value,
        error: serde_json::Value,
    ) -> (String, ryeos_app::thread_lifecycle::ExecuteResponseResult) {
        (
            if outcome == "success" {
                "completed"
            } else {
                "failed"
            }
            .to_owned(),
            ryeos_app::thread_lifecycle::ExecuteResponseResult {
                outcome_code: Some(outcome.to_owned()),
                result: Some(result),
                error: Some(error),
                artifacts: Vec::new(),
            },
        )
    }

    #[test]
    fn managed_release_dispatch_uses_exact_retained_terminal_not_live_payload() {
        let live = json!({
            "thread":{"thread_id":"T-producer"},
            "result":{"success":true,"status":"completed","result":{"schema":"spoof"}}
        });
        let normalized =
            normalize_retained_dispatch_terminal(live.clone(), Some("T-producer"), |id| {
                assert_eq!(id, "T-producer");
                Ok(Some(retained_terminal(
                    "success",
                    json!({"schema":"retained"}),
                    json!(null),
                )))
            })
            .unwrap();
        assert_eq!(
            successful_dispatch_result(&normalized).unwrap()["schema"],
            "retained"
        );

        let failed = normalize_retained_dispatch_terminal(live, None, |_| {
            Ok(Some(retained_terminal(
                "failed",
                json!({"outcome_code":"success","result":{"schema":"spoof"}}),
                json!("spawn item: inspect source-backed sandbox mount target ENOENT"),
            )))
        })
        .unwrap();
        let diagnostic = successful_dispatch_result(&failed).unwrap_err().to_string();
        assert!(diagnostic.contains("T-producer"));
        assert!(diagnostic.contains("failed"));
        assert!(diagnostic.contains("inspect source-backed sandbox mount target ENOENT"));
        assert!(!diagnostic.contains("spoof"));
    }

    #[test]
    fn release_dispatch_requires_present_exact_retained_terminal() {
        let live = json!({"thread":{"thread_id":"T-producer"},"result":{"outcome_code":"success","result":{}}});
        assert!(normalize_retained_dispatch_terminal(live.clone(), None, |_| Ok(None)).is_err());
        assert!(
            normalize_retained_dispatch_terminal(live, Some("T-other"), |_| {
                panic!("mismatched dispatch must not read another terminal")
            })
            .is_err()
        );
        assert!(
            normalize_retained_dispatch_terminal(json!({"result":{}}), None, |_| {
                panic!("missing identity must not read terminal")
            })
            .is_err()
        );
    }

    #[test]
    fn retained_nested_failure_diagnostic_is_bounded_not_success_authority() {
        let normalized = normalize_retained_dispatch_terminal(
            json!({"thread":{"thread_id":"T-producer"},"result":{"success":true}}),
            None,
            |_| Ok(Some(retained_terminal("failed", json!({
                "error":"é".repeat(8192), "outcome_code":"success", "artifacts":["do-not-dump"]
            }), json!(null)))),
        ).unwrap();
        let diagnostic = successful_dispatch_result(&normalized)
            .unwrap_err()
            .to_string();
        assert_eq!(diagnostic.matches('é').count(), 4096);
        assert!(!diagnostic.contains("do-not-dump"));
    }

    #[test]
    fn release_dispatch_rejects_nonterminal_or_contradictory_retained_success() {
        for status in ["running", "continued", "failed", "unknown"] {
            let live = json!({"thread":{"thread_id":"T-producer"},"result":{"success":true}});
            assert!(
                normalize_retained_dispatch_terminal(live, None, |_| {
                    let (_, terminal) = retained_terminal("success", json!({}), json!(null));
                    Ok(Some((status.to_owned(), terminal)))
                })
                .is_err(),
                "{status}"
            );
        }
    }

    #[test]
    fn release_graphs_bind_one_exact_child_tool_selection() {
        let assets = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/bundle-release/.ai/graphs/ryeos/bundle-release");
        for name in [
            "portable-build",
            "portable-signed-capture",
            "native-build",
            "signed-capture",
            "core-seed-build",
            "core-seed-capture",
            "substrate-build",
        ] {
            let source = std::fs::read_to_string(assets.join(format!("{name}.yaml"))).unwrap();
            let graph: serde_json::Value = serde_yaml::from_str(&source).unwrap();
            assert_eq!(
                selected_release_graph_child(&graph).unwrap(),
                format!("tool:ryeos/bundle-release/{name}")
            );
        }
        assert!(selected_release_graph_child(&json!({"config":{"nodes":{}}})).is_err());
        assert!(
            selected_release_graph_child(&json!({"config":{"nodes":{"build":{"action":{
                "item_id":"tool:ryeos/bundle-release/portable-build",
                "product_selections":"${inputs.other}"
            }}}}}))
            .is_err()
        );
    }

    #[test]
    fn successful_result_rejects_failed_or_empty_terminal_envelopes() {
        assert!(successful_dispatch_result(&json!({"result":{"outcome_code":"failed"}})).is_err());
        assert!(
            successful_dispatch_result(&json!({"result":{"outcome_code":"success","result":null}}))
                .is_err()
        );
        assert_eq!(
            successful_dispatch_result(
                &json!({"result":{"outcome_code":"success","result":{"schema":"ok"}}})
            )
            .unwrap()["schema"],
            "ok"
        );
    }

    #[test]
    fn failed_release_result_preserves_producer_identity_and_error() {
        for error in [
            json!("realization command requires a child-owned declaration"),
            json!({"message": "realization command requires a child-owned declaration"}),
        ] {
            let envelope = json!({
                "thread": {"thread_id": "T-producer"},
                "result": {"outcome_code": "failed", "error": error, "result": null},
            });
            let message = successful_dispatch_result(&envelope)
                .unwrap_err()
                .to_string();
            assert!(message.contains("T-producer"));
            assert!(message.contains("failed"));
            assert!(message.contains("child-owned declaration"));
        }
    }

    #[test]
    fn failed_release_result_bounds_diagnostics_without_dumping_artifacts() {
        let envelope = json!({
            "result": {
                "outcome_code": "failed",
                "error": "é".repeat(8192),
                "artifacts": ["do-not-dump"],
            },
        });
        let message = successful_dispatch_result(&envelope)
            .unwrap_err()
            .to_string();
        assert_eq!(message.matches('é').count(), 4096);
        assert!(!message.contains("do-not-dump"));
    }

    #[test]
    fn release_recipe_preflight_rejects_unbound_calibration_parameters() {
        let assets = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/bundle-release/.ai/config/bundle-release");
        for (filename, producer) in [
            ("calibration-portable-build-products.yaml", "portable-build"),
            (
                "calibration-portable-capture-products.yaml",
                "portable-signed-capture",
            ),
            ("calibration-core-build-products.yaml", "core-seed-build"),
            (
                "calibration-core-capture-products.yaml",
                "core-seed-capture",
            ),
            (
                "calibration-substrate-build-products.yaml",
                "substrate-build",
            ),
        ] {
            let source = std::fs::read_to_string(assets.join(filename)).unwrap();
            let producer = format!("graph:ryeos/bundle-release/{producer}");
            let mut recipe: serde_json::Value = serde_yaml::from_str(&source).unwrap();
            // Exercise an exact request without requiring a production or a
            // signer: these fixtures are shape tests, not admitted evidence.
            let parameters = json!({"exact_request": "run-1"});
            for relationship in recipe["product_relationships"]["relationships"]
                .as_array_mut()
                .unwrap()
            {
                relationship["producer"]["parameters"] = parameters.clone();
            }
            let exact = serde_json::to_string(&recipe).unwrap();
            assert!(verify_recipe_producer_parameters(&exact, &producer, &parameters).unwrap());
            assert!(verify_recipe_producer_parameters(&exact, &producer, &json!({})).is_err());
            assert!(
                verify_recipe_producer_parameters(
                    &exact,
                    &producer,
                    &json!({"exact_request": "run-2"})
                )
                .is_err()
            );
            // A verifier using this recipe is not its producer. It has its
            // own independently checked parameters and qualification policy.
            assert!(
                !verify_recipe_producer_parameters(
                    &exact,
                    "tool:ryeos/bundle-release/portable-qualify",
                    &json!({}),
                )
                .unwrap()
            );
        }
    }
}
