//! `ui/invocations/dispatch` — browser-session invocation transport.
//!
//! The browser names only an entry in the exact binding compiled when its
//! session was minted. Executable refs, command tokens and capability claims
//! remain server-side signed data. Renderer-local navigation, focus and
//! overlay changes reduce in the shared client core and never cross this
//! execution transport.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::service_registry::UiDispatchMode;
use ryeos_app::state::AppState;
use ryeos_client_base::ui::{UiBindingCoordinate, UiBindingPayload, UiBindingRequest};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_executor::executor::ServiceAvailability;

use crate::browser_session::{
    AdmittedBindingAttachment, BindingAttachmentCoordinate, BrowserSession,
};
use crate::compiled_binding::{
    CompiledUiAffordance, CompiledUiProducer, CompiledUiResultEffect, CompiledUiSource,
    CompiledUiTarget,
};
use crate::state::get_ui_state;
use crate::{seat_auth::SeatCaller, thread_authorization::authorize_exact_thread_subjects};

struct BoundInvocation {
    target: CompiledUiTarget,
    params: Value,
}

/// Extract session_id from the handler context's fingerprint.
fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

fn invocation_context_for_session(session: &BrowserSession) -> HandlerContext {
    HandlerContext::new(
        session.principal_id.clone(),
        session.granted_caps.clone(),
        true,
    )
}

pub(crate) struct PreparedInvocation {
    pub(crate) project: ryeos_executor::execution::project_source::ResolvedProjectContext,
    pub(crate) exec_ctx: ryeos_executor::executor::ExecutionContext,
    pub(crate) project_content: Option<lillux::PinnedDirectory>,
}

pub async fn handle(input: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let req: UiBindingRequest = serde_json::from_value(input.clone()).map_err(|e| {
        HandlerError::BadRequest(format!("invalid ui.invocations.dispatch request: {e}"))
    })?;

    // Require browser session.
    let session_id = session_id_from_context(&ctx).ok_or_else(|| {
        HandlerError::Forbidden("session cookie required for UI invocation dispatch".into())
    })?;

    let ui = get_ui_state(&state).expect("UiState not set");

    let session = ui
        .browser_sessions
        .get_session(&session_id)
        .ok_or_else(|| HandlerError::Forbidden("session expired or invalid".into()))?;
    let attachment_coordinate = BindingAttachmentCoordinate {
        binding_attachment_id: req.binding_attachment_id.clone(),
        binding_generation: req.binding_generation,
        binding_digest: req.binding_digest.clone(),
    };
    let attachment = ui
        .browser_sessions
        .resolve_attachment(&session_id, &attachment_coordinate)
        .map_err(|error| binding_stale(&error.to_string()))?;
    if attachment
        .compiled_binding
        .binding
        .node_policy_generation_digest
        != state.node_policy.generation_digest()
    {
        return Err(binding_stale(
            "the node policy generation changed after session mint",
        ));
    }
    enforce_request_bounds(&state, &req, &input)?;
    authorize_route_context(&ctx, &state, &attachment, &req)?;
    let project_access =
        crate::seat_auth::attachment_project_access(&attachment).map_err(|_| {
            binding_stale("the selected project path no longer names its retained authority")
        })?;
    let project_path = match project_access.as_ref() {
        Some(access) => access.path().to_path_buf(),
        None => state.config.app_root.clone(),
    };
    // Authored source parameters and projection filters receive the stable
    // validated project identity.  The descriptor-rooted path above remains
    // solely the filesystem resolution coordinate and must never escape into
    // a request or durable row.
    let project_marker = crate::seat_auth::attachment_project_query_identity(&attachment)?
        .map(|path| path.to_string_lossy().into_owned());
    let bound = resolve_binding_request(&attachment, &req, project_marker.as_deref())?;
    let item_ref = bound.target.identity.canonical_ref.clone();
    let invocation_ctx = invocation_context_for_session(&session);
    let prepared = prepare_item_ref(
        &invocation_ctx,
        &state,
        &project_path,
        project_access
            .as_ref()
            .map(|access| access.try_clone_directory())
            .transpose()?,
    )?;
    let current = prepared
        .exec_ctx
        .engine
        .with_checked_bundle_generation(|generation| {
            if generation.request_engine_generation_identity()
                != attachment
                    .compiled_binding
                    .binding
                    .request_engine_generation_identity
            {
                return Err(binding_stale(
                    "the engine generation changed after session mint",
                ));
            }
            super::ui_launch_mint::revalidate_binding_authority(
                generation,
                attachment.compiled_binding.as_ref(),
                attachment
                    .project_authority
                    .as_ref()
                    .map(|_| project_path.as_path()),
                prepared.project_content.as_ref(),
            )?;
            super::ui_launch_mint::compile_target(generation, &prepared, &item_ref)
        })?;
    if current != bound.target {
        return Err(binding_stale(
            "a signed binding target changed after session mint",
        ));
    }
    let verified = match prepared.project_content.as_ref() {
        Some(project_content) => prepared
            .exec_ctx
            .engine
            .resolve_verified_under_project_authority(
                &prepared.exec_ctx.plan_ctx,
                &CanonicalRef::parse(&item_ref)?,
                &prepared.project.effective_path,
                project_content,
            )
            .map_err(|error| HandlerError::BadRequest(error.to_string()))?,
        None => ryeos_executor::executor::resolve_and_verify(
            &prepared.exec_ctx.engine,
            &prepared.exec_ctx.plan_ctx,
            &item_ref,
            None,
        )
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?,
    };

    let invocation_id = uuid::Uuid::new_v4().to_string();
    let trusted_handler_context = select_trusted_handler_context(
        dispatch_mode(bound.target.dispatch_class),
        &ctx,
        &invocation_ctx,
    );

    let source_safe = bound.target.source_safe;
    let mut result = crate::seat_auth::with_compiled_ui_attachment(attachment.clone(), async {
        if bound.target.source_safe {
            execute_read_only_service(
                &item_ref,
                bound.params,
                &state,
                prepared,
                verified,
                trusted_handler_context,
            )
            .await
        } else {
            let _dispatch_admission = ui
                .browser_sessions
                .admit_attachment_dispatch(&session_id, &attachment_coordinate)
                .map_err(|error| binding_stale(&error.to_string()))?;
            execute_prepared_item_ref(
                &item_ref,
                bound.params,
                &state,
                prepared,
                verified,
                &invocation_ctx.scopes,
                trusted_handler_context,
            )
            .await
        }
    })
    .await?;
    if source_safe {
        ui.browser_sessions
            .recheck_attachment_after_read(&session_id, &attachment_coordinate)
            .map_err(|error| binding_stale(&error.to_string()))?;
    }
    retain_declared_result_effect(&bound.target, &mut result)?;

    ui.session_bus.publish(
        &session_id,
        "invocation.dispatched",
        json!({
            "target": { "kind": "ref", "ref": item_ref },
            "invocation_id": invocation_id,
            "status": "executed",
        }),
    );

    Ok(json!({
        "status": "executed",
        "target": { "kind": "ref", "ref": item_ref },
        "invocation_id": invocation_id,
        "binding_attachment_id": attachment_coordinate.binding_attachment_id,
        "binding_generation": attachment_coordinate.binding_generation,
        "binding_digest": attachment_coordinate.binding_digest,
        "result": result,
    }))
}

