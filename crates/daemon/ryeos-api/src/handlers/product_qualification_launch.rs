//! Accepted launch of the exact signed product verifier.
//!
//! The caller owns only the launch coordinate and product relationship. The
//! daemon resolves the verifier, parameters, subject selection, and purpose
//! from current trusted Bundle authority after reserving the launch ID.

use std::collections::BTreeMap;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::operator_external_content::qualification_launch::{
    PreparedQualificationLaunch, QualificationLaunchSourceRequest,
};
use ryeos_app::state::AppState;
use ryeos_executor::execution::project_source::{ProjectSource, ProjectSourceError};
use ryeos_executor::executor::ServiceAvailability;
use ryeos_runtime::authorizer::AuthorizationPolicy;
use ryeos_state::external_content::qualification_purpose::{
    QUALIFICATION_LAUNCH_PURPOSE_SCHEMA, QualificationLaunchPurpose, QualificationSubject,
};
use serde_json::{Value, json};

use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use crate::routes::launch::{AcceptedLaunchAdmissionGuard, DispatchLaunchOptions};
use crate::routes::parsed_ref::ParsedItemRef;
use crate::routes::response_modes::execute_mode::{
    ProjectRootNormalization, ResolveProjectContextRequest, create_isolated_no_project_workspace,
    resolve_project_context_off_thread,
};

pub type Request =
    ryeos_app::operator_external_content::product_qualification::launch::ProductQualificationLaunchRequest;

const REQUIRED_CAP: &str = "ryeos.execute.service.external-content/launch-product-qualification";
const CONTENT_REQUIRED_CAP: &str =
    "ryeos.execute.service.external-content/launch-content-qualification";

fn map_reservation_error(
    error: ryeos_app::state_store::LaunchPlanningReservationError,
) -> HandlerError {
    match error {
        ryeos_app::state_store::LaunchPlanningReservationError::AlreadyReserved(_) => {
            HandlerError::Conflict(
                "launch_id is unavailable; query its exact owner-bound status before any new request"
                    .to_string(),
            )
        }
        ryeos_app::state_store::LaunchPlanningReservationError::CapacityExceeded(_) => {
            HandlerError::Structured {
                code: "launch_planning_capacity_exceeded".to_string(),
                status: 503,
                body: json!({
                    "code":"launch_planning_capacity_exceeded",
                    "error":"pending launch admission reached node capacity",
                }),
            }
        }
        ryeos_app::state_store::LaunchPlanningReservationError::Internal(error) => {
            HandlerError::Internal(format!("reserve qualification launch: {error:#}"))
        }
    }
}

/// A missing generation-scoped content binding is operator-correctable and
/// keeps its typed 409; every other preparation failure stays a 400.
fn refused(stage: &str, error: anyhow::Error) -> HandlerError {
    if let Some(missing) =
        error.downcast_ref::<ryeos_app::external_content_admission::ContentBindingMissing>()
    {
        return missing.handler_error();
    }
    HandlerError::BadRequest(format!("{stage}: {error:#}"))
}

fn dispatch_error(error: ryeos_executor::dispatch_error::DispatchError) -> HandlerError {
    HandlerError::Structured {
        code: error.code().to_string(),
        status: error.http_status().as_u16(),
        body: json!({ "code":error.code(), "error":error.to_string() }),
    }
}

fn launch_error(error: crate::routes::launch::LaunchSpawnError) -> HandlerError {
    HandlerError::Structured {
        code: error.code().to_string(),
        status: error.http_status().as_u16(),
        body: json!({ "code":error.code(), "error":error.to_string() }),
    }
}

/// An uncertain handoff may still have a dispatch task borrowing the staged
/// consumer objects. The stage must outlive that task, not merely the API
/// response. The task's accepted root owns the durable closure if admitted.
fn retain_uncertain_consumer_stage_until_task_terminal(
    task: tokio::task::JoinHandle<Result<(), crate::routes::launch::LaunchSpawnError>>,
    workspace_guard: Arc<ryeos_app::temp_dir_guard::TempDirGuard>,
    thread_id: String,
    publication: Option<ryeos_state::PendingCasPublication>,
) {
    let keeper = crate::routes::launch::retain_launch_workspace_until_task_terminal(
        task,
        Some(workspace_guard),
        thread_id,
    );
    tokio::spawn(async move {
        let _publication = publication;
        let _ = keeper.await;
    });
}

fn require_exact_admitted_fixed_pin(
    engine: &ryeos_engine::engine::Engine,
    admission: &ryeos_app::thread_lifecycle::RootExecutionAdmission,
    declaration_id: &str,
    manifest_hash: &str,
) -> Result<(), HandlerError> {
    let resolution = admission.resolution_output();
    let kind = admission
        .verified_subject()
        .resolved
        .canonical_ref
        .kind
        .as_str();
    let contract = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.external_content_contract());
    let declarer = ryeos_engine::external_content::declaring_authority(resolution)
        .map_err(|error| HandlerError::BadRequest(format!("verifier declarer: {error:#}")))?;
    let declarations = ryeos_engine::external_content::effective_external_content_declarations(
        resolution, contract, declarer,
    )
    .map_err(|error| HandlerError::BadRequest(format!("verifier external content: {error:#}")))?
    .ok_or_else(|| HandlerError::BadRequest("verifier admitted no fixed pin".to_string()))?;
    let mut matches = declarations
        .iter()
        .filter(|declaration| declaration.id == declaration_id);
    let subject = matches.next().ok_or_else(|| {
        HandlerError::BadRequest("verifier admitted no policy subject pin".to_string())
    })?;
    if matches.next().is_some()
        || subject.mode != ryeos_engine::external_content::ExternalContentMode::Pinned
        || subject.locator.is_some()
        || subject.digest.as_deref() != Some(manifest_hash)
    {
        return Err(HandlerError::BadRequest(
            "verifier's admitted fixed pin differs from the selected product witness".to_string(),
        ));
    }
    Ok(())
}

