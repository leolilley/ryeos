//! Subprocess terminator execution for managed runtimes, callback-free streams,
//! and ordinary tool subprocesses.

use super::*;

// ── Unified subprocess terminator ─────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManagedProtocolRoute {
    CallbackRuntime,
    FramedStreaming,
}

/// Select the existing executor-plan surface from wire mechanics, never a
/// kind or protocol name. Managed callback runtimes own launch envelopes;
/// callback-free streams consume the ordinary signed subprocess plan.
pub(super) fn uses_direct_subprocess_plan(
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
) -> bool {
    use ryeos_engine::protocol_vocabulary::{CallbackChannel, LifecycleMode};
    protocol.descriptor.lifecycle.mode == LifecycleMode::DetachedOk
        || protocol.descriptor.callback_channel == CallbackChannel::None
}

/// Enforce the one ordinary-subprocess wire contract everywhere admission can
/// happen. Keeping this check shared prevents accepted preflight from minting
/// a thread that the runner later rejects for protocol shape.
pub(crate) fn validate_ordinary_protocol_contract(
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
    kind: &str,
) -> Result<(), DispatchError> {
    use ryeos_engine::protocol_vocabulary::{LifecycleMode, StdinShape, StdoutMode, StdoutShape};

    let terminal = protocol.descriptor.lifecycle.mode == LifecycleMode::DetachedOk
        && protocol.descriptor.stdout.shape == StdoutShape::OpaqueBytes
        && protocol.descriptor.stdout.mode == StdoutMode::Terminal;
    let streaming = protocol.descriptor.lifecycle.mode == LifecycleMode::Managed
        && protocol.descriptor.stdout.shape == StdoutShape::StreamingChunks
        && protocol.descriptor.stdout.mode == StdoutMode::Streaming
        && protocol.descriptor.callback_channel
            == ryeos_engine::protocol_vocabulary::CallbackChannel::None
        && !protocol.descriptor.capabilities.allows_detached;
    if protocol.descriptor.stdin.shape != StdinShape::Opaque || !(terminal || streaming) {
        return Err(DispatchError::SchemaMisconfigured {
            kind: kind.to_string(),
            detail: format!(
                "ordinary subprocess protocol '{}' has unsupported wire contract: expected plan-owned opaque stdin and either terminal opaque_bytes/detached_ok or callback-free non-detachable streaming_chunks/managed; got {:?} stdin, {:?}/{:?} stdout, and {:?} lifecycle",
                protocol.canonical_ref,
                protocol.descriptor.stdin.shape,
                protocol.descriptor.stdout.shape,
                protocol.descriptor.stdout.mode,
                protocol.descriptor.lifecycle.mode,
            ),
        });
    }
    if streaming {
        validate_callback_free_env(protocol, kind)?;
    }
    Ok(())
}

fn validate_callback_free_env(
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
    kind: &str,
) -> Result<(), DispatchError> {
    use ryeos_engine::protocol_vocabulary::EnvInjectionSource;
    if let Some(injection) = protocol.descriptor.env_injections.iter().find(|injection| {
        matches!(
            injection.source,
            EnvInjectionSource::CallbackSocketPath
                | EnvInjectionSource::CallbackToken
                | EnvInjectionSource::ThreadAuthToken
        )
    }) {
        return Err(DispatchError::SchemaMisconfigured {
            kind: kind.to_owned(),
            detail: format!(
                "callback-free protocol '{}' requests unavailable env source {:?}",
                protocol.canonical_ref, injection.source
            ),
        });
    }
    Ok(())
}

/// Framed output promises durable replay through the existing event braid.
/// There is no ephemeral streaming-result surface; do not let output events
/// bypass a signed digest-only result contract.
pub(crate) fn validate_direct_result_retention(
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
    retention: ryeos_engine::history_policy::ThreadResultRetention,
    kind: &str,
) -> Result<(), DispatchError> {
    if protocol.descriptor.stdout.shape
        == ryeos_engine::protocol_vocabulary::StdoutShape::StreamingChunks
        && retention != ryeos_engine::history_policy::ThreadResultRetention::Full
    {
        return Err(DispatchError::SchemaMisconfigured {
            kind: kind.to_owned(),
            detail: "framed stdout requires full result retention; digest-only streaming delivery is not supported".into(),
        });
    }
    Ok(())
}