fn retain_declared_result_effect(target: &CompiledUiTarget, result: &mut Value) -> Result<()> {
    let Some(fields) = result.as_object_mut() else {
        if target.result_effect.is_some() {
            return Err(HandlerError::Internal(
                "service declared a UI result effect but returned a non-object".into(),
            )
            .into());
        }
        return Ok(());
    };
    let authored = fields.remove("ui_transition");
    match target.result_effect {
        None => Ok(()),
        Some(CompiledUiResultEffect::AdmitBindingAttachment) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct AttachmentTransition {
                kind: String,
                attachment: ryeos_client_base::ui::UiBindingAttachment,
            }
            let transition: AttachmentTransition =
                serde_json::from_value(authored.ok_or_else(|| {
                    HandlerError::Internal(
                        "attachment-admission service returned no typed UI transition".into(),
                    )
                })?)
                .map_err(|_| {
                    HandlerError::Internal(
                        "attachment-admission service returned an invalid typed UI transition"
                            .into(),
                    )
                })?;
            if transition.kind != "admit_binding_attachment" {
                return Err(HandlerError::Internal(
                    "attachment-admission service returned an invalid typed UI transition".into(),
                )
                .into());
            }
            fields.insert(
                "ui_transition".to_string(),
                json!({"kind":"admit_binding_attachment","attachment":transition.attachment}),
            );
            Ok(())
        }
    }
}

fn authorize_route_context(
    ctx: &HandlerContext,
    state: &AppState,
    attachment: &Arc<AdmittedBindingAttachment>,
    req: &UiBindingRequest,
) -> Result<()> {
    let (
        UiBindingCoordinate::SurfaceRoute,
        UiBindingPayload::Input {
            route: Some(route), ..
        },
    ) = (&req.coordinate, &req.payload)
    else {
        return Ok(());
    };
    let Some(thread_id) = route.thread_id.as_deref() else {
        return Ok(());
    };
    let subjects = authorize_exact_thread_subjects(
        ctx,
        state,
        &SeatCaller::Attachment(attachment.clone()),
        &[thread_id],
    )?;
    if route.chain_root_id.as_deref() != Some(subjects[0].chain_root_id.as_str()) {
        return Err(HandlerError::NotFound.into());
    }
    Ok(())
}

