//! Accepted launch of the exact signed product verifier.
//!
//! The caller owns only the launch coordinate and product relationship. The
//! daemon resolves the verifier, parameters, subject selection, and purpose
//! from current trusted Bundle authority after reserving the launch ID.

use std::collections::BTreeMap;
use std::{path::PathBuf, sync::Arc};

use ryeos_app::execution_policy::{ExecutionPolicy, ExecutionResponse};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::operator_external_content::qualification_launch::{
    PreparedQualificationLaunch, QualificationLaunchSourceRequest,
};
use ryeos_app::state::AppState;
use ryeos_executor::execution::project_source::ProjectSource;
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
        .map_err(|error| HandlerError::BadRequest(format!("verifier declarer: {error}")))?;
    let declarations = ryeos_engine::external_content::effective_external_content_declarations(
        resolution, contract, declarer,
    )
    .map_err(|error| HandlerError::BadRequest(format!("verifier external content: {error}")))?
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

/// Reserve before any witness/Bundle resolution. Once reserved, every exit
/// settles it; after handoff, the launch task owns settlement. A repeated ID
/// is never treated as permission to launch again after an ambiguous ACK.
pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    handle_source(
        QualificationLaunchSourceRequest::CapturedProduct(req),
        ctx,
        state,
    )
    .await
}

async fn handle_source(
    req: QualificationLaunchSourceRequest,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    let required_cap = match &req {
        QualificationLaunchSourceRequest::CapturedProduct(_) => REQUIRED_CAP,
        QualificationLaunchSourceRequest::ActivatedContent(_) => CONTENT_REQUIRED_CAP,
    };
    req.validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    ryeos_app::operator_authority::require_admitted_operator(&state, &ctx)
        .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    state
        .authorizer
        .authorize(&ctx.scopes, &AuthorizationPolicy::require(required_cap))
        .map_err(|_| {
            HandlerError::Forbidden(format!("missing required capability: {required_cap}"))
        })?;

    let mut reservation =
        AcceptedLaunchAdmissionGuard::reserve(&state, &req.launch_id(), &ctx.fingerprint)
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
            .map_err(|error| {
                HandlerError::BadRequest(format!("qualification preparation refused: {error:#}"))
            })?
    };
    if prepared.launch_id() != req.launch_id() || prepared.owner_fingerprint() != ctx.fingerprint {
        return Err(HandlerError::Internal(
            "prepared verifier differs from the reserved launch owner".to_string(),
        ));
    }

    let checkout_id = format!("qualification-{}", reservation.reserved_thread_id);
    let (project, workspace_guard, provenance, lifecycle_authority, pinned_project_snapshot) =
        if let Some(snapshot_hash) = prepared.pinned_snapshot_hash() {
            // The display locator is synthesized from immutable identity; no
            // caller filesystem path is accepted or opened for this lane.
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
            let policy = ExecutionPolicy::local_pinned_snapshot_read_only(
                ExecutionResponse::Accepted,
                snapshot_hash,
            );
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
            let (workspace, workspace_guard) =
                create_isolated_no_project_workspace(&state, &checkout_id)
                    .map_err(|error| HandlerError::Internal(error.to_string()))?;
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
                HandlerError::Internal(format!("qualification execution policy: {error}"))
            })?;
            let project_authority = ryeos_state::objects::ExecutionProjectAuthority::projectless(
                ryeos_state::objects::EnvironmentAuthority::None,
            )
            .map_err(|error| HandlerError::Internal(format!("projectless authority: {error}")))?;
            let provenance =
                ryeos_app::execution_provenance::ExecutionProvenance::root_projectless(
                    project.effective_path.clone(),
                    Arc::clone(&project.request_engine),
                    Arc::clone(&workspace_guard),
                    project_authority,
                )
                .map_err(|error| {
                    HandlerError::Internal(format!("qualification provenance: {error}"))
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
        )
        .map_err(|error| HandlerError::BadRequest(format!("pinned verifier identity refused: {error:#}")))?;
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
            &prepared.subject_manifest_hash(),
        )?;
        admitted
    };
    let producer_recipe_sources =
        ryeos_app::operator_external_content::product_qualification::
            resolve_current_bundle_producer_recipes_for_policy(
                &state,
                &prepared.policy_source(),
                &prepared.subject_manifest_hash(),
            )
            .map_err(|error| HandlerError::BadRequest(format!(
                "qualification producer recipe admission refused: {error:#}"
            )))?;
    let (subject, required_claims, consumer_content) = match &prepared {
        PreparedQualificationLaunch::CapturedProduct(product) => (
            QualificationSubject::CapturedProduct {
                product_witness_hash: product.product_witness_hash.clone(),
                witness_source: product.witness_source.clone(),
                relationship_name: product.relationship.name.clone(),
            },
            product.relationship.qualification.required_claims.clone(),
            product.consumer_content_identity().map_err(|error| {
                HandlerError::BadRequest(format!(
                    "qualification consumer content identity refused: {error:#}"
                ))
            })?,
        ),
        PreparedQualificationLaunch::ActivatedContent { source, .. } => (
            QualificationSubject::ActivatedContent {
                content: source.subject.clone(),
            },
            source.allowance.required_claims.clone(),
            None,
        ),
    };
    let consumer_definitions = consumer_content
        .as_ref()
        .map(|content| content.definitions.clone());
    let purpose = QualificationLaunchPurpose {
        schema: QUALIFICATION_LAUNCH_PURPOSE_SCHEMA.to_string(),
        launch_id: prepared.launch_id().to_owned(),
        owner_fingerprint: prepared.owner_fingerprint().to_owned(),
        subject,
        policy_source: prepared.policy_source().clone(),
        consumer_definitions,
        consumer_content,
        producer_recipe_sources,
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
                HandlerError::Internal(format!("signed verifier parameters: {error}"))
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
            let root_admission = match &prepared {
                PreparedQualificationLaunch::CapturedProduct(_) => {
                    prepared.require_current(&state, &context)?;
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
        .map_err(|error| {
            HandlerError::BadRequest(format!("qualification root refused: {error:#}"))
        })?
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
    let consumer_publication = match &mut prepared {
        PreparedQualificationLaunch::CapturedProduct(product) => {
            product.take_consumer_content_publication()
        }
        PreparedQualificationLaunch::ActivatedContent { .. } => Ok(None),
    }
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
}