/// Classify a verified managed protocol once for both preflight and live
/// dispatch. A callback-free managed subprocess is daemon-owned only when its
/// descriptor carries the framed streaming contract; other terminal shapes
/// (for example a local TTY client launcher) belong to a different surface.
pub(super) fn classify_managed_protocol(
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
    kind: &str,
) -> Result<ManagedProtocolRoute, DispatchError> {
    use ryeos_engine::protocol_vocabulary::{
        CallbackChannel, LifecycleMode, StdinShape, StdoutMode, StdoutShape,
    };

    if protocol.descriptor.callback_channel != CallbackChannel::None {
        if protocol.descriptor.stdin.shape != StdinShape::LaunchEnvelope
            || protocol.descriptor.stdout.shape != StdoutShape::RuntimeResult
            || protocol.descriptor.stdout.mode != StdoutMode::Terminal
            || protocol.descriptor.lifecycle.mode != LifecycleMode::Managed
        {
            return Err(DispatchError::SchemaMisconfigured {
                kind: kind.to_string(),
                detail: format!(
                    "managed callback protocol '{}' has unsupported wire contract: expected launch_envelope stdin, terminal runtime_result stdout, and managed lifecycle; got {:?} stdin, {:?}/{:?} stdout, and {:?} lifecycle",
                    protocol.canonical_ref,
                    protocol.descriptor.stdin.shape,
                    protocol.descriptor.stdout.shape,
                    protocol.descriptor.stdout.mode,
                    protocol.descriptor.lifecycle.mode,
                ),
            });
        }
        return Ok(ManagedProtocolRoute::CallbackRuntime);
    }
    if protocol.descriptor.lifecycle.mode != LifecycleMode::Managed {
        return Err(DispatchError::SchemaMisconfigured {
            kind: kind.to_owned(),
            detail: "managed protocol classification requires managed lifecycle".into(),
        });
    }
    validate_ordinary_protocol_contract(protocol, kind)?;
    Ok(ManagedProtocolRoute::FramedStreaming)
}

/// Validate the separate wire used when a kind's method dispatcher spawns its
/// selected runtime. A method call is managed and callback-capable, but it does
/// not consume or emit the normal runtime launch/result envelopes.
pub(super) fn validate_method_protocol_contract(
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
    kind: &str,
) -> Result<(), DispatchError> {
    ryeos_engine::protocols::validate_method_runtime_protocol(&protocol.descriptor).map_err(
        |reason| DispatchError::SchemaMisconfigured {
            kind: kind.to_string(),
            detail: format!("method protocol '{}' {reason}", protocol.canonical_ref),
        },
    )
}

pub(crate) async fn dispatch_subprocess(
    sctx: SubprocessDispatchContext<'_>,
) -> Result<Value, DispatchError> {
    let SubprocessDispatchContext {
        current_ref,
        thread_profile,
        verified: hop_verified,
        request,
        ctx,
        state,
        handler_context,
        role,
        root_subject,
        hop_runtime,
        launch_handoff,
    } = sctx;
    let schema = ctx.engine.kinds.get(&current_ref.kind).ok_or_else(|| {
        let mut available: Vec<String> = ctx.engine.kinds.kinds().map(|k| k.to_string()).collect();
        available.sort();
        DispatchError::SchemaMisconfigured {
            kind: current_ref.kind.clone(),
            detail: format!(
                "no kind schema registered for ref '{current_ref}'; registered kinds: [{}]",
                available.join(", ")
            ),
        }
    })?;
    let exec = schema
        .execution()
        .ok_or_else(|| DispatchError::NotRootExecutable {
            kind: current_ref.kind.clone(),
            detail: "schema has no `execution:` block".into(),
        })?;
    let terminator =
        exec.terminator
            .as_ref()
            .ok_or_else(|| DispatchError::SchemaMisconfigured {
                kind: current_ref.kind.clone(),
                detail: "dispatch_subprocess called on a schema with no terminator".into(),
            })?;
    let protocol_ref = match terminator {
        TerminatorDecl::Subprocess { protocol } => {
            let verified = hop_verified.ok_or_else(|| DispatchError::SchemaMisconfigured {
                kind: current_ref.kind.clone(),
                detail: "subprocess protocol selection requires a verified item".into(),
            })?;
            let effective = ctx
                .engine
                .effective_item(ryeos_engine::engine::EffectiveItemRequest {
                    item_ref: verified.resolved.canonical_ref.clone(),
                    expected_kind: Some(current_ref.kind.clone()),
                    project_root: verified.resolved.materialized_project_root.clone(),
                    subject_resolution_authority: verified
                        .resolved
                        .subject_resolution_authority
                        .clone(),
                })
                .map_err(|error| DispatchError::SchemaMisconfigured {
                    kind: current_ref.kind.clone(),
                    detail: format!("resolve subprocess protocol selection: {error}"),
                })?;
            if effective.source.content_hash != verified.resolved.content_hash {
                return Err(DispatchError::SchemaMisconfigured {
                    kind: current_ref.kind.clone(),
                    detail:
                        "effective subprocess protocol selection changed the verified root bytes"
                            .into(),
                });
            }
            protocol
                .resolve(&effective.composed_value)
                .map_err(|detail| DispatchError::SchemaMisconfigured {
                    kind: current_ref.kind.clone(),
                    detail,
                })?
        }
        TerminatorDecl::InProcess { .. } => {
            return Err(DispatchError::SchemaMisconfigured {
                kind: current_ref.kind.clone(),
                detail: "dispatch_subprocess called on schema declaring InProcess terminator, not Subprocess".into(),
            });
        }
    };

    enforce_runtime_target_caps(role, &ctx.caller_scopes)?;

    let protocol = ctx
        .engine
        .protocols
        .require(&protocol_ref)
        .map_err(|_| DispatchError::ProtocolNotRegistered(protocol_ref.clone()))?;

    check_dispatch_capabilities(&protocol.descriptor.capabilities, request)?;

    use ryeos_engine::protocol_vocabulary::StdoutMode;
    if protocol.descriptor.stdout.mode == StdoutMode::Streaming && request.launch_mode == "detached"
    {
        return Err(DispatchError::StreamingNotDetachable);
    }

    match uses_direct_subprocess_plan(protocol) {
        false => {
            // Keep the managed and ordinary subprocess futures behind an
            // allocation boundary. Both leaves carry substantial launch
            // state; embedding both branch futures in this routing future can
            // exhaust an unoptimized Tokio worker stack before the selected
            // leaf reaches its first suspension point.
            Box::pin(dispatch_managed_subprocess(
                SubprocessDispatchContext {
                    current_ref,
                    thread_profile,
                    verified: hop_verified,
                    request,
                    ctx,
                    state,
                    handler_context,
                    role,
                    root_subject,
                    hop_runtime,
                    launch_handoff,
                },
                protocol,
            ))
            .await
        }
        true => {
            validate_ordinary_protocol_contract(protocol, &current_ref.kind)?;
            Box::pin(dispatch_tool_subprocess(
                current_ref,
                thread_profile,
                hop_verified,
                request,
                ctx,
                state,
                handler_context,
                launch_handoff,
                protocol,
            ))
            .await
        }
    }
}