fn dispatch_mode(class: crate::compiled_binding::CompiledUiDispatchClass) -> UiDispatchMode {
    match class {
        crate::compiled_binding::CompiledUiDispatchClass::Verified => UiDispatchMode::Verified,
        crate::compiled_binding::CompiledUiDispatchClass::SessionLocal => {
            UiDispatchMode::SessionLocal
        }
    }
}

fn binding_stale(message: &str) -> anyhow::Error {
    HandlerError::Structured {
        code: "ui_binding_stale".to_string(),
        status: 409,
        body: json!({"code":"ui_binding_stale","error":message,"retryable":false,"remediation":"mint a new UI session"}),
    }.into()
}

fn enforce_request_bounds(state: &AppState, req: &UiBindingRequest, input: &Value) -> Result<()> {
    let maximum = state
        .node_config
        .routes
        .iter()
        .find(|route| route.response.source.as_deref() == Some(DESCRIPTOR.service_ref))
        .map(|route| route.limits.body_bytes_max)
        .ok_or_else(|| HandlerError::Internal("UI binding dispatch route is absent".into()))?;
    let actual = serde_json::to_vec(input)?.len() as u64;
    if actual > maximum {
        return Err(HandlerError::BadRequest(format!(
            "UI binding request is {actual} bytes; maximum is {maximum}"
        ))
        .into());
    }
    req.validate_bounds(ryeos_client_base::ui::UiBindingRequestBounds {
        max_request_bytes: maximum,
        max_input_bytes: maximum,
    })
    .map_err(|error| HandlerError::BadRequest(error.to_string()).into())
}