/// The project identity a pinned qualification may state, and the only way
/// it is ever stated. The caller must be the node's admitted operator
/// (checked first), and the caller's own head for the name the snapshot
/// declares must be exactly `head_snapshot_hash`. A path the caller names is
/// a label only and never participates. Every refusal is typed.
pub fn state_pinned_qualification_logical_project(
    state: &AppState,
    ctx: &HandlerContext,
    head_snapshot_hash: &str,
) -> Result<ryeos_state::project_identity::ProjectIdentity, HandlerError> {
    ryeos_app::operator_authority::require_admitted_operator(state, ctx).map_err(|error| {
        HandlerError::Forbidden(format!(
            "logical_project_requires_operator: a pinned qualification states a logical project only for the node's operator: {error:#}"
        ))
    })?;
    ryeos_executor::execution::project_source::current_head_logical_project(
        state,
        &ctx.fingerprint,
        head_snapshot_hash,
    )
    .map_err(|error| match error {
        ProjectSourceError::SnapshotNotCurrentHead { .. } => {
            HandlerError::Conflict(format!("snapshot_not_current_head: {error}"))
        }
        other => {
            HandlerError::BadRequest(format!("qualification_project_unresolvable: {other}"))
        }
    })
}

/// Internal release-stage source of a stated logical project. The release
/// graph holds the release project identity and the release source
/// generation's snapshot; it never travels in the public JSON request, so a
/// caller cannot choose it. The same gate runs on it under the launching
/// principal.
#[derive(Debug, Clone)]
pub(crate) struct ReleaseHeadSource {
    pub project_path: PathBuf,
    pub head_snapshot_hash: String,
}

/// Reserve before any witness/Bundle resolution. Once reserved, every exit
/// settles it; after handoff, the launch task owns settlement. A repeated ID
/// is never treated as permission to launch again after an ambiguous ACK.
pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    handle_expected_verifier(req, ctx, state, None, None, None).await
}

/// A release stage may assert the checker it intends to use. This assertion
/// only narrows the signed policy's selection; it cannot choose a different
/// verifier, parameters, purpose or execution route.
pub(super) async fn handle_expected_verifier(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
    expected_verifier_ref: Option<&str>,
    expected_product_selections: Option<
        &ryeos_state::external_content::products::composition::ProductSelectionInputs,
    >,
    release_source: Option<ReleaseHeadSource>,
) -> Result<Value, HandlerError> {
    handle_source(
        QualificationLaunchSourceRequest::CapturedProduct(req),
        ctx,
        state,
        expected_verifier_ref,
        expected_product_selections,
        release_source,
    )
    .await
}