async fn dispatch_managed_subprocess(
    sctx: SubprocessDispatchContext<'_>,
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
) -> Result<Value, DispatchError> {
    let SubprocessDispatchContext {
        current_ref: canonical_ref,
        verified: hop_verified,
        thread_profile: hop_thread_profile,
        hop_runtime: _hop_runtime,
        root_subject,
        request,
        ctx,
        state,
        handler_context,
        role,
        launch_handoff,
    } = sctx;

    if classify_managed_protocol(protocol, &canonical_ref.kind)?
        == ManagedProtocolRoute::FramedStreaming
    {
        return Err(DispatchError::SchemaMisconfigured {
            kind: canonical_ref.kind.clone(),
            detail: "callback-free streams require the ordinary executor-plan route".into(),
        });
    }

    let runtime_ref = canonical_ref.to_string();

    let verified_runtime = match role {
        SubprocessRole::RuntimeTarget { verified_runtime } => {
            Some(verified_runtime.as_ref().clone())
        }
        SubprocessRole::Regular => ctx.engine.runtimes.lookup_by_ref(canonical_ref).cloned(),
    };

    let verified_runtime = verified_runtime.ok_or_else(|| {
        let mut available: Vec<String> = ctx
            .engine
            .runtimes
            .all()
            .map(|r| r.canonical_ref.to_string())
            .collect();
        available.sort();
        DispatchError::SchemaMisconfigured {
            kind: canonical_ref.kind.clone(),
            detail: format!(
                "runtime '{runtime_ref}' has no registry entry; registered runtimes: [{}]",
                available.join(", ")
            ),
        }
    })?;

    let params = request.params.clone();
    let acting_principal = request.acting_principal;
    let project_path: &Path = request.project_path;

    if request.original_root_kind == ROOT_KIND_RUNTIME {
        enforce_runtime_caps(
            &state.authorizer,
            &runtime_ref,
            &verified_runtime.yaml.required_caps,
            &ctx.caller_scopes,
        )?;
    }

    let prepared = prepare_managed_launch(
        &verified_runtime,
        root_subject,
        hop_thread_profile,
        hop_verified,
        &runtime_ref,
        ctx,
        request,
        state.node_history_policy()?,
    )?;

    if request.validate_only {
        let external_content = validate_managed_effective_program(&prepared, request, ctx, state)?;
        let root_ready = external_content
            .as_ref()
            .is_none_or(|preview| preview.ready_for_admission);
        let root_admission = prepared.resolved.root_admission.as_ref().ok_or_else(|| {
            DispatchError::Internal(anyhow::anyhow!(
                "managed static validation has no exact root admission"
            ))
        })?;
        let applicability = crate::dispatch::LaunchContractApplicability::ManagedEnvelope {
            runtime: Box::new(verified_runtime.clone()),
        };
        let launch_contract = crate::dispatch::prepare_admitted_launch_contract(
            &applicability,
            root_admission,
            &request.ref_bindings,
            &request.lifecycle_authority,
            &request.provenance,
            ctx,
            state,
        )
        .await?
        .ok_or_else(|| {
            DispatchError::Internal(anyhow::anyhow!(
                "managed static validation produced no prepared launch contract"
            ))
        })?;
        let dependencies = crate::execution::persistent_session::preview_prepared_dependencies(
            state,
            &ctx.engine,
            &launch_contract,
            root_admission.resolution_subject_authority(),
            handler_context.as_ref(),
            &ctx.engine.resolution_roots(
                root_admission
                    .resolution_workspace()
                    .map(std::path::Path::to_path_buf),
            ),
        )
        .map_err(DispatchError::Internal)?;
        let dependencies_ready = dependencies.admission_ready;
        let mut credential_names = root_admission
            .verified_subject()
            .resolved
            .metadata
            .required_secrets
            .clone();
        credential_names.extend(
            launch_contract
                .required_secrets
                .iter()
                .map(|requirement| requirement.name.clone()),
        );
        credential_names.sort();
        credential_names.dedup();
        let credentials_ready = credential_names.is_empty();
        let credential_readiness = if credentials_ready {
            "required_none"
        } else {
            "not_checked"
        };
        let project_result_ready =
            crate::execution::launch_preparation::project_result_requirement_satisfied(
                &launch_contract,
                &request.provenance,
            );
        let output_partition_required =
            crate::execution::workspace_outputs::admission::requires_output_partition(
                &launch_contract,
            )
            .map_err(DispatchError::Internal)?;
        let output_partition = if output_partition_required && project_result_ready {
            crate::execution::workspace_outputs::admission::derive_initial_partition(
                state,
                &ctx.engine,
                root_admission.resolution_output(),
                &launch_contract,
                request
                    .provenance
                    .project_authority()
                    .operational_snapshot_projection(),
            )
            .map_err(DispatchError::Internal)?
        } else {
            None
        };
        let output_partition_ready = !output_partition_required
            || (output_partition.is_some() && !state.isolation.is_enforced());
        let runtime_preparation_ready = dependencies_ready
            && credentials_ready
            && project_result_ready
            && output_partition_ready;
        let admission_ready = root_ready && runtime_preparation_ready;
        return Ok(json!({
            "validated": true,
            "admission_ready": admission_ready,
            "item_ref": &prepared.resolved.item_ref,
            "kind": &prepared.resolved.resolved_item.kind,
            "executor_ref": &prepared.executor_ref,
            "external_content": external_content,
            "runtime_preparation": {
                "project_result": {
                    "requirement": launch_contract.project_result_requirement,
                    "satisfied": project_result_ready,
                },
                "workspace_outputs": {
                    "required": output_partition_required,
                    "admission_ready": output_partition_ready,
                    "partition": output_partition,
                },
                "runtime_ref": verified_runtime.canonical_ref.to_string(),
                "binding_records": dependencies.binding_records,
                "execution_dependencies": dependencies.execution_dependencies,
                "content_dependencies": dependencies.content_dependencies,
                "environment_contributions": dependencies.environment_contributions,
                "credential_readiness": {
                    "status": credential_readiness,
                    "required_count": credential_names.len(),
                },
                "admission_ready": runtime_preparation_ready,
            },
        }));
    }

    // Runtime callback caps (bundle-events / runtime-vault) are minted inside
    // `build_and_launch` from the *composed* `requires` block — after the
    // extends-chain composer has narrowed a child directive against its parent.
    // Minting here (pre-composition) would miss that narrowing.
    let result = launch::build_and_launch(launch::BuildAndLaunchParams {
        state,
        lifecycle_authority: request.lifecycle_authority,
        launch_timings: request.launch_timings.clone(),
        // The serving runtime's canonical ref, captured so a continuation
        // successor reattaches the same runtime identity (not just the kind's
        // current default).
        runtime_ref: Some(&runtime_ref),
        acting_principal,
        handler_context: handler_context.as_ref(),
        resolved: &prepared.resolved,
        project_path,
        provenance: &request.provenance,
        parameters: &params,
        metadata_required_secrets: &prepared.resolved.resolved_item.metadata.required_secrets,
        pre_minted_thread_id: request.pre_minted_thread_id.as_deref(),
        effect_authority: request.effect_authority.as_ref(),
        previous_thread_id: request.previous_thread_id.as_deref(),
        parent_execution_context: request.parent_execution_context.as_ref(),
        // Fresh launches and operator follow-ups inject their inputs as the
        // opening stimulus; only an autonomous machine continuation suppresses it.
        suppress_stimulus: false,
        // Fresh resolution: use the freshly-resolved caps (no captured set to pin).
        capability_policy: crate::execution::launch::CapabilityPolicy::AdmissionDefault,
        // Fresh launch: cold start, no checkpoint resume.
        checkpoint_resume_mode: crate::execution::launch::CheckpointResumeMode::None,
        pre_pinned_checkpoint_authority: None,
        rearm_native_resume_budget_after_attach: false,
        launch_handoff,
    })
    .await
    .map_err(|error| error.into_dispatch_error(&prepared.executor_ref))?;

    let mut response = json!({
        "thread": result.thread,
        "result": result.result,
        "result_project_snapshot_hash": result.result_project_snapshot_hash,
    });
    if let Some(dispatch) = result.dispatch {
        response["dispatch"] = serde_json::to_value(dispatch)
            .map_err(|error| DispatchError::Internal(error.into()))?;
    }
    Ok(response)
}