fn resolve_binding_request(
    attachment: &AdmittedBindingAttachment,
    req: &UiBindingRequest,
    project_root: Option<&str>,
) -> Result<BoundInvocation> {
    match (&req.coordinate, &req.payload) {
        (UiBindingCoordinate::SurfaceRoute, UiBindingPayload::Input { value, route }) => {
            let surface_route = attachment
                .compiled_binding
                .binding
                .surface_route
                .as_ref()
                .ok_or_else(|| {
                    HandlerError::Forbidden(
                        "surface route is absent from the compiled binding".into(),
                    )
                })?;
            let mut params = resolve_session_markers(&surface_route.parameters, project_root)?;
            let params = params.as_object_mut().ok_or_else(|| {
                HandlerError::Internal("compiled surface route parameters are not an object".into())
            })?;
            params.insert(
                surface_route.bindings.input_parameter.clone(),
                Value::String(value.clone()),
            );
            if let Some(route) = route {
                validate_route_context(route)?;
                if let Some(thread_id) = &route.thread_id {
                    let parameter = surface_route
                        .bindings
                        .thread_target_parameter
                        .as_ref()
                        .ok_or_else(|| {
                            HandlerError::BadRequest(
                                "the signed surface route does not accept a thread target".into(),
                            )
                        })?;
                    params.insert(
                        parameter.clone(),
                        json!({"kind":"thread","thread_id":thread_id}),
                    );
                }
                if route.interrupt {
                    let parameter = surface_route
                        .bindings
                        .interrupt_intent_parameter
                        .as_ref()
                        .ok_or_else(|| {
                            HandlerError::BadRequest(
                                "the signed surface route does not accept interrupt delivery"
                                    .into(),
                            )
                        })?;
                    params.insert(parameter.clone(), Value::String("interrupt".to_string()));
                }
            }
            Ok(BoundInvocation {
                target: surface_route.target.clone(),
                params: Value::Object(params.clone()),
            })
        }
        (
            UiBindingCoordinate::Source { view_ref, channel },
            UiBindingPayload::SourceParameters { params },
        ) => {
            let source = attachment
                .compiled_binding
                .binding
                .sources
                .get(view_ref)
                .and_then(|entries| entries.get(channel))
                .ok_or_else(|| {
                    HandlerError::Forbidden(
                        "source coordinate is absent from the compiled binding".into(),
                    )
                })?;
            Ok(BoundInvocation {
                target: source.target.clone(),
                params: bind_source_parameters(source, params, project_root)?,
            })
        }
        (
            UiBindingCoordinate::Affordance {
                view_ref,
                affordance_id,
            },
            payload,
        ) => {
            let entry = attachment
                .compiled_binding
                .binding
                .affordances
                .get(view_ref)
                .and_then(|entries| entries.get(affordance_id))
                .ok_or_else(|| {
                    HandlerError::Forbidden(
                        "affordance coordinate is absent from the compiled binding".into(),
                    )
                })?;
            let CompiledUiAffordance::Execution {
                producer,
                invoke,
                target,
            } = entry
            else {
                return Err(HandlerError::BadRequest(
                    "UI-local affordances do not use execution dispatch".into(),
                )
                .into());
            };
            let (actual_producer, payload) = match payload {
                UiBindingPayload::Selection { record } => (
                    CompiledUiProducer::Selection,
                    ryeos_client_base::ui::content::Payload::Selection(record),
                ),
                UiBindingPayload::Input { value, route } if route.is_none() => (
                    CompiledUiProducer::Input,
                    ryeos_client_base::ui::content::Payload::Input(value),
                ),
                UiBindingPayload::Tokens { tokens, arguments } => (
                    CompiledUiProducer::Tokens,
                    ryeos_client_base::ui::content::Payload::Tokens { tokens, arguments },
                ),
                UiBindingPayload::Input { .. } => {
                    return Err(HandlerError::BadRequest(
                        "affordance input cannot carry surface-route context".into(),
                    )
                    .into());
                }
                UiBindingPayload::SourceParameters { .. } => {
                    return Err(HandlerError::BadRequest(
                        "source payload cannot invoke an affordance".into(),
                    )
                    .into());
                }
            };
            if *producer != actual_producer {
                return Err(HandlerError::BadRequest(
                    "payload producer does not match the signed affordance".into(),
                )
                .into());
            }
            let authored = json!({"invoke": invoke});
            let resolved = ryeos_client_base::ui::content::resolve_affordance_invoke(
                &authored,
                match actual_producer {
                    CompiledUiProducer::Selection => {
                        ryeos_client_base::ui::content::Producer::Selection
                    }
                    CompiledUiProducer::Input => ryeos_client_base::ui::content::Producer::Input,
                    CompiledUiProducer::Tokens => ryeos_client_base::ui::content::Producer::Tokens,
                },
                &payload,
            )
            .ok_or_else(|| {
                HandlerError::BadRequest(
                    "affordance payload does not satisfy the signed template".into(),
                )
            })?;
            let params = match resolved {
                ryeos_client_base::ui::content::AffordanceInvoke::Service {
                    item_ref,
                    args,
                    ..
                } => {
                    if item_ref != target.identity.canonical_ref {
                        return Err(binding_stale(
                            "resolved affordance target differs from its compiled target",
                        ));
                    }
                    args
                }
                ryeos_client_base::ui::content::AffordanceInvoke::Rye { tokens, args, .. } => {
                    json!({"project_path": project_root, "tokens": tokens, "arguments": args})
                }
                ryeos_client_base::ui::content::AffordanceInvoke::Ui { .. }
                | ryeos_client_base::ui::content::AffordanceInvoke::OpenSavedViewSet { .. }
                | ryeos_client_base::ui::content::AffordanceInvoke::SaveActiveViewSet { .. } => {
                    return Err(HandlerError::BadRequest(
                        "UI-local affordance crossed execution dispatch".into(),
                    )
                    .into());
                }
            };
            let params = resolve_session_markers(&params, project_root)?;
            Ok(BoundInvocation {
                target: target.clone(),
                params,
            })
        }
        _ => Err(HandlerError::BadRequest(
            "binding coordinate and payload kind do not match".into(),
        )
        .into()),
    }
}

fn validate_route_context(route: &ryeos_client_base::ui::UiBindingRouteContext) -> Result<()> {
    for (name, value) in [
        ("thread_id", route.thread_id.as_deref()),
        ("chain_root_id", route.chain_root_id.as_deref()),
    ] {
        if value.is_some_and(|value| {
            value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
        }) {
            return Err(HandlerError::BadRequest(format!(
                "surface route `{name}` is not a bounded identifier"
            ))
            .into());
        }
    }
    if route.thread_id.is_some() != route.chain_root_id.is_some() {
        return Err(HandlerError::BadRequest(
            "surface route thread_id and chain_root_id must be supplied together".into(),
        )
        .into());
    }
    if route.interrupt && route.thread_id.is_none() {
        return Err(HandlerError::BadRequest(
            "surface route interrupt requires an exact thread_id".into(),
        )
        .into());
    }
    Ok(())
}