async fn handle_source(
    req: QualificationLaunchSourceRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
    expected_verifier_ref: Option<&str>,
    expected_product_selections: Option<
        &ryeos_state::external_content::products::composition::ProductSelectionInputs,
    >,
    release_source: Option<ReleaseHeadSource>,
) -> Result<Value, HandlerError> {
    let required_cap = match &req {
        QualificationLaunchSourceRequest::CapturedProduct(_) => REQUIRED_CAP,
        QualificationLaunchSourceRequest::ActivatedContent(_) => CONTENT_REQUIRED_CAP,
    };
    req.validate()
        .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?;
    ryeos_app::operator_authority::require_admitted_operator(&state, &ctx)
        .map_err(|error| HandlerError::Forbidden(format!("{error:#}")))?;
    state
        .authorizer
        .authorize(&ctx.scopes, &AuthorizationPolicy::require(required_cap))
        .map_err(|_| {
            HandlerError::Forbidden(format!("missing required capability: {required_cap}"))
        })?;

    // The stated logical project, gated before anything is reserved. A release
    // stage supplies its own head source; a public request supplies a path
    // only through its project context. Both run the one gate under this
    // caller.
    let request_project = match &req {
        QualificationLaunchSourceRequest::CapturedProduct(request) => request
            .project_context
            .as_ref()
            .and_then(|context| Some((context.project_path.as_deref()?, &context.snapshot_hash))),
        _ => None,
    };
    let stated_project = match (&release_source, request_project) {
        (Some(_), Some(_)) => {
            return Err(HandlerError::BadRequest(
                "a release qualification takes its logical project from the release source only"
                    .to_string(),
            ));
        }
        (Some(release), None) => Some(state_pinned_qualification_logical_project(
            &state,
            &ctx,
            &release.head_snapshot_hash,
        )?),
        (None, Some((_label, snapshot_hash))) => Some(
            state_pinned_qualification_logical_project(&state, &ctx, snapshot_hash)?,
        ),
        (None, None) => None,
    };

    let mut reservation =
        AcceptedLaunchAdmissionGuard::reserve(&state, req.launch_id(), &ctx.fingerprint)
            .map_err(map_reservation_error)?;

    let mut prepared = {
        let state = Arc::clone(&state);
        let context = ctx.clone();
        let request = req.clone();
        tokio::task::spawn_blocking(move || request.prepare_after_reservation(&state, &context))
            .await
            .map_err(|error| {
                HandlerError::Internal(format!("qualification preparation stopped: {error}"))
            })?
            .map_err(|error| refused("qualification preparation refused", error))?
    };
    if prepared.launch_id() != req.launch_id() || prepared.owner_fingerprint() != ctx.fingerprint {
        return Err(HandlerError::Internal(
            "prepared verifier differs from the reserved launch owner".to_string(),
        ));
    }
    if expected_verifier_ref
        .is_some_and(|expected| expected != prepared.policy_source().policy.verifier_ref.as_str())
    {
        return Err(HandlerError::BadRequest(
            "signed qualification policy selects a different verifier than the release stage"
                .into(),
        ));
    }
    if expected_product_selections
        .is_some_and(|expected| expected != &prepared.product_selections())
    {
        return Err(HandlerError::BadRequest(
            "signed qualification inputs differ from the release stage's composed selections"
                .into(),
        ));
    }

    let checkout_id = format!("qualification-{}", reservation.reserved_thread_id);
    let (project, workspace_guard, provenance, lifecycle_authority, pinned_project_snapshot) =
        if let Some(snapshot_hash) = prepared.pinned_snapshot_hash() {
            // The locator is always synthesized from immutable identity; a
            // stated project is an owner+name identity the gate proved, never a
            // path that is opened.
            let display_path = PathBuf::from("/ryeos/pinned-snapshots").join(snapshot_hash);
            let source = ProjectSource::Snapshot {
                hash: snapshot_hash.to_owned(),
            };
            let project = resolve_project_context_off_thread(ResolveProjectContextRequest {
                state: state.as_ref().clone(),
                source: source.clone(),
                project_path: display_path,
                principal_id: ctx.fingerprint.clone(),
                checkout_id: checkout_id.clone(),
                pinned_realization: Some(
                    ryeos_executor::execution::project_source::PinnedContextRealization::ReadOnly,
                ),
                normalization: ProjectRootNormalization::Preserve,
                launch_timings: None,
            })
            .await
            .map_err(|error| {
                HandlerError::Internal(format!("resolve pinned qualification snapshot: {error}"))
            })?;
            // A qualification verifier and the guest it starts may not inherit any
            // project or operator environment: the guest's inputs are exactly its
            // admitted output-free read-only pinned root, as on the projectless lane.
            let mut policy = ExecutionPolicy::local_pinned_snapshot_read_only(
                ExecutionResponse::Accepted,
                snapshot_hash,
            );
            policy.environment = ryeos_app::execution_policy::ExecutionEnvironmentPolicy::None;
            let contract = crate::routes::response_modes::execute_mode::resolve_execution_contract(
                &policy,
                &source,
                &project,
                None,
                None,
                &ctx.fingerprint,
                &ctx.scopes,
                &state,
            )
            .map_err(|error| {
                HandlerError::Internal(format!("qualification pinned provenance: {error:#}"))
            })?;
            let mut contract = contract;
            if let Some(identity) = &stated_project {
                contract.provenance = contract
                    .provenance
                    .with_signer_project(ryeos_engine::contracts::LogicalProject::Stated(
                        identity.clone(),
                    ))
                    .map_err(|error| {
                        HandlerError::Internal(format!("seal the qualification project: {error:#}"))
                    })?;
            }
            let lifeline = project.temp_dir.clone().ok_or_else(|| {
                HandlerError::Internal(
                    "pinned qualification snapshot lost its materialization lease".to_string(),
                )
            })?;
            (
                project,
                lifeline,
                contract.provenance,
                contract.lifecycle_authority,
                Some(snapshot_hash.to_owned()),
            )
        } else {
            if stated_project.is_some() {
                return Err(HandlerError::BadRequest(
                    "a logical project is stated only for a pinned qualification".to_string(),
                ));
            }
            let (workspace, workspace_guard) =
                create_isolated_no_project_workspace(&state, &checkout_id)
                    .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
            let project = resolve_project_context_off_thread(ResolveProjectContextRequest {
                state: state.as_ref().clone(),
                source: ProjectSource::LiveFs,
                project_path: workspace,
                principal_id: ctx.fingerprint.clone(),
                checkout_id,
                pinned_realization: None,
                normalization: ProjectRootNormalization::Preserve,
                launch_timings: None,
            })
            .await
            .map_err(|error| {
                HandlerError::Internal(format!("resolve projectless workspace: {error}"))
            })?;
            let policy = ExecutionPolicy::projectless(ExecutionResponse::Accepted);
            policy.validate().map_err(|error| {
                HandlerError::Internal(format!("qualification execution policy: {error:#}"))
            })?;
            let project_authority = ryeos_state::objects::ExecutionProjectAuthority::projectless(
                ryeos_state::objects::EnvironmentAuthority::None,
            )
            .map_err(|error| HandlerError::Internal(format!("projectless authority: {error:#}")))?;
            let provenance =
                ryeos_app::execution_provenance::ExecutionProvenance::root_projectless(
                    project.effective_path.clone(),
                    Arc::clone(&project.request_engine),
                    Arc::clone(&workspace_guard),
                    project_authority,
                )
                .map_err(|error| {
                    HandlerError::Internal(format!("qualification provenance: {error:#}"))
                })?;
            (
                project,
                workspace_guard,
                provenance,
                policy.lifecycle_authority(),
                None,
            )
        };

    let parsed_ref = ParsedItemRef::parse(&prepared.policy_source().policy.verifier_ref)
        .map_err(|error| HandlerError::Internal(format!("signed verifier ref: {error}")))?;
    let kind_root_executable = project
        .request_engine
        .kinds
        .get(parsed_ref.kind())
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.thread_profile.as_ref())
        .is_some_and(|profile| profile.root_executable);
    if !kind_root_executable {
        return Err(HandlerError::BadRequest(
            "signed product verifier is not root executable".to_string(),
        ));
    }

    let remote_consumer = prepared.remote_consumer_proof().map_err(|error| {
        HandlerError::BadRequest(format!(
            "qualification remote consumer scenario refused: {error:#}"
        ))
    })?;
    let preflight = crate::routes::launch::preflight_dispatch_launch_off_thread(
        crate::routes::launch::OwnedDispatchPreflight {
            state: state.as_ref().clone(),
            item_ref: parsed_ref.clone(),
            project_path: project.effective_path.clone(),
            request_engine: Arc::clone(&project.request_engine),
            provenance: provenance.clone(),
            parameters: prepared.policy_source().policy.verifier_parameters.clone(),
            ref_bindings: BTreeMap::new(),
            product_selections: prepared.product_selections(),
            principal_id: ctx.fingerprint.clone(),
            principal_scopes: ctx.scopes.clone(),
            origin_site_id: ctx.execution_origin(state.threads.site_id()),
            call: None,
            // Accepted is the API acknowledgement mode. The verifier's
            // admitted execution request remains a normal bounded wait
            // invocation, as required by its sealed qualification purpose.
            launch_mode: "wait".to_string(),
            validate_only: false,
            usage_subject: None,
            usage_subject_asserted_by: None,
            launch_timings: None,
            pinned_project_snapshot: pinned_project_snapshot.clone(),
            remote_consumer,
        },
    )
    .await
    .map_err(dispatch_error)?;
    if !preflight.class.persists_pre_minted_root() {
        return Err(HandlerError::BadRequest(
            "signed product verifier cannot persist an accepted root".to_string(),
        ));
    }
    let required_caps = ryeos_app::service_registry::extract_required_caps(
        &preflight.requested_subject.resolved.metadata.extra,
    );
    if !required_caps.is_empty() {
        let cap_refs = required_caps.iter().map(String::as_str).collect::<Vec<_>>();
        state
            .authorizer
            .authorize(&ctx.scopes, &AuthorizationPolicy::require_all(&cap_refs))
            .map_err(|_| {
                HandlerError::Forbidden(
                    "signed product verifier requires additional unavailable capabilities"
                        .to_string(),
                )
            })?;
    }
    if !preflight
        .requested_subject
        .resolved
        .metadata
        .required_secrets
        .is_empty()
    {
        return Err(HandlerError::BadRequest(
            "product verifier requires credentials; qualification must be credential-free"
                .to_string(),
        ));
    }
    let root_admission = preflight.root_admission.ok_or_else(|| {
        HandlerError::Internal("threaded verifier has no root admission".to_string())
    })?;
    let admitted_definition_digest = if pinned_project_snapshot.is_some() {
        let project_binding = ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
            &project.request_engine,
            root_admission.plan_context(),
            &provenance,
        )
        .map_err(|error| {
            HandlerError::Internal(format!("pinned qualification binding: {error:#}"))
        })?;
        let PreparedQualificationLaunch::CapturedProduct(product) = &mut prepared else {
            return Err(HandlerError::BadRequest(
                "content qualification cannot use product pinned finalization".into(),
            ));
        };
        ryeos_app::operator_external_content::product_qualification::finalize_pinned_qualification_launch(
            &state,
            &ctx,
            product,
            &root_admission,
            &project.request_engine,
            root_admission.plan_context(),
            &project_binding,
            &project.effective_path,
            provenance.project_authority(),
            super::qualification_project_context::resolver(state.as_ref()).as_ref(),
        )
        .map_err(|error| refused("pinned verifier identity refused", error))?;
        prepared
            .verifier_admitted_definition_digest()
            .map(str::to_owned)
            .ok_or_else(|| {
                HandlerError::Internal("pinned verifier D1 was not finalized".to_string())
            })?
    } else {
        let admitted = root_admission
            .resolution_output()
            .effective_definition_digest()
            .map_err(|error| HandlerError::Internal(format!("verifier D1 identity: {error}")))?
            .as_str()
            .to_string();
        if Some(admitted.as_str()) != prepared.verifier_admitted_definition_digest() {
            return Err(HandlerError::BadRequest(
                "verifier source changed between product preparation and accepted dispatch"
                    .to_string(),
            ));
        }
        require_exact_admitted_fixed_pin(
            &project.request_engine,
            &root_admission,
            &prepared.policy_source().policy.subject_declaration_id,
            prepared.subject_manifest_hash(),
        )?;
        admitted
    };
    let producer_recipe_sources =
        ryeos_app::operator_external_content::product_qualification::
            resolve_current_bundle_producer_recipes_for_policy(
                &state,
                prepared.policy_source(),
                prepared.subject_manifest_hash(),
            )
            .map_err(|error| HandlerError::BadRequest(format!(
                "qualification producer recipe admission refused: {error:#}"
            )))?;
    let remote_verifier_sources = prepared
        .stage_remote_verifier_sources(&state, &producer_recipe_sources)
        .map_err(|error| {
            HandlerError::BadRequest(format!(
                "qualification remote verifier staging refused: {error:#}"
            ))
        })?;
    let consumer_content = prepared.consumer_content_identity().map_err(|error| {
        HandlerError::BadRequest(format!(
            "qualification consumer content identity refused: {error:#}"
        ))
    })?;
    let (subject, required_claims) = match &prepared {
        PreparedQualificationLaunch::CapturedProduct(product) => (
            QualificationSubject::CapturedProduct {
                product_witness_hash: product.product_witness_hash.clone(),
                witness_source: product.witness_source.clone(),
                relationship_name: product.relationship.name.clone(),
            },
            product.relationship.qualification.required_claims.clone(),
        ),
        PreparedQualificationLaunch::ActivatedContent { source, .. } => (
            QualificationSubject::ActivatedContent {
                content: source.subject.clone(),
            },
            source.allowance.required_claims.clone(),
        ),
    };
    let consumer_definitions = consumer_content
        .as_ref()
        .map(|content| content.definitions.clone());
    let consumer_invocation = match &prepared {
        PreparedQualificationLaunch::CapturedProduct(product) => {
            product.consumer_invocation.clone()
        }
        PreparedQualificationLaunch::ActivatedContent { .. } => None,
    };
    let purpose = QualificationLaunchPurpose {
        schema: QUALIFICATION_LAUNCH_PURPOSE_SCHEMA.to_string(),
        launch_id: prepared.launch_id().to_owned(),
        owner_fingerprint: prepared.owner_fingerprint().to_owned(),
        subject,
        policy_source: prepared.policy_source().clone(),
        consumer_definitions,
        consumer_content,
        consumer_invocation,
        producer_recipe_sources,
        remote_verifier_sources,
        subject_declaration_id: prepared
            .policy_source()
            .policy
            .subject_declaration_id
            .clone(),
        subject_manifest_hash: prepared.subject_manifest_hash().to_owned(),
        required_claims,
        admitted_parameters_digest: prepared
            .policy_source()
            .policy
            .admitted_parameters_digest()
            .map_err(|error| {
                HandlerError::Internal(format!("signed verifier parameters: {error:#}"))
            })?,
        verifier_ref: prepared.policy_source().policy.verifier_ref.clone(),
        verifier_effective_definition_digest: admitted_definition_digest,
        verifier_realized_definition_digest: prepared
            .verifier_realized_definition_digest()
            .map(str::to_owned)
            .ok_or_else(|| HandlerError::Internal("verifier D2 was not finalized".to_string()))?,
    };
    let (root_admission, returned_prepared) = {
        let state = Arc::clone(&state);
        let context = ctx.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            prepared.require_current(&state, &context)?;
            let root_admission = match &prepared {
                PreparedQualificationLaunch::CapturedProduct(_) => {
                    root_admission.for_qualification(purpose)
                }
                PreparedQualificationLaunch::ActivatedContent {
                    source,
                    acquisition_mode,
                    ..
                } => root_admission.for_content_qualification(
                    &state,
                    &context,
                    *acquisition_mode,
                    source,
                    purpose,
                ),
            }?;
            Ok((root_admission, prepared))
        })
        .await
        .map_err(|error| {
            HandlerError::Internal(format!("qualification attachment stopped: {error}"))
        })?
        .map_err(|error| refused("qualification root refused", error))?
    };
    let mut prepared = returned_prepared;
    let options = DispatchLaunchOptions::admitted(
        root_admission,
        preflight.root_dispatch_evidence,
        &project.effective_path,
        BTreeMap::new(),
        prepared.product_selections(),
        lifecycle_authority,
        Some(ctx.clone()),
    )
    .map_err(|error| {
        HandlerError::Internal(format!("qualification dispatch admission: {error:#}"))
    })?;
    let consumer_publication = prepared
        .take_consumer_content_publication()
        .map_err(|error| {
            HandlerError::Internal(format!("qualification consumer content staging: {error:#}"))
        })?;
    let thread_id = reservation.reserved_thread_id.clone();
    let (mut task, handoff) = crate::routes::launch::spawn_dispatch_launch_with_handoff(
        &state,
        parsed_ref,
        prepared.policy_source().policy.verifier_parameters.clone(),
        ctx.fingerprint.clone(),
        ctx.scopes.clone(),
        thread_id.clone(),
        provenance,
        options,
    );
    reservation.disarm();

    let ready_thread_id = tokio::select! {
        biased;
        readiness = handoff => match readiness {
            Ok(Ok(id)) => id,
            Ok(Err(failure)) => {
                retain_uncertain_consumer_stage_until_task_terminal(
                    task,
                    Arc::clone(&workspace_guard),
                    thread_id.clone(),
                    consumer_publication,
                );
                return Err(HandlerError::Structured {
                    code: failure.code,
                    status: failure.status,
                    body: failure.body,
                });
            }
            Err(_) => return Err(match task.await {
                Ok(Err(error)) => launch_error(error),
                Ok(Ok(())) | Err(_) => HandlerError::Internal(format!(
                    "qualification handoff closed; query launch/status for {}", req.launch_id()
                )),
            }),
        },
        outcome = &mut task => match outcome {
            Ok(Err(error)) => return Err(launch_error(error)),
            Ok(Ok(())) | Err(_) => return Err(HandlerError::Internal(format!(
                "qualification launch ended before handoff; query launch/status for {}", req.launch_id()
            ))),
        },
    };
    if ready_thread_id != thread_id {
        retain_uncertain_consumer_stage_until_task_terminal(
            task,
            Arc::clone(&workspace_guard),
            thread_id.clone(),
            consumer_publication,
        );
        return Err(HandlerError::Internal(
            "qualification handoff returned a different thread identity".to_string(),
        ));
    }
    crate::routes::launch::retain_launch_workspace_until_task_terminal(
        task,
        Some(workspace_guard),
        thread_id.clone(),
    );
    if let Some(publication) = consumer_publication {
        publication.publish().map_err(|error| HandlerError::Internal(format!(
            "qualification root {thread_id} accepted but consumer content stage could not release; query launch/status: {error:#}"
        )))?;
    }
    Ok(json!({
        "status":"accepted",
        "launch_id":req.launch_id(),
        "thread_id":ready_thread_id,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/launch-product-qualification",
    endpoint: "external-content.launch-product-qualification",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[REQUIRED_CAP],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

pub const CONTENT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/launch-content-qualification",
    endpoint: "external-content.launch-content-qualification",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[CONTENT_REQUIRED_CAP],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: ryeos_app::operator_external_content::content_qualification::ContentQualificationLaunchRequest = crate::handler_error::parse_request(params)?;
            handle_source(
                QualificationLaunchSourceRequest::ActivatedContent(req),
                ctx,
                state,
                None,
                None,
                None,
            )
            .await
            .map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_launch_has_distinct_authority_and_no_product_coordinate_alias() {
        use ryeos_app::operator_external_content::content_qualification::ContentQualificationLaunchRequest;
        assert_ne!(DESCRIPTOR.service_ref, CONTENT_DESCRIPTOR.service_ref);
        assert_ne!(DESCRIPTOR.endpoint, CONTENT_DESCRIPTOR.endpoint);
        assert_eq!(CONTENT_DESCRIPTOR.required_caps, &[CONTENT_REQUIRED_CAP]);
        assert!(!CONTENT_DESCRIPTOR.required_caps.contains(&REQUIRED_CAP));
        let coordinates = json!({
            "launch_id": "L-0123456789abcdef0123456789abcdef",
            "activation_ref": "config:test/activation", "declaration_id": "runtime"
        });
        let request: ContentQualificationLaunchRequest =
            serde_json::from_value(coordinates.clone()).unwrap();
        request.validate().unwrap();
        assert!(serde_json::from_value::<Request>(coordinates.clone()).is_err());
        for field in [
            "witness_hash",
            "witness_source",
            "relationship_name",
            "project_context",
            "parameters",
            "policy_ref",
            "purpose",
        ] {
            let mut changed = coordinates.clone();
            changed[field] = json!("caller-selected");
            assert!(serde_json::from_value::<ContentQualificationLaunchRequest>(changed).is_err());
        }
    }

    #[test]
    fn launch_surface_is_only_caller_retained_coordinates() {
        let request = json!({
            "launch_id":"L-0123456789abcdef0123456789abcdef",
            "witness_hash":"a".repeat(64),
            "witness_source":{"kind":"local_capture"},
            "relationship_name":"runtime_to_verifier",
            "project_context":null
        });
        serde_json::from_value::<Request>(request.clone()).unwrap();
        let mut missing_context = request.clone();
        missing_context
            .as_object_mut()
            .unwrap()
            .remove("project_context");
        assert!(serde_json::from_value::<Request>(missing_context).is_err());
        for forbidden in [
            "verifier_ref",
            "parameters",
            "project",
            "claims",
            "policy_ref",
            "source_site",
            "subject_manifest_hash",
            "purpose",
        ] {
            let mut forged = request.clone();
            forged[forbidden] = json!("caller-defined");
            assert!(serde_json::from_value::<Request>(forged).is_err());
        }
        assert_eq!(DESCRIPTOR.required_caps, &[REQUIRED_CAP]);
    }

    /// Fields drop in declaration order: the state goes before its store
    /// owner, and the owner before the node directory.
    struct GateFixture {
        state: Arc<AppState>,
        operator: HandlerContext,
        operator_principal: String,
        directory: tempfile::TempDir,
        project: PathBuf,
        head: String,
        other_snapshot: String,
        _owner: ryeos_app::test_support::TestStateStore,
        _node: tempfile::TempDir,
    }

    fn write_head(state: &AppState, principal: &str, project: &Path, snapshot: &str) {
        let project_ref = ryeos_executor::execution::project_source::canonical_project_ref(
            project.to_str().unwrap(),
        )
        .unwrap();
        let project_hash = lillux::cas::sha256_hex(project_ref.as_bytes());
        let principal_key = ryeos_state::refs::principal_storage_key(principal)
            .unwrap()
            .to_owned();
        let authority = state.state_store.pinned_state_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        state
            .state_store
            .write_project_head_ref(
                &principal_key,
                &project_hash,
                snapshot,
                &ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity),
                &guard,
            )
            .unwrap();
    }

    /// An operator-owned head at a real temp project; no head for anyone else.
    fn gate_fixture() -> GateFixture {
        let (node, owner, state) = crate::test_support::build_test_state_with_bundles();
        let operator = ryeos_node_runtime::operator_identity::test_support::load_operator(
            &state.config.app_root,
        )
        .unwrap();
        let principal = operator.principal_id();
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        let authority = state.state_store.pinned_state_authority().unwrap();
        let cas = authority.cas_store().unwrap();
        let head = cas.store_object(&json!({"snapshot": "head"})).unwrap();
        let other_snapshot = cas.store_object(&json!({"snapshot": "other"})).unwrap();
        drop(cas);
        drop(authority);
        write_head(&state, &principal, &project, &head);
        let operator_context = HandlerContext::new_with_authority(
            principal.clone(),
            vec![REQUIRED_CAP.to_owned()],
            true,
            Some(ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass::LocalClient),
            None,
        );
        GateFixture {
            state: Arc::new(state),
            operator: operator_context,
            operator_principal: principal,
            directory,
            project,
            head,
            other_snapshot,
            _owner: owner,
            _node: node,
        }
    }

    fn launch_request(project_path: Option<&str>, snapshot: &str) -> Value {
        json!({
            "launch_id":"L-0123456789abcdef0123456789abcdef",
            "witness_hash":"a".repeat(64),
            "witness_source":{"kind":"local_capture"},
            "relationship_name":"runtime_to_verifier",
            "project_context": match project_path {
                Some(path) => json!({"snapshot_hash": snapshot, "project_path": path}),
                None => json!({"snapshot_hash": snapshot}),
            },
        })
    }

    #[test]
    fn gate_states_the_canonical_project_for_the_operators_own_head() {
        let fixture = gate_fixture();
        // Another spelling of the same project canonicalizes to the same path.
        let respelled = fixture.project.join("sub/..");
        let stated = state_pinned_qualification_logical_project(
            &fixture.state,
            &fixture.operator,
            &respelled,
            &fixture.head,
        )
        .unwrap();
        assert_eq!(stated, fixture.project.canonicalize().unwrap());
    }

    #[test]
    fn gate_refuses_a_non_operator_caller_even_with_a_matching_head() {
        let fixture = gate_fixture();
        let stranger = format!("fp:{}", "b".repeat(64));
        write_head(&fixture.state, &stranger, &fixture.project, &fixture.head);
        let context = HandlerContext::new(stranger, Vec::new(), true);
        let refused = state_pinned_qualification_logical_project(
            &fixture.state,
            &context,
            &fixture.project,
            &fixture.head,
        )
        .unwrap_err();
        assert!(
            matches!(&refused, HandlerError::Forbidden(message)
                if message.starts_with("logical_project_requires_operator:")),
            "{refused:?}"
        );
    }

    #[test]
    fn gate_refuses_when_only_another_principal_holds_the_head() {
        let fixture = gate_fixture();
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("other-project");
        std::fs::create_dir_all(&project).unwrap();
        let stranger = format!("fp:{}", "c".repeat(64));
        write_head(&fixture.state, &stranger, &project, &fixture.head);
        let refused = state_pinned_qualification_logical_project(
            &fixture.state,
            &fixture.operator,
            &project,
            &fixture.head,
        )
        .unwrap_err();
        assert!(
            matches!(&refused, HandlerError::Conflict(message)
                if message.starts_with("snapshot_not_current_head:")
                    && message.contains("current head: none")),
            "{refused:?}"
        );
    }

    #[test]
    fn gate_refuses_a_snapshot_that_is_not_the_callers_head() {
        let fixture = gate_fixture();
        let refused = state_pinned_qualification_logical_project(
            &fixture.state,
            &fixture.operator,
            &fixture.project,
            &fixture.other_snapshot,
        )
        .unwrap_err();
        assert!(
            matches!(&refused, HandlerError::Conflict(message)
                if message.starts_with("snapshot_not_current_head:")
                    && message.contains(&fixture.head)),
            "{refused:?}"
        );
    }

    #[test]
    fn gate_refuses_an_unresolvable_path_without_a_synthetic_fallback() {
        let fixture = gate_fixture();
        let missing = fixture.directory.path().join("does-not-exist");
        let refused = state_pinned_qualification_logical_project(
            &fixture.state,
            &fixture.operator,
            &missing,
            &fixture.head,
        )
        .unwrap_err();
        assert!(
            matches!(&refused, HandlerError::BadRequest(message)
                if message.starts_with("qualification_project_path_unresolvable:")),
            "{refused:?}"
        );
    }

    #[test]
    fn request_accepts_project_path_but_refuses_unknown_fields_and_relative_paths() {
        let snapshot = "a".repeat(64);
        let accepted: Request =
            serde_json::from_value(launch_request(Some("/work/project"), &snapshot)).unwrap();
        accepted.validate().unwrap();
        assert_eq!(
            accepted
                .project_context
                .as_ref()
                .unwrap()
                .project_path
                .as_deref(),
            Some("/work/project")
        );
        let mut unknown = launch_request(Some("/work/project"), &snapshot);
        unknown["project_context"]["checkout"] = json!("/elsewhere");
        assert!(serde_json::from_value::<Request>(unknown).is_err());
        // A release source is internal and cannot be named in the request.
        let mut release = launch_request(None, &snapshot);
        release["release_source"] = json!({"project_path":"/work/project"});
        assert!(serde_json::from_value::<Request>(release).is_err());
        for bad in [
            "",
            "relative/project",
            "/work/\u{0}project",
            "/work/\nproject",
        ] {
            let request: Request =
                serde_json::from_value(launch_request(Some(bad), &snapshot)).unwrap();
            assert!(request.validate().is_err(), "{bad:?} must be refused");
        }
        let too_long = format!("/{}", "a".repeat(4096));
        let request: Request =
            serde_json::from_value(launch_request(Some(&too_long), &snapshot)).unwrap();
        assert!(request.validate().is_err());
        let none: Request = serde_json::from_value(launch_request(None, &snapshot)).unwrap();
        none.validate().unwrap();
    }

    #[tokio::test]
    async fn a_project_path_request_reaches_the_gate_before_anything_is_reserved() {
        let fixture = gate_fixture();
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("no-head");
        std::fs::create_dir_all(&project).unwrap();
        let request: Request = serde_json::from_value(launch_request(
            Some(project.to_str().unwrap()),
            &fixture.head,
        ))
        .unwrap();
        let launch_id = request.launch_id.clone();
        let refused = handle(
            request,
            fixture.operator.clone(),
            Arc::clone(&fixture.state),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&refused, HandlerError::Conflict(message)
                if message.starts_with("snapshot_not_current_head:")),
            "{refused:?}"
        );
        assert!(
            fixture
                .state
                .state_store
                .launch_planning_status(&launch_id, &fixture.operator_principal)
                .unwrap()
                .is_none(),
            "a gate refusal must not reserve the launch"
        );
    }

    /// Stores `files` (relative path -> bytes) as one AI-only project
    /// snapshot in the fixture's CAS and returns its hash.
    fn store_ai_only_snapshot(state: &AppState, files: &[(&str, &[u8])]) -> String {
        use ryeos_state::objects::{
            project_file::ProjectFile, project_snapshot::ProjectSnapshot, project_tree::ProjectTree,
        };
        let source = tempfile::tempdir().unwrap();
        for (relative, bytes) in files {
            let path = source.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let pinned = lillux::PinnedDirectory::open(source.path())
            .unwrap()
            .unwrap();
        let no_node_patterns =
            ryeos_state::ignore::IgnoreMatcher::from_config(&ryeos_state::ignore::IgnoreConfig {
                patterns: Vec::new(),
            })
            .unwrap();
        let policy = ryeos_state::project_sync::capture_snapshot_policy_from_pinned(
            &pinned,
            &no_node_patterns,
            ryeos_state::project_sync::ProjectSyncScope::AiOnly,
        )
        .unwrap();
        let authority = state.state_store.pinned_state_authority().unwrap();
        let cas = authority.cas_store().unwrap();
        let mut tree = BTreeMap::new();
        for (relative, bytes) in files {
            let blob_hash = cas.store_blob(bytes).unwrap();
            let file = ProjectFile {
                blob_hash,
                size: u64::try_from(bytes.len()).unwrap(),
                normalized_mode: ProjectFile::REGULAR_MODE,
            };
            tree.insert(
                (*relative).to_owned(),
                cas.store_object(&file.to_value()).unwrap(),
            );
        }
        let snapshot = ProjectSnapshot {
            project_tree_hash: cas
                .store_object(&ProjectTree { files: tree }.to_value())
                .unwrap(),
            effective_policy_hash: cas.store_object(&policy.to_value()).unwrap(),
            message: None,
            parent_hashes: Vec::new(),
            created_at: lillux::time::iso8601_now(),
            source: "push".to_owned(),
        };
        cas.store_object(&snapshot.to_value()).unwrap()
    }

    /// The stated project path is a real host path, but a pinned
    /// qualification resolves only the snapshot: a file present in the live
    /// checkout at that path and absent from the snapshot never appears in
    /// the effective root, and the effective root is not the checkout.
    #[tokio::test]
    async fn a_stated_project_path_is_a_label_and_the_live_checkout_is_never_resolved() {
        let fixture = gate_fixture();
        let in_snapshot = ".ai/config/qlr/in-snapshot.yaml";
        let live_only = ".ai/config/qlr/live-only.yaml";
        let snapshot =
            store_ai_only_snapshot(&fixture.state, &[(in_snapshot, b"snapshot: true\n")]);
        // A project of its own: the fixture's project already has a head.
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("labelled-project");
        std::fs::create_dir_all(&project).unwrap();
        write_head(
            &fixture.state,
            &fixture.operator_principal,
            &project,
            &snapshot,
        );
        for relative in [in_snapshot, live_only] {
            let path = project.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"live: true\n").unwrap();
        }
        let stated = state_pinned_qualification_logical_project(
            &fixture.state,
            &fixture.operator,
            &project,
            &snapshot,
        )
        .unwrap();
        let context = resolve_project_context_off_thread(ResolveProjectContextRequest {
            state: fixture.state.as_ref().clone(),
            source: ProjectSource::Snapshot {
                hash: snapshot.clone(),
            },
            project_path: stated.clone(),
            principal_id: fixture.operator_principal.clone(),
            checkout_id: "qualification-label-test".to_owned(),
            pinned_realization: Some(
                ryeos_executor::execution::project_source::PinnedContextRealization::ReadOnly,
            ),
            normalization: ProjectRootNormalization::Preserve,
            launch_timings: None,
        })
        .await
        .unwrap();
        assert_eq!(context.original_path, stated);
        assert_eq!(context.snapshot_hash.as_deref(), Some(snapshot.as_str()));
        let effective = context.effective_path.canonicalize().unwrap();
        assert!(
            !effective.starts_with(&stated) && !stated.starts_with(&effective),
            "the effective root {} must not be the live checkout {}",
            effective.display(),
            stated.display()
        );
        assert_eq!(
            std::fs::read(effective.join(in_snapshot)).unwrap(),
            b"snapshot: true\n",
            "the snapshot's bytes win over the live checkout's"
        );
        assert!(
            !effective.join(live_only).exists(),
            "a live-only file must never appear in the pinned root"
        );
    }

    #[tokio::test]
    async fn release_source_runs_the_gate_under_the_launching_principal() {
        let fixture = gate_fixture();
        // Another principal's head at a path the launcher has no head for.
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("release-project");
        std::fs::create_dir_all(&project).unwrap();
        let stranger = format!("fp:{}", "d".repeat(64));
        write_head(&fixture.state, &stranger, &project, &fixture.head);
        // The overlay capture hash in the request differs from the source
        // snapshot the gate checks.
        let request: Request =
            serde_json::from_value(launch_request(None, &fixture.other_snapshot)).unwrap();
        let refused = handle_expected_verifier(
            request,
            fixture.operator.clone(),
            Arc::clone(&fixture.state),
            None,
            None,
            Some(ReleaseHeadSource {
                project_path: project,
                head_snapshot_hash: fixture.head.clone(),
            }),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&refused, HandlerError::Conflict(message)
                if message.starts_with("snapshot_not_current_head:")
                    && message.contains("current head: none")),
            "{refused:?}"
        );
        // The launcher's own head at its own path passes the same gate.
        let stated = state_pinned_qualification_logical_project(
            &fixture.state,
            &fixture.operator,
            &fixture.project,
            &fixture.head,
        )
        .unwrap();
        assert_eq!(stated, fixture.project.canonicalize().unwrap());
    }
}