fn validate_managed_effective_program(
    prepared: &crate::dispatch::PreparedManagedLaunch,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<
    Option<ryeos_app::external_content_admission::ExternalContentValidationPreview>,
    DispatchError,
> {
    let admission = prepared.resolved.root_admission.as_ref().ok_or_else(|| {
        DispatchError::Internal(anyhow::anyhow!(
            "managed static validation has no exact root admission"
        ))
    })?;
    admission
        .ensure_matches_provenance(&request.provenance)
        .map_err(DispatchError::Internal)?;
    let engine = admission.request_engine();
    if !std::sync::Arc::ptr_eq(engine, &ctx.engine) {
        return Err(DispatchError::Internal(anyhow::anyhow!(
            "managed static validation engine differs from root admission"
        )));
    }
    let subject_authority = admission.resolution_subject_authority().clone();
    let resolution_project_root = (!matches!(
        subject_authority,
        ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
    ))
    .then(|| {
        admission.resolution_workspace().ok_or_else(|| {
            DispatchError::Internal(anyhow::anyhow!(
                "managed static validation has no admitted project workspace"
            ))
        })
    })
    .transpose()?;
    let roots = engine.resolution_roots(resolution_project_root.map(Path::to_path_buf));
    let request_snapshot = match admission.admitted_request_snapshot() {
        Some(admitted) => engine.effective_request_snapshot_under_admitted_authority(
            resolution_project_root.ok_or_else(|| {
                DispatchError::Internal(anyhow::anyhow!(
                    "admitted static validation snapshot has no project workspace"
                ))
            })?,
            admitted,
        ),
        None if subject_authority.operational_generation().is_some() => {
            return Err(DispatchError::Internal(anyhow::anyhow!(
                "content-addressed static validation has no admitted request snapshot"
            )));
        }
        None => engine
            .effective_request_snapshot(resolution_project_root, &subject_authority)
            .map(std::sync::Arc::new),
    }
    .map_err(|error| {
        DispatchError::Internal(anyhow::anyhow!(
            "managed static validation request authority: {error}"
        ))
    })?;
    let resolution = admission.resolution_output().clone();
    let declared_caps = launch::derive_effective_caps(&resolution.composed);
    ryeos_bundle::runtime_authority::reject_disallowed_composed_grants(&declared_caps).map_err(
        |error| DispatchError::CapabilityRejected {
            reason: error.to_string(),
        },
    )?;
    let runtime_caps = crate::dispatch::mint_runtime_capability_caps(
        resolution.composed.composed.get("requires"),
        &prepared.resolved.resolved_item,
        resolution.effective_trust_class,
        engine,
    )
    .map_err(|reason| DispatchError::CapabilityRejected { reason })?;
    let effective_caps = declared_caps
        .into_iter()
        .chain(runtime_caps)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let materialization = admission
        .resolution_materialization_binding()
        .map_err(DispatchError::Internal)?;
    crate::execution::effective_program_projection::validate_admitted_effective_program(
        state,
        engine,
        &prepared.resolved.resolved_item.kind,
        resolution,
        &effective_caps,
        &roots,
        &request_snapshot.parser_dispatcher,
        &request_snapshot.trust_store,
        Some(&materialization),
    )
}

async fn dispatch_tool_subprocess(
    current_ref: &CanonicalRef,
    thread_profile: &str,
    verified: Option<&VerifiedItem>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    handler_context: Option<ryeos_app::handler_context::HandlerContext>,
    launch_handoff: Option<&crate::execution::launch::LaunchHandoff>,
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
) -> Result<Value, DispatchError> {
    let item_ref = current_ref.to_string();

    require_terminal_executor_id(verified, &item_ref)?;

    let admitted_resolution = if request.previous_thread_id.is_none() {
        request
            .root_admission
            .as_ref()
            .map(|admission| {
                admission
                    .ensure_matches_provenance(&request.provenance)
                    .map_err(DispatchError::Internal)?;
                admission
                    .execution_request(
                        ryeos_app::thread_lifecycle::RootExecutionRoute::RootExecutorChain,
                        request.launch_mode.to_owned(),
                        request.params.clone(),
                    )
                    .map_err(DispatchError::Internal)
            })
            .transpose()?
    } else {
        None
    };
    let resolution_engine = std::sync::Arc::clone(&ctx.engine);
    let resolution_item_ref = item_ref.clone();
    let resolution_plan_context = ctx.plan_ctx.clone();
    let resolution_project_binding =
        ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
            &ctx.engine,
            &ctx.plan_ctx,
            &request.provenance,
        )?;
    let node_history_policy = std::sync::Arc::new(state.node_history_policy()?.clone());
    let resolution_ref_bindings = request.ref_bindings.clone();
    let resolution_product_selections = request.product_selections.clone();
    let resolution_launch_mode = request.launch_mode.to_owned();
    let resolution_parameters = request.params.clone();
    let resolution_usage_subject = request.usage_subject.clone();
    let resolution_usage_subject_asserted_by = request.usage_subject_asserted_by.clone();
    let creates_chain_root =
        request.previous_thread_id.is_none() && request.root_admission.is_none();
    let mut resolved = match admitted_resolution {
        Some(resolved) => resolved,
        None => tokio::task::spawn_blocking(move || {
            ryeos_app::thread_lifecycle::resolve_root_execution(
                ryeos_app::thread_lifecycle::ResolveRootExecutionParams {
                    engine: &resolution_engine,
                    plan_context: resolution_plan_context,
                    project_binding: resolution_project_binding,
                    node_history_policy: &node_history_policy,
                    item_ref: &resolution_item_ref,
                    ref_bindings: resolution_ref_bindings,
                    product_selections: resolution_product_selections,
                    launch_mode: &resolution_launch_mode,
                    parameters: resolution_parameters,
                    usage_subject: resolution_usage_subject,
                    usage_subject_asserted_by: resolution_usage_subject_asserted_by,
                    creates_chain_root,
                },
            )
        })
        .await
        .map_err(|error| {
            DispatchError::Internal(anyhow::anyhow!(
                "root execution resolution blocking worker failed: {error}"
            ))
        })?
        .map_err(DispatchError::Internal)?,
    };

    if protocol.descriptor.stdout.shape
        == ryeos_engine::protocol_vocabulary::StdoutShape::StreamingChunks
    {
        validate_direct_result_retention(
            protocol,
            resolved
                .root_admission
                .as_ref()
                .ok_or_else(|| {
                    DispatchError::Internal(anyhow::anyhow!(
                        "direct execution lacks root admission"
                    ))
                })?
                .resolved_result_policy()
                .retention,
            &current_ref.kind,
        )?;
    }
    resolved.kind = thread_profile.to_string();
    // Data-driven execution routine: walk the wrapper's executor chain to its
    // terminal and branch on the terminal's typed `terminal_executor.kind` —
    // never on the alias name or the terminal ref. Every terminal must declare
    // `terminal_executor`; a missing/invalid descriptor is a hard error (no
    // silent subprocess fallback).
    let terminal = super::resolve_terminal_executor_for_subject(
        &ctx.engine,
        &resolved.resolved_item,
        &resolved.executor_ref,
        resolved
            .root_admission
            .as_ref()
            .map_or(Some(request.project_path), |admission| {
                admission.resolution_workspace()
            }),
        resolved
            .root_admission
            .as_ref()
            .and_then(|admission| admission.admitted_request_snapshot())
            .map(AsRef::as_ref),
    )
    .map_err(|e| DispatchError::SchemaMisconfigured {
        kind: current_ref.kind.clone(),
        detail: format!("failed to resolve executor-chain terminal for '{item_ref}': {e}"),
    })?;
    if terminal.kind == ryeos_engine::plan_builder::TerminalExecutorKind::MethodDispatch {
        if protocol.descriptor.stdout.mode
            == ryeos_engine::protocol_vocabulary::StdoutMode::Streaming
        {
            return Err(DispatchError::SchemaMisconfigured {
                kind: current_ref.kind.clone(),
                detail: "framed subprocess stdout cannot be supplied by a method-dispatch terminal"
                    .into(),
            });
        }
        return Box::pin(dispatch_via_method_executor(
            &resolved,
            request,
            ctx,
            state,
            handler_context,
            launch_handoff,
        ))
        .await;
    }

    // A method-dispatch wrapper carries the target's admission through the
    // recursive request above. Only a concrete subprocess root may attach that
    // admission to the locally resolved subject.
    if request.previous_thread_id.is_none()
        && let Some(admission) = request.root_admission.as_ref()
    {
        let local_subject = verified.ok_or_else(|| {
            DispatchError::InvalidRef(
                item_ref.clone(),
                "terminal subprocess root did not resolve and verify".to_string(),
            )
        })?;
        admission
            .ensure_matches_subject(&ctx.engine, local_subject, thread_profile)
            .map_err(DispatchError::Internal)?;
        resolved.plan_context = admission.plan_context().clone();
        resolved.root_admission = Some(admission.clone());
        admission
            .ensure_matches_request(&resolved)
            .map_err(DispatchError::Internal)?;
    }

    if let Some(target) = request.target_site_id {
        resolved.target_site_id = Some(target.to_string());
    }

    if request.validate_only {
        let engine = ctx.engine.clone();
        let validation_engine = engine.clone();
        let resolved_clone = resolved.clone();
        let workspace_lifeline = request.provenance.workspace_lifeline();
        let validated = tokio::task::spawn_blocking(move || {
            let _workspace_lifeline = workspace_lifeline;
            ryeos_app::thread_lifecycle::validate_item(&validation_engine, &resolved_clone)
        })
        .await
        .map_err(|e| DispatchError::SubprocessRunFailed {
            item_ref: resolved.item_ref.clone(),
            detail: format!("validate_only join failure: {e}"),
        })??;

        let source_project_root = resolved
            .root_admission
            .as_ref()
            .map_or(Some(request.project_path), |admission| {
                admission.resolution_workspace()
            });
        let source =
            validate_direct_source_closure(state, &engine, &resolved, source_project_root)?;
        let admission_ready = source
            .as_ref()
            .is_none_or(|preview| preview.ready_for_admission);

        return Ok(json!({
            "validated": true,
            "admission_ready": admission_ready,
            "item_ref": resolved.item_ref,
            "kind": resolved.kind,
            "executor_ref": resolved.executor_ref,
            "trust_class": validated.trust_class,
            "plan_id": validated.plan_id,
            "source": source,
        }));
    }

    let parent_thread_id = request
        .parent_execution_context
        .as_ref()
        .map(|parent| parent.parent_thread_id.clone());
    let finalized_direct = crate::execution::runner::finalize_direct_effective_program(
        state,
        &resolved,
        &request.provenance,
        parent_thread_id.as_deref(),
        handler_context.as_ref(),
    )
    .map_err(DispatchError::Internal)?;

    let item_ref_for_error = resolved.item_ref.clone();
    let effective_caps =
        derive_manifest_runtime_caps(&resolved.resolved_item, &resolved.item_ref, ctx)?;

    let required_caps =
        ryeos_app::service_registry::extract_required_caps(&resolved.resolved_item.metadata.extra);
    if !required_caps.is_empty() {
        enforce_runtime_caps(
            &state.authorizer,
            &item_ref_for_error,
            &required_caps,
            &ctx.caller_scopes,
        )?;
    }

    let dotenv_dirs =
        ryeos_app::vault::dotenv_search_dirs(Some(request.provenance.original_project_path()));
    let vault_bindings = ryeos_app::vault::read_required_secrets(
        state.vault.as_ref(),
        request.acting_principal,
        &resolved.resolved_item.metadata.required_secrets,
        &dotenv_dirs,
    )
    .map_err(|e| match e {
        ryeos_app::vault::VaultReadError::MissingSecrets { names, .. } => {
            let env_var = names
                .first()
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            DispatchError::RequiredSecretMissing {
                item_ref: item_ref_for_error.clone(),
                env_var: env_var.clone(),
                source_kind: "declared".to_string(),
                source_name: "item metadata".to_string(),
                remediation: crate::dispatch_error::required_secret_remediation(&env_var),
            }
        }
        error @ ryeos_app::vault::VaultReadError::AuthorityViolation(_) => {
            DispatchError::Internal(anyhow::anyhow!("vault read refused: {error}"))
        }
        ryeos_app::vault::VaultReadError::Internal(e) => {
            DispatchError::Internal(anyhow::anyhow!("vault read failed: {e}"))
        }
    })?;

    let params = crate::execution::runner::ExecutionParams {
        resolved,
        acting_principal: request.acting_principal.to_string(),
        handler_context,
        vault_bindings,
        parameters: request.params.clone(),
        pre_minted_thread_id: request.pre_minted_thread_id.clone(),
        effective_caps,
        provenance: request.provenance.clone(),
        lifecycle_authority: request.lifecycle_authority,
        // Fresh dispatch: no captured runtime ref. The thread's runtime identity
        // is captured in launch metadata; resume reads it back from there.
        runtime_ref: None,
        parent_thread_id,
        effect_authority: request.effect_authority.clone(),
        finalized_direct: Some(finalized_direct),
    };

    if request.launch_mode == "detached" {
        // `run_detached` and `run_and_wait` are independent, large lifecycle
        // futures. Boxing the selected leaf prevents this tool router from
        // carrying both state machines inline on every poll.
        let result = Box::pin(crate::execution::runner::run_detached(
            state.clone(),
            params,
            launch_handoff,
        ))
        .await
        .map_err(|error| map_runner_error(item_ref_for_error.clone(), error))?;
        Ok(json!({
            "thread": result.running_thread,
            "detached": true,
        }))
    } else {
        let outcome = Box::pin(crate::execution::runner::run_and_wait(
            state.clone(),
            params,
            launch_handoff,
        ))
        .await
        .map_err(|error| map_runner_error(item_ref_for_error, error))?;
        match outcome {
            crate::execution::runner::WaitOutcome::Executed(result) => {
                let mut envelope = json!({
                    "thread": result.finalized_thread,
                    "result": result.result,
                    "result_project_snapshot_hash": result.result_project_snapshot_hash,
                });
                if let Some(debug) = result.debug {
                    envelope["debug"] = debug;
                }
                if let Some(dispatch) = result.dispatch_effect {
                    envelope["dispatch"] = serde_json::to_value(dispatch)
                        .map_err(|error| DispatchError::Internal(error.into()))?;
                }
                Ok(envelope)
            }
            crate::execution::runner::WaitOutcome::Replayed { result, dispatch } => Ok(json!({
                "thread": null,
                "result": result,
                "dispatch": dispatch,
            })),
        }
    }
}