fn bind_source_parameters(
    source: &CompiledUiSource,
    supplied: &Value,
    project_root: Option<&str>,
) -> Result<Value> {
    let supplied = supplied
        .as_object()
        .ok_or_else(|| HandlerError::BadRequest("source parameters must be an object".into()))?;
    let template = source.parameters.as_object().ok_or_else(|| {
        HandlerError::Internal("compiled source parameters are not an object".into())
    })?;
    if supplied
        .keys()
        .any(|key| !template.contains_key(key) && !source.dynamic_parameters.contains(key))
    {
        return Err(HandlerError::BadRequest(
            "source parameters contain a key absent from the signed binding".into(),
        )
        .into());
    }
    let mut bound = serde_json::Map::new();
    for (key, authored) in template {
        let supplied = supplied.get(key);
        let value = if source.dynamic_parameters.contains(key) {
            supplied
                .map(|value| bind_dynamic_source_parameter(key, value))
                .transpose()?
                .unwrap_or_else(|| authored.clone())
        } else {
            bind_authored_parameter(authored, supplied, key)?
        };
        bound.insert(key.clone(), value);
    }
    for key in &source.dynamic_parameters {
        if template.contains_key(key) {
            continue;
        }
        if let Some(value) = supplied.get(key) {
            bound.insert(key.clone(), bind_dynamic_source_parameter(key, value)?);
        }
    }
    let value = Value::Object(bound);
    resolve_session_markers(&value, project_root)
}

fn bind_dynamic_source_parameter(key: &str, value: &Value) -> Result<Value> {
    if !value.is_string() {
        return Err(HandlerError::BadRequest(format!(
            "dynamic source parameter `{key}` must be a string"
        ))
        .into());
    }
    Ok(value.clone())
}

fn bind_authored_parameter(
    authored: &Value,
    supplied: Option<&Value>,
    path: &str,
) -> Result<Value> {
    match authored {
        Value::String(marker) if marker.starts_with("@facet:") => {
            supplied.cloned().ok_or_else(|| {
                HandlerError::BadRequest(format!(
                    "source parameter `{path}` requires its signed facet value"
                ))
                .into()
            })
        }
        Value::String(marker) if marker.starts_with("@session:") => {
            Ok(Value::String(marker.clone()))
        }
        Value::Object(authored_fields) => {
            let supplied_fields = supplied.and_then(Value::as_object).ok_or_else(|| {
                HandlerError::BadRequest(format!(
                    "source parameter `{path}` must preserve its signed object shape"
                ))
            })?;
            if supplied_fields
                .keys()
                .any(|key| !authored_fields.contains_key(key))
            {
                return Err(HandlerError::BadRequest(format!(
                    "source parameter `{path}` contains an unsigned field"
                ))
                .into());
            }
            Ok(Value::Object(
                authored_fields
                    .iter()
                    .map(|(key, value)| {
                        let child = format!("{path}.{key}");
                        Ok((
                            key.clone(),
                            bind_authored_parameter(value, supplied_fields.get(key), &child)?,
                        ))
                    })
                    .collect::<Result<_>>()?,
            ))
        }
        Value::Array(authored_values) => {
            let supplied_values = supplied.and_then(Value::as_array).ok_or_else(|| {
                HandlerError::BadRequest(format!(
                    "source parameter `{path}` must preserve its signed array shape"
                ))
            })?;
            if authored_values.len() != supplied_values.len() {
                return Err(HandlerError::BadRequest(format!(
                    "source parameter `{path}` changed its signed array length"
                ))
                .into());
            }
            Ok(Value::Array(
                authored_values
                    .iter()
                    .zip(supplied_values)
                    .enumerate()
                    .map(|(index, (value, supplied))| {
                        bind_authored_parameter(value, Some(supplied), &format!("{path}[{index}]"))
                    })
                    .collect::<Result<_>>()?,
            ))
        }
        _ if supplied.is_none() || supplied == Some(authored) => Ok(authored.clone()),
        _ => Err(HandlerError::BadRequest(format!(
            "source parameter `{path}` attempts to replace signed data"
        ))
        .into()),
    }
}

fn resolve_session_markers(value: &Value, project_root: Option<&str>) -> Result<Value> {
    match value {
        Value::String(marker) if marker == "@session:project_root_or_null" => {
            Ok(project_root.map(Value::from).unwrap_or(Value::Null))
        }
        Value::String(marker) if marker == "@session:project_root" => {
            project_root.map(Value::from).ok_or_else(|| {
                HandlerError::BadRequest(
                    "this signed binding entry requires a project-scoped UI session".into(),
                )
                .into()
            })
        }
        Value::String(marker) if marker.starts_with("@session:") => Err(HandlerError::Internal(
            format!("unsupported compiled session marker `{marker}`"),
        )
        .into()),
        Value::Array(values) => Ok(Value::Array(
            values
                .iter()
                .map(|value| resolve_session_markers(value, project_root))
                .collect::<Result<_>>()?,
        )),
        Value::Object(values) => Ok(Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    Ok((key.clone(), resolve_session_markers(value, project_root)?))
                })
                .collect::<Result<_>>()?,
        )),
        value => Ok(value.clone()),
    }
}

fn select_trusted_handler_context(
    dispatch_mode: UiDispatchMode,
    browser_context: &HandlerContext,
    invocation_context: &HandlerContext,
) -> HandlerContext {
    match dispatch_mode {
        // These services intentionally operate on the transient browser
        // session and therefore need its session:<id> principal.
        UiDispatchMode::SessionLocal => browser_context.clone(),
        // Ordinary verified services execute under the durable principal and
        // scopes sealed into the execution context. Supplying the browser
        // session context here would create an identity mismatch.
        UiDispatchMode::Verified => invocation_context.clone(),
    }
}

pub(crate) fn prepare_item_ref(
    ctx: &HandlerContext,
    state: &AppState,
    project_path: &std::path::Path,
    project_content: Option<lillux::PinnedDirectory>,
) -> Result<PreparedInvocation> {
    // Resolution is not project execution. The verified descriptor decides
    // whether this request belongs to the daemon-local read lane or the
    // ordinary live-project execution lane, so requiring write authority here
    // makes every source-safe service unsatisfiable by construction.
    let project_source = ryeos_executor::execution::project_source::ProjectSource::LiveFs;
    let checkout_id = format!(
        "ui-{}-{:08x}",
        lillux::time::timestamp_millis(),
        rand::random::<u32>()
    );
    let project_ctx = ryeos_executor::execution::project_source::resolve_project_context(
        state,
        &project_source,
        project_path,
        &ctx.fingerprint,
        &checkout_id,
        None,
    )
    .map_err(|e| HandlerError::Internal(format!("resolve project context: {e}")))?;

    let plan_ctx = ryeos_engine::contracts::PlanContext {
        requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: ctx.fingerprint.clone(),
                scopes: ctx.scopes.clone(),
            },
        ),
        project_context: ryeos_engine::contracts::ProjectContext::LocalPath {
            path: project_ctx.effective_path.clone(),
        },
        subject_resolution_authority: ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs,
        current_site_id: state.threads.site_id().to_string(),
        origin_site_id: state.threads.site_id().to_string(),
        execution_hints: Default::default(),
        scheduled_fire: None,
        validate_only: false,
    };

    let exec_ctx = ryeos_executor::executor::ExecutionContext {
        principal_fingerprint: ctx.fingerprint.clone(),
        caller_scopes: ctx.scopes.clone(),
        engine: project_ctx.request_engine.clone(),
        plan_ctx,
        requested_call: None,
    };

    Ok(PreparedInvocation {
        project: project_ctx,
        exec_ctx,
        project_content,
    })
}

async fn execute_read_only_service(
    item_ref: &str,
    params: Value,
    state: &AppState,
    prepared: PreparedInvocation,
    verified: ryeos_engine::contracts::VerifiedItem,
    local_handler_context: HandlerContext,
) -> Result<Value> {
    let schema_is_daemon_service = prepared
        .exec_ctx
        .engine
        .kinds
        .get(&verified.resolved.kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.terminator.as_ref())
        .is_some_and(|terminator| {
            matches!(
                terminator,
                ryeos_engine::kind_registry::TerminatorDecl::InProcess {
                    registry: ryeos_engine::kind_registry::InProcessRegistryKind::Services,
                    ..
                }
            )
        });
    if !schema_is_daemon_service {
        return Err(HandlerError::Forbidden(
            "source-safe dispatch is restricted to verified daemon services".into(),
        )
        .into());
    }
    let thread_profile = prepared
        .exec_ctx
        .engine
        .kinds
        .get(&verified.resolved.kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.thread_profile.as_ref())
        .map(|profile| profile.name.clone())
        .ok_or_else(|| {
            HandlerError::Internal(format!(
                "verified executable kind '{}' has no execution.thread_profile",
                verified.resolved.kind
            ))
        })?;

    // Read-only UI sources are in-process daemon queries. They need the live
    // project only as a resolution/input scope; manufacturing project
    // execution provenance for them both over-authorizes the handler and
    // wrongly demands `ryeos.write.project.live`. Availability, signed
    // required_caps, audit policy, and session-local context are still
    // enforced by the shared verified service executor.
    let result = ryeos_executor::executor::execute_service_verified(
        verified,
        item_ref,
        params,
        ryeos_executor::executor::ExecutionMode::Live,
        &prepared.exec_ctx,
        state,
        ryeos_executor::executor::ServiceRecordingContext {
            authority_source:
                ryeos_executor::executor::ServiceRecordingAuthoritySource::UnrecordedOnly,
            usage_subject: None,
            usage_subject_asserted_by: None,
        },
        None,
        None,
        Some(local_handler_context),
    )
    .await
    .map_err(map_dispatch_error)?;

    Ok(json!({
        "thread": {
            "thread_id": result.invocation_id,
            "recorded": result.recorded,
            "kind": thread_profile,
            "item_ref": item_ref,
            "status": "completed",
            "trust_class": format!("{:?}", result.trust_class),
            "effective_caps": result.effective_caps,
        },
        "result": result.value,
    }))
}