fn validate_direct_source_closure(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    resolved: &ryeos_app::thread_lifecycle::ResolvedExecutionRequest,
    project_root: Option<&Path>,
) -> Result<
    Option<ryeos_app::source_closure_admission::SourceClosureValidationPreview>,
    DispatchError,
> {
    let admission = resolved.root_admission.as_ref().ok_or_else(|| {
        DispatchError::Internal(anyhow::anyhow!(
            "direct static validation has no exact root admission"
        ))
    })?;
    let resolution = admission.resolution_output();
    let roots = engine.resolution_roots(project_root.map(Path::to_path_buf));
    let materialization = admission
        .resolution_materialization_binding()
        .map_err(DispatchError::Internal)?;
    let project_content = materialization
        .authoritative_project_content()
        .map_err(DispatchError::Internal)?;
    let project = project_content.as_ref().map(|(root, content)| {
        (
            *root,
            *content as &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
        )
    });
    let source_contract = engine
        .kinds
        .get(&resolved.kind)
        .and_then(|schema| schema.execution.as_ref())
        .and_then(|execution| execution.source_closure.as_ref());
    let source_policy = if source_contract.is_some() {
        let executor_id = resolved
            .resolved_item
            .metadata
            .executor_id
            .as_deref()
            .ok_or_else(|| {
                DispatchError::Internal(anyhow::anyhow!(
                    "source-owning direct item has no executor chain"
                ))
            })?;
        ryeos_engine::launch::plan_builder::resolve_executor_source_policy(
            executor_id,
            &resolution.root.source_path,
            &resolved.kind,
            &engine.kinds,
            &engine.parser_dispatcher,
            &roots,
            &engine.trust_store,
            &engine.node_trust_store,
            project,
        )
        .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?
    } else {
        None
    };
    let project = project
        .map(|(root, content)| {
            let identity = materialization
                .subject_authority()
                .operational_generation()
                .ok_or_else(|| {
                    DispatchError::Internal(anyhow::anyhow!(
                        "pinned source validation has no content generation"
                    ))
                })?
                .to_owned();
            Ok::<_, DispatchError>((root, content, identity))
        })
        .transpose()?;
    ryeos_app::source_closure_admission::preview_source_closure(
        state,
        engine,
        &resolved.kind,
        resolution,
        &roots,
        project,
        source_policy.as_ref(),
    )
    .map_err(DispatchError::Internal)
}