async fn execute_prepared_item_ref(
    item_ref: &str,
    params: Value,
    state: &AppState,
    prepared: PreparedInvocation,
    verified: ryeos_engine::contracts::VerifiedItem,
    authority_scopes: &[String],
    local_handler_context: HandlerContext,
) -> Result<Value> {
    let root_canonical = CanonicalRef::parse(item_ref)
        .map_err(|e| HandlerError::BadRequest(format!("invalid item ref: {e}")))?;

    let resolved_authority = ryeos_app::execution_policy::resolve_standard_local_live_authority(
        &prepared.project.effective_path,
        authority_scopes.to_vec(),
        &state.isolation,
    )
    .map_err(|error| HandlerError::Forbidden(error.to_string()))?;
    let provenance = ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
        prepared.project.effective_path.clone(),
        prepared.exec_ctx.engine.clone(),
        resolved_authority.project,
    )
    .map_err(|error| HandlerError::Internal(error.to_string()))?;

    let dispatch_req = ryeos_executor::dispatch::DispatchRequest {
        launch_mode: "wait",
        target_site_id: None,
        validate_only: false,
        params,
        ref_bindings: Default::default(),
        product_selections: Vec::new(),
        acting_principal: prepared.exec_ctx.principal_fingerprint.as_str(),
        project_path: &prepared.project.effective_path,
        provenance,
        lifecycle_authority: resolved_authority.lifecycle,
        launch_timings: None,
        original_root_kind: root_canonical.kind.as_str(),
        pre_minted_thread_id: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        previous_thread_id: None,
        root_admission: None,
        root_dispatch_evidence: None,
        parent_execution_context: None,
        effect_authority: None,
    };

    let result = ryeos_executor::dispatch::dispatch_verified_with_handler_context(
        item_ref,
        verified,
        local_handler_context,
        &dispatch_req,
        &prepared.exec_ctx,
        state,
    )
    .await
    .map_err(dispatch_error_to_handler)
    .map_err(Into::into);
    drop(dispatch_req);
    result
}

fn map_dispatch_error(error: anyhow::Error) -> HandlerError {
    let error = error
        .downcast::<ryeos_executor::dispatch_error::DispatchError>()
        .unwrap_or_else(ryeos_executor::dispatch_error::DispatchError::Internal);
    dispatch_error_to_handler(error)
}