fn map_runner_error(item_ref: String, error: anyhow::Error) -> DispatchError {
    if let Some(eligibility) = error.chain().find_map(|cause| {
        cause.downcast_ref::<crate::execution::runner::ExecutionNotRestartEligible>()
    }) {
        return DispatchError::ExecutionNotRestartEligible {
            item_ref: eligibility.item_ref.clone(),
            reason: eligibility.reason.clone(),
            remediation: eligibility.remediation.clone(),
        };
    }
    DispatchError::SubprocessRunFailed {
        item_ref,
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod direct_protocol_tests {
    use super::*;
    use ryeos_engine::{
        protocol_vocabulary::{EnvInjection, EnvInjectionSource, StdinShape},
        protocols::VerifiedProtocol,
    };

    fn streaming() -> VerifiedProtocol {
        VerifiedProtocol {
            canonical_ref: "protocol:fixture/framed".into(),
            raw_content_digest: "1".repeat(64),
            signer_fingerprint: "2".repeat(64),
            descriptor: serde_yaml::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../bundles/core/.ai/protocols/ryeos/core/tool_streaming.yaml"
            )))
            .unwrap(),
            trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
            bundle_root: std::path::PathBuf::new(),
            descriptor_path: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn framed_output_uses_direct_plan_without_parameters_injection() {
        let mut protocol = streaming();
        assert!(uses_direct_subprocess_plan(&protocol));
        validate_ordinary_protocol_contract(&protocol, "fixture").unwrap();
        protocol.descriptor.stdin.shape = StdinShape::ParametersJson;
        assert!(validate_ordinary_protocol_contract(&protocol, "fixture").is_err());
    }

    #[test]
    fn ordinary_opaque_and_framed_protocols_share_the_route_selector() {
        for body in [
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../bundles/core/.ai/protocols/ryeos/core/opaque.yaml"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../../bundles/core/.ai/protocols/ryeos/core/tool_streaming.yaml"
            )),
        ] {
            let mut protocol = streaming();
            protocol.descriptor = serde_yaml::from_str(body).unwrap();
            assert!(uses_direct_subprocess_plan(&protocol));
            validate_ordinary_protocol_contract(&protocol, "fixture").unwrap();
        }
    }

    #[test]
    fn framed_output_refuses_callback_credentials_and_detachment() {
        for source in [
            EnvInjectionSource::CallbackSocketPath,
            EnvInjectionSource::CallbackToken,
            EnvInjectionSource::ThreadAuthToken,
        ] {
            let mut protocol = streaming();
            protocol.descriptor.env_injections.push(EnvInjection {
                name: "FORBIDDEN".into(),
                source,
            });
            assert!(validate_ordinary_protocol_contract(&protocol, "fixture").is_err());
        }
        let mut protocol = streaming();
        protocol.descriptor.capabilities.allows_detached = true;
        assert!(validate_ordinary_protocol_contract(&protocol, "fixture").is_err());
    }

    #[test]
    fn digest_only_admission_cannot_publish_stream_bodies() {
        use ryeos_engine::history_policy::ThreadResultRetention;
        let protocol = streaming();
        validate_direct_result_retention(&protocol, ThreadResultRetention::Full, "fixture")
            .unwrap();
        assert!(
            validate_direct_result_retention(
                &protocol,
                ThreadResultRetention::DigestOnly,
                "fixture"
            )
            .is_err()
        );
    }
}