fn dispatch_error_to_handler(error: ryeos_executor::dispatch_error::DispatchError) -> HandlerError {
    HandlerError::Structured {
        code: error.code().to_owned(),
        status: error.http_status().as_u16(),
        body: ryeos_executor::structured_error::dispatch_error_value(&error),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/invocations/dispatch",
    endpoint: "ui.invocations.dispatch",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    fn target(result_effect: Option<CompiledUiResultEffect>) -> CompiledUiTarget {
        use ryeos_api::surface_views::EffectiveUiItemIdentity;
        use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};

        CompiledUiTarget {
            identity: EffectiveUiItemIdentity {
                canonical_ref: "service:test/result".to_string(),
                effective_definition_digest: EffectiveDefinitionDigest::parse("44".repeat(32))
                    .expect("fixture digest"),
                effective_trust_class: TrustClass::TrustedBundle,
            },
            required_caps: Vec::new(),
            source_safe: false,
            dispatch_class: crate::compiled_binding::CompiledUiDispatchClass::Verified,
            result_effect,
            parameter_schema: BTreeMap::new(),
        }
    }

    fn session(user_principal_id: Option<String>) -> BrowserSession {
        let now = Instant::now();
        BrowserSession {
            session_id: "session-1".to_string(),
            created_at: now,
            expires_at: now + Duration::from_secs(60),
            principal_id: "fp:test".to_string(),
            granted_caps: vec!["ui.read".to_string()],
            user_principal_id,
            surface_attachment_id: "attachment-1".to_string(),
            attachments: Default::default(),
            next_attachment_generation: 1,
        }
    }

    #[test]
    fn invocation_context_uses_compiled_binding_principal() {
        let invocation_ctx = invocation_context_for_session(&session(Some(
            "fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        )));

        assert_eq!(invocation_ctx.fingerprint, "fp:test");
        assert!(invocation_ctx.verified);
        assert_eq!(invocation_ctx.scopes, vec!["ui.read".to_string()]);
    }

    #[test]
    fn invocation_context_never_uses_cookie_transport_as_authority() {
        let invocation_ctx = invocation_context_for_session(&session(None));

        assert_eq!(invocation_ctx.fingerprint, "fp:test");
        assert!(invocation_ctx.verified);
        assert_eq!(invocation_ctx.scopes, vec!["ui.read".to_string()]);
    }

    #[test]
    fn undeclared_service_result_cannot_forge_a_ui_transition() {
        let mut result = json!({
            "value": 7,
            "ui_transition": {
                "kind": "forged_transition",
                "session_id": "forged",
                "launch_url": "/ui/launch/forged"
            }
        });

        retain_declared_result_effect(&target(None), &mut result).expect("sanitize result");

        assert_eq!(result, json!({"value": 7}));
    }

    #[test]
    fn declared_attachment_effect_is_typed_and_retained() {
        let transition = json!({
            "kind": "admit_binding_attachment",
            "attachment": {
                "binding_attachment_id": "attachment-2",
                "binding_generation": 2,
                "binding_digest": "11",
                "surface_ref": "surface:test/base",
                "surface_generation": "22",
                "effective_surface": {},
                "project_path": null,
                "posture": "observation_only",
                "binding_request_bounds": {"max_request_bytes": 1024, "max_input_bytes": 512}
            }
        });
        let mut result = json!({"ui_transition": transition.clone()});

        retain_declared_result_effect(
            &target(Some(CompiledUiResultEffect::AdmitBindingAttachment)),
            &mut result,
        )
        .expect("retain signed result effect");

        assert_eq!(result["ui_transition"], transition);
    }

    #[test]
    fn declared_attachment_effect_rejects_missing_descriptor() {
        let mut result = json!({
            "ui_transition": {
                "kind": "admit_binding_attachment"
            }
        });

        let error = retain_declared_result_effect(
            &target(Some(CompiledUiResultEffect::AdmitBindingAttachment)),
            &mut result,
        )
        .expect_err("missing descriptor must fail closed");

        assert!(error.to_string().contains("invalid typed UI transition"));
    }

    #[test]
    fn declared_attachment_effect_rejects_unknown_fields() {
        let mut result = json!({
            "ui_transition": {
                "kind": "admit_binding_attachment",
                "attachment": {},
                "session_id": "forged"
            }
        });

        assert!(
            retain_declared_result_effect(
                &target(Some(CompiledUiResultEffect::AdmitBindingAttachment)),
                &mut result,
            )
            .is_err()
        );
    }

    #[test]
    fn binding_request_cannot_decode_an_arbitrary_target() {
        let request = serde_json::from_value::<UiBindingRequest>(serde_json::json!({
            "binding_attachment_id": "attachment-1",
            "binding_generation": 1,
            "binding_digest": "11",
            "coordinate": { "kind": "source", "view_ref": "view:x", "channel": "default" },
            "payload": { "kind": "source_parameters", "params": {} },
            "target": { "kind": "ref", "ref": "service:commands/submit" }
        }));
        assert!(request.is_err());
    }

    #[test]
    fn verified_dispatch_uses_the_durable_invocation_context() {
        let browser_context = HandlerContext::new("session:session-1".to_string(), vec![], false);
        let invocation_context = HandlerContext::new(
            "fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            vec!["ui.read".to_string()],
            true,
        );

        let selected = select_trusted_handler_context(
            UiDispatchMode::Verified,
            &browser_context,
            &invocation_context,
        );

        assert_eq!(selected.fingerprint, invocation_context.fingerprint);
        assert_eq!(selected.scopes, invocation_context.scopes);
        assert!(selected.verified);
    }

    #[test]
    fn session_local_dispatch_keeps_the_browser_session_context() {
        let browser_context = HandlerContext::new("session:session-1".to_string(), vec![], false);
        let invocation_context = HandlerContext::new(
            "fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            vec!["ui.read".to_string()],
            true,
        );

        let selected = select_trusted_handler_context(
            UiDispatchMode::SessionLocal,
            &browser_context,
            &invocation_context,
        );

        assert_eq!(selected.fingerprint, browser_context.fingerprint);
        assert_eq!(selected.scopes, browser_context.scopes);
        assert!(!selected.verified);
    }

    #[test]
    fn optional_project_marker_preserves_projectless_command_dispatch() {
        assert_eq!(
            resolve_session_markers(
                &json!({"project_path": "@session:project_root_or_null"}),
                None,
            )
            .unwrap(),
            json!({"project_path": null})
        );
        assert_eq!(
            resolve_session_markers(
                &json!({"project_path": "@session:project_root_or_null"}),
                Some("/project"),
            )
            .unwrap(),
            json!({"project_path": "/project"})
        );
    }

    #[test]
    fn required_project_marker_still_refuses_projectless_sessions() {
        assert!(resolve_session_markers(&json!("@session:project_root"), None).is_err());
    }
}
