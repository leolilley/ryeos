//! Subprocess terminator execution for managed runtimes, callback-free streams,
//! and ordinary tool subprocesses.

use super::*;

// ── Unified subprocess terminator ─────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManagedProtocolRoute {
    CallbackRuntime,
    FramedStreaming,
}

/// Enforce the one ordinary-subprocess wire contract everywhere admission can
/// happen. Keeping this check shared prevents accepted preflight from minting
/// a thread that the runner later rejects for protocol shape.
pub(crate) fn validate_ordinary_protocol_contract(
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
    kind: &str,
) -> Result<(), DispatchError> {
    use ryeos_engine::protocol_vocabulary::{LifecycleMode, StdinShape, StdoutMode, StdoutShape};

    if protocol.descriptor.lifecycle.mode != LifecycleMode::DetachedOk
        || protocol.descriptor.stdin.shape != StdinShape::Opaque
        || protocol.descriptor.stdout.shape != StdoutShape::OpaqueBytes
        || protocol.descriptor.stdout.mode != StdoutMode::Terminal
    {
        return Err(DispatchError::SchemaMisconfigured {
            kind: kind.to_string(),
            detail: format!(
                "ordinary subprocess protocol '{}' has unsupported wire contract: expected opaque stdin, terminal opaque_bytes stdout, and detached_ok lifecycle; got {:?} stdin, {:?}/{:?} stdout, and {:?} lifecycle",
                protocol.canonical_ref,
                protocol.descriptor.stdin.shape,
                protocol.descriptor.stdout.shape,
                protocol.descriptor.stdout.mode,
                protocol.descriptor.lifecycle.mode,
            ),
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
        CallbackChannel, EnvInjectionSource, LifecycleMode, StdinShape, StdoutMode, StdoutShape,
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
    if protocol.descriptor.stdin.shape == StdinShape::LaunchEnvelope {
        return Err(DispatchError::SchemaMisconfigured {
            kind: kind.to_string(),
            detail: format!(
                "managed callback-free protocol '{}' requests a launch envelope, but the daemon's framed-streaming surface has no runtime envelope authority",
                protocol.canonical_ref,
            ),
        });
    }
    if let Some(injection) = protocol.descriptor.env_injections.iter().find(|injection| {
        matches!(
            injection.source,
            EnvInjectionSource::CallbackSocketPath
                | EnvInjectionSource::CallbackToken
                | EnvInjectionSource::ThreadAuthToken
        )
    }) {
        return Err(DispatchError::SchemaMisconfigured {
            kind: kind.to_string(),
            detail: format!(
                "managed callback-free protocol '{}' requests unavailable env source {:?} for injection '{}'",
                protocol.canonical_ref, injection.source, injection.name,
            ),
        });
    }
    match (
        protocol.descriptor.stdout.mode,
        protocol.descriptor.stdout.shape,
    ) {
        (StdoutMode::Streaming, StdoutShape::StreamingChunks) => {
            Ok(ManagedProtocolRoute::FramedStreaming)
        }
        (mode, shape) => Err(DispatchError::SchemaMisconfigured {
            kind: kind.to_string(),
            detail: format!(
                "managed callback-free protocol '{}' declares stdout {shape:?}/{mode:?}; \
                 daemon dispatch currently owns only the verified framed-streaming contract",
                protocol.canonical_ref,
            ),
        }),
    }
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
        TerminatorDecl::Subprocess { protocol_ref } => protocol_ref.as_str(),
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
        .require(protocol_ref)
        .map_err(|_| DispatchError::ProtocolNotRegistered(protocol_ref.to_string()))?;

    check_dispatch_capabilities(&protocol.descriptor.capabilities, request)?;

    use ryeos_engine::protocol_vocabulary::StdoutMode;
    if protocol.descriptor.stdout.mode == StdoutMode::Streaming && request.launch_mode == "detached"
    {
        return Err(DispatchError::StreamingNotDetachable);
    }

    use ryeos_engine::protocol_vocabulary::LifecycleMode;
    match protocol.descriptor.lifecycle.mode {
        LifecycleMode::Managed => {
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
                    role,
                    root_subject,
                    hop_runtime,
                    launch_handoff,
                },
                protocol,
            ))
            .await
        }
        LifecycleMode::DetachedOk => {
            validate_ordinary_protocol_contract(protocol, &current_ref.kind)?;
            Box::pin(dispatch_tool_subprocess(
                current_ref,
                thread_profile,
                hop_verified,
                request,
                ctx,
                state,
                launch_handoff,
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
        role,
        launch_handoff,
    } = sctx;

    if classify_managed_protocol(protocol, &canonical_ref.kind)?
        == ManagedProtocolRoute::FramedStreaming
    {
        return dispatch_streaming_subprocess(
            canonical_ref,
            hop_thread_profile,
            hop_verified,
            root_subject,
            request,
            ctx,
            state,
            protocol,
        )
        .await;
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
        &state.node_history_policy,
    )?;

    if request.validate_only {
        let external_content = validate_managed_effective_program(&prepared, request, ctx, state)?;
        let admission_ready = external_content
            .as_ref()
            .is_none_or(|preview| preview.ready_for_admission);
        return Ok(json!({
            "validated": true,
            "admission_ready": admission_ready,
            "item_ref": &prepared.resolved.item_ref,
            "kind": &prepared.resolved.resolved_item.kind,
            "executor_ref": &prepared.executor_ref,
            "external_content": external_content,
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
        resolved: &prepared.resolved,
        project_path,
        provenance: &request.provenance,
        parameters: &params,
        metadata_required_secrets: &prepared.resolved.resolved_item.metadata.required_secrets,
        pre_minted_thread_id: request.pre_minted_thread_id.as_deref(),
        previous_thread_id: request.previous_thread_id.as_deref(),
        parent_execution_context: request.parent_execution_context.as_ref(),
        // Fresh launches and operator follow-ups inject their inputs as the
        // opening stimulus; only an autonomous machine continuation suppresses it.
        suppress_stimulus: false,
        // Fresh resolution: use the freshly-resolved caps (no captured set to pin).
        capability_policy: crate::execution::launch::CapabilityPolicy::AdmissionDefault,
        // Fresh launch: cold start, no checkpoint resume.
        checkpoint_resume_mode: crate::execution::launch::CheckpointResumeMode::None,
        launch_handoff,
    })
    .await
    .map_err(|e| match e {
        launch::BuildAndLaunchError::LaunchPreparation(error) => *error,
        launch::BuildAndLaunchError::MissingSecrets { item_ref, secrets } => {
            let first = secrets.first().expect("missing secret error has a secret");
            let source = first.primary_source();
            DispatchError::RequiredSecretMissing {
                item_ref,
                env_var: first.name.clone(),
                source_kind: source.kind_for_wire().to_string(),
                source_name: source.name_for_wire(),
                remediation: crate::dispatch_error::required_secret_remediation(&first.name),
            }
        }
        launch::BuildAndLaunchError::CapabilityRejected { reason } => {
            DispatchError::CapabilityRejected { reason }
        }
        launch::BuildAndLaunchError::LaunchCancelled { stage, .. } => {
            DispatchError::LaunchCancelled { stage }
        }
        other => {
            let msg = other.to_string();
            if msg.contains("manifest")
                || msg.contains("binary")
                || msg.contains("blob")
                || msg.contains("materializ")
                || msg.contains("native executor")
                || msg.contains("arch check")
            {
                DispatchError::RuntimeMaterializationFailed {
                    executor_ref: prepared.executor_ref.clone(),
                    detail: msg,
                }
            } else {
                DispatchError::Internal(other.into())
            }
        }
    })?;

    Ok(json!({
        "thread": result.thread,
        "result": result.result,
    }))
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
    let subject_authority = admission
        .plan_context()
        .subject_resolution_authority
        .clone();
    let resolution_project_root = (!matches!(
        subject_authority,
        ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
    ))
    .then(|| {
        admission.execution_workspace().ok_or_else(|| {
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

// Verified hop identity, root authority, request context, daemon state, and
// protocol contract remain explicit at the subprocess dispatch boundary.
#[allow(clippy::too_many_arguments)]
async fn dispatch_streaming_subprocess(
    current_ref: &CanonicalRef,
    thread_profile: &str,
    verified: Option<&VerifiedItem>,
    root_subject: Option<RootSubject>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    protocol: &ryeos_engine::protocols::VerifiedProtocol,
) -> Result<Value, DispatchError> {
    let terminal_ref = current_ref.to_string();
    let resolution_project_root = (!matches!(
        ctx.plan_ctx.subject_resolution_authority,
        ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
    ))
    .then_some(request.project_path);
    let engine_roots = ctx
        .engine
        .resolution_roots(resolution_project_root.map(std::path::Path::to_path_buf));

    let bundle_roots: Vec<std::path::PathBuf> = engine_roots
        .authoritative_bundle_roots()
        .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?
        .into_iter()
        .map(std::path::Path::to_path_buf)
        .collect();

    // A callback-free streaming terminator owns its executable directly rather
    // than borrowing a runtime-registry host. The kind schema exposes that
    // signed direct-binary identity through `executor_id`; no kind or protocol
    // name participates in the routing decision.
    let verified_item = verified.ok_or_else(|| DispatchError::SchemaMisconfigured {
        kind: current_ref.kind.clone(),
        detail: format!(
            "callback-free streaming item '{terminal_ref}' dispatched without a verified item — \
             the dispatch loop must resolve before reaching a streaming terminator"
        ),
    })?;

    let executor_id = verified_item
        .resolved
        .metadata
        .executor_id
        .as_ref()
        .ok_or_else(|| DispatchError::SchemaMisconfigured {
            kind: current_ref.kind.clone(),
            detail: format!(
                "callback-free streaming item '{terminal_ref}' has no direct `executor_id`; \
                 its kind schema must extract a signed bare binary identity so the daemon \
                 can resolve it against the installed bundle executor manifest"
            ),
        })?;
    let executor_ref = format!("native:{executor_id}");

    // The terminal hop selects the executable and protocol. Durable thread
    // identity remains the first verified subject admitted by public preflight
    // across any alias or registry traversal.
    let subject = root_subject.unwrap_or_else(|| RootSubject {
        item_ref: terminal_ref.clone(),
        thread_profile: thread_profile.to_string(),
        verified: Some(verified_item.clone()),
    });
    let verified_subject = match subject.verified {
        Some(verified) => verified,
        None => {
            let canonical = CanonicalRef::parse(&subject.item_ref).map_err(|error| {
                DispatchError::InvalidRef(subject.item_ref.clone(), error.to_string())
            })?;
            let resolved = ctx
                .engine
                .resolve(&ctx.plan_ctx, &canonical)
                .map_err(|error| DispatchError::SchemaMisconfigured {
                    kind: canonical.kind.clone(),
                    detail: format!(
                        "streaming subject resolution failed for '{}': {error}",
                        subject.item_ref
                    ),
                })?;
            ctx.engine
                .verify(&ctx.plan_ctx, resolved)
                .map_err(|error| {
                    DispatchError::InvalidRef(
                        subject.item_ref.clone(),
                        format!("streaming subject verification failed: {error}"),
                    )
                })?
        }
    };
    let subject_item_ref = subject.item_ref;
    let subject_thread_profile = subject.thread_profile;

    if request.parent_execution_context.is_some() && request.previous_thread_id.is_some() {
        return Err(DispatchError::Internal(anyhow::anyhow!(
            "streaming launch cannot be both a callback child and a chained continuation"
        )));
    }

    let cache_root = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("state");
    let materialization_engine = (*ctx.engine).clone();
    let materialization_bundle_roots = bundle_roots.clone();
    let materialization_executor_ref = executor_ref.clone();
    let materialization_timings = request.launch_timings.clone();
    let materialization_queue_timer = materialization_timings.as_ref().map(|timings| {
        timings.nested(
            "background_dispatch",
            "executor_materialization_blocking_queue_wait",
        )
    });
    let executor = tokio::task::spawn_blocking(move || {
        drop(materialization_queue_timer);
        let _materialization_work_timer = materialization_timings.as_ref().map(|timings| {
            timings.nested(
                "background_dispatch",
                "executor_materialization_blocking_work",
            )
        });
        crate::execution::launch::materialize_native_executor_for_engine(
            &materialization_engine,
            &materialization_bundle_roots,
            &materialization_executor_ref,
            &cache_root,
            ryeos_engine::resolution::TrustClass::TrustedBundle,
            materialization_timings.as_ref(),
        )
    })
    .await
    .map_err(|error| {
        DispatchError::Internal(anyhow::anyhow!(
            "streaming executor materialization worker failed: {error}"
        ))
    })?
    .map_err(|error| DispatchError::RuntimeMaterializationFailed {
        executor_ref: executor_ref.clone(),
        detail: error.to_string(),
    })?;

    let executor_path = executor.path.clone();
    let executor_path_str = executor_path
        .to_str()
        .ok_or_else(|| {
            DispatchError::Internal(anyhow::anyhow!("resolved executor path is not valid UTF-8"))
        })?
        .to_owned();
    let project_path = request.project_path.to_path_buf();
    let project_path_str = project_path
        .to_str()
        .ok_or_else(|| {
            DispatchError::Internal(anyhow::anyhow!("streaming project path is not valid UTF-8"))
        })?
        .to_owned();
    let isolation_verified_command = executor.verified_command.clone();
    // Mint the authoritative id before building the protocol environment so
    // the child and its durable lifecycle row observe the exact same identity.
    // This is still pure pre-launch setup: no row exists until every declared
    // injection has been resolved and validated below.
    let thread_id = request
        .pre_minted_thread_id
        .clone()
        .unwrap_or_else(ryeos_app::thread_lifecycle::new_thread_id);
    let roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
        &engine_roots,
        &state.config.app_root,
    )
    .map_err(DispatchError::Internal)?;
    let env_request = ryeos_engine::subprocess_spec::SubprocessBuildRequest {
        cmd: executor_path,
        args: Vec::new(),
        cwd: project_path.clone(),
        timeout: std::time::Duration::from_secs(120),
        item_ref: CanonicalRef::parse(&subject_item_ref).map_err(|error| {
            DispatchError::InvalidRef(subject_item_ref.clone(), error.to_string())
        })?,
        thread_id: thread_id.clone(),
        project_path: project_path.clone(),
        acting_principal: request.acting_principal.to_string(),
        cas_root: state
            .state_store
            .cas_root()
            .map_err(DispatchError::Internal)?,
        callback_token: None,
        callback_socket_path: None,
        callback_project_path: None,
        project_state_scope: request
            .provenance
            .project_authority()
            .project_state_scope_id()
            .map_err(DispatchError::Internal)?,
        thread_auth_token: None,
        params: request.params.clone(),
        resolution_output: None,
    };
    let stdin_bytes = ryeos_engine::protocol_vocabulary::build_stdin(
        protocol.descriptor.stdin.shape,
        &env_request,
    )
    .map_err(|error| DispatchError::SchemaMisconfigured {
        kind: current_ref.kind.clone(),
        detail: format!(
            "protocol '{}' stdin contract is unavailable for this streaming launch: {error}",
            protocol.canonical_ref
        ),
    })?;
    let stdin_data =
        String::from_utf8(stdin_bytes).map_err(|error| DispatchError::SchemaMisconfigured {
            kind: current_ref.kind.clone(),
            detail: format!(
                "protocol '{}' produced non-UTF-8 stdin for the text subprocess bridge: {error}",
                protocol.canonical_ref
            ),
        })?;
    let protocol_bindings = protocol
        .descriptor
        .env_injections
        .iter()
        .map(|injection| {
            let value = ryeos_engine::protocol_vocabulary::produce_env_value(
                injection.source,
                &env_request,
            )
            .map_err(|error| DispatchError::SchemaMisconfigured {
                kind: current_ref.kind.clone(),
                detail: format!(
                    "protocol '{}' env injection '{}' is unavailable for this streaming launch: {error}",
                    protocol.canonical_ref, injection.name
                ),
            })?;
            Ok(ryeos_app::env_contract::EnvBinding::new(
                injection.name.clone(),
                value,
                ryeos_app::env_contract::EnvSourceDetail::ProtocolInjection {
                    source: injection.source,
                },
            ))
        })
        .collect::<Result<Vec<_>, DispatchError>>()?;
    let envs = ryeos_app::env_contract::EnvContractBuilder::new()
        .with_base_allowlist(std::env::vars_os().map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        }))
        .map_err(|error| DispatchError::Internal(error.into()))?
        .with_daemon_roots(roots)
        .map_err(|error| DispatchError::Internal(error.into()))?
        .with_typed_bindings(protocol_bindings)
        .map_err(|error| DispatchError::Internal(error.into()))?
        .build();
    // Streaming execution used to run as an untracked `lillux::run` blocking
    // task. A forced UDS shutdown could therefore drop the request future while
    // leaving a process absent from the daemon's exact-identity drain. Give
    // every invocation the same durable lifecycle/process owner as the other
    // inline terminators before any process can be spawned.
    let launch_claim =
        crate::execution::launch_claim::ThreadLaunchClaim::acquire_fresh(state, &thread_id)
            .map_err(DispatchError::Internal)?;
    let launch_owner = launch_claim
        .canonical_owner()
        .map_err(DispatchError::Internal)?;
    let created = if let Some(parent) = request.parent_execution_context.as_ref() {
        let durable_parent = state
            .threads
            .get_thread(&parent.parent_thread_id)
            .map_err(DispatchError::Internal)?
            .ok_or_else(|| {
                DispatchError::Internal(anyhow::anyhow!(
                    "streaming parent thread not found: {}",
                    parent.parent_thread_id
                ))
            })?;
        let project_authority = request
            .provenance
            .project_authority()
            .clone()
            .for_child()
            .map_err(DispatchError::Internal)?;
        state
            .threads
            .create_thread(&ryeos_app::thread_lifecycle::ThreadCreateParams {
                thread_id: thread_id.clone(),
                chain_root_id: durable_parent.chain_root_id,
                kind: subject_thread_profile.clone(),
                item_ref: subject_item_ref.clone(),
                executor_ref: executor_ref.clone(),
                launch_mode: request.launch_mode.to_string(),
                current_site_id: ctx.plan_ctx.current_site_id.clone(),
                origin_site_id: ctx.plan_ctx.origin_site_id.clone(),
                upstream_thread_id: Some(durable_parent.thread_id.clone()),
                requested_by: Some(request.acting_principal.to_string()),
                project_root: project_authority
                    .project_root_projection()
                    .map(std::path::Path::to_path_buf),
                base_project_snapshot_hash: project_authority
                    .operational_snapshot_projection()
                    .map(str::to_owned),
                project_authority,
                usage_subject: request.usage_subject.clone(),
                usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
                captured_history_policy: None,
            })
    } else {
        let root_admission = if request.previous_thread_id.is_some() {
            None
        } else {
            let admission = request.root_admission.as_ref().ok_or_else(|| {
                DispatchError::Internal(anyhow::anyhow!(
                    "streaming root `{subject_item_ref}` has no sealed root admission"
                ))
            })?;
            admission
                .ensure_matches_subject(&ctx.engine, &verified_subject, &subject_thread_profile)
                .map_err(DispatchError::Internal)?;
            Some(admission.clone())
        };
        let resolved_stream = match root_admission {
            Some(admission) => admission
                .execution_request(
                    ryeos_app::thread_lifecycle::RootExecutionRoute::DirectNativeExecutor,
                    request.launch_mode.to_string(),
                    request.params.clone(),
                )
                .map_err(DispatchError::Internal)?,
            None => ryeos_app::thread_lifecycle::ResolvedExecutionRequest {
                kind: subject_thread_profile.clone(),
                item_ref: subject_item_ref.clone(),
                executor_ref: executor_ref.clone(),
                launch_mode: request.launch_mode.to_string(),
                current_site_id: ctx.plan_ctx.current_site_id.clone(),
                origin_site_id: ctx.plan_ctx.origin_site_id.clone(),
                target_site_id: None,
                requested_by: Some(request.acting_principal.to_string()),
                usage_subject: request.usage_subject.clone(),
                usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
                parameters: request.params.clone(),
                ref_bindings: request.ref_bindings.clone(),
                root_raw_content_digest: verified_subject.resolved.raw_content_digest.clone(),
                resolved_item: verified_subject.resolved.clone(),
                plan_context: ctx.plan_ctx.clone(),
                root_admission: None,
            },
        };
        if let Some(previous_thread_id) = request.previous_thread_id.as_deref() {
            state.threads.create_continuation_with_id(
                &thread_id,
                previous_thread_id,
                &resolved_stream,
                Some("chained_resume"),
                Vec::new(),
            )
        } else {
            state.threads.create_root_thread_with_id(
                &thread_id,
                &resolved_stream,
                request.provenance.project_authority().clone(),
            )
        }
    };
    created.map_err(|error| {
        DispatchError::Internal(anyhow::anyhow!(
            "streaming thread creation failed for {thread_id}: {error}"
        ))
    })?;
    let mut lifecycle_owner =
        crate::execution::process_attachment::LifecycleOwnerGuard::new(state, &thread_id);

    if let Some(parent) = request.parent_execution_context.as_ref() {
        let inherited_stop = match state.state_store.record_child_link(
            &parent.parent_thread_id,
            &thread_id,
            "dispatch",
        ) {
            Ok(inherited_stop) => inherited_stop,
            Err(link_error) => {
                let link_error_message = link_error.to_string();
                let cleanup = crate::dispatch::finalize_method_thread_if_needed(
                    state,
                    &thread_id,
                    &launch_owner,
                    "failed",
                    Some(json!({
                        "code": "child_link_failed",
                        "reason": link_error_message.clone(),
                    })),
                );
                match cleanup {
                    Ok(outcome) if outcome.is_settled() => lifecycle_owner.disarm(),
                    Ok(_) => {}
                    Err(cleanup_error) => {
                        return Err(DispatchError::Internal(anyhow::anyhow!(
                            "record streaming child lineage for {} failed: {}; conditional cleanup also failed: {:#}",
                            parent.parent_thread_id,
                            link_error_message,
                            cleanup_error,
                        )));
                    }
                }
                return Err(DispatchError::Internal(anyhow::anyhow!(
                    "record streaming child lineage for {}: {}",
                    parent.parent_thread_id,
                    link_error_message,
                )));
            }
        };
        if inherited_stop.is_some() {
            let settled = crate::execution::process_attachment::finalize_requested_stop_if_present(
                state, &thread_id,
            )
            .map_err(DispatchError::Internal)?;
            if !settled {
                return Err(DispatchError::Internal(anyhow::anyhow!(
                    "parent {} propagated a stop to streaming child {thread_id}, but the child had no durable stop",
                    parent.parent_thread_id,
                )));
            }
            lifecycle_owner.disarm();
            return Err(DispatchError::Internal(anyhow::anyhow!(
                "parent {} was stop-requested before streaming child launch",
                parent.parent_thread_id,
            )));
        }
    }

    let execution: Result<(Value, usize, usize), DispatchError> = async {
        state
            .threads
            .mark_running(&thread_id)
            .map_err(DispatchError::Internal)?;

        let subprocess_request = lillux::SubprocessRequest {
            cmd: executor_path_str,
            argv0: None,
            args: vec![],
            cwd: Some(project_path_str),
            envs,
            stdin_data: Some(stdin_data),
            timeout: 120.0,
            limits: None,
            inherited_fds: Vec::new(),
            supervised_status: None,
        };
        let live_access = request
            .provenance
            .isolation_live_access_authority()
            .map_err(DispatchError::Internal)?;
        let applied = state
            .isolation
            .apply_awaiting_attachment_with_provenance(
                subprocess_request,
                ryeos_engine::isolation::IsolationLaunchContext {
                    project_path: request.project_path,
                    project_authority: request.provenance.isolation_project_authority(),
                    filesystem_authority_ceiling:
                        ryeos_engine::isolation::IsolationFilesystemAuthorityCeiling::NodePolicy,
                    network_authority_ceiling:
                        ryeos_engine::isolation::IsolationNetworkAuthorityCeiling::NodePolicy,
                    live_access: live_access.as_ref(),
                    state_root: request.provenance.state_root_override(),
                    checkpoint_dir: None,
                    daemon_socket_path: None,
                    bundle_roots: &bundle_roots,
                    node_trusted_keys_dir: Some(&state.config.runtime_root().trusted_keys_dir()),
                    verified_code: &[],
                    verified_command: Some(&isolation_verified_command),
                    external_read_only_mounts: &[],
                    target_channel: None,
                    item_ref: &subject_item_ref,
                    thread_id: &thread_id,
                },
            )
            .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
        state
            .state_store
            .seed_isolation_provenance(&thread_id, applied.provenance)
            .map_err(DispatchError::Internal)?;
        let subprocess_request = applied.request;
        let workspace_lifeline = request.provenance.workspace_lifeline();
        let process_state = state.clone();
        let process_thread_id = thread_id.clone();
        let process_launch_owner = launch_owner.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::execution::process_attachment::run_lillux_attached(
                &process_state,
                &process_thread_id,
                &process_launch_owner,
                subprocess_request,
                workspace_lifeline,
            )
        })
        .await
        .map_err(|error| DispatchError::Internal(error.into()))?
        .map_err(DispatchError::Internal)?;

        if !result.success {
            return Err(DispatchError::SubprocessRunFailed {
                item_ref: subject_item_ref.clone(),
                detail: format!(
                    "exit_code={}, stderr={}",
                    result.exit_code,
                    result.stderr.chars().take(500).collect::<String>()
                ),
            });
        }

        let framed_stdout_bytes = result.stdout.len();
        let frames = ryeos_engine::protocol_vocabulary::read_all_frames(std::io::Cursor::new(
            result.stdout.as_bytes(),
        ))
        .map_err(|error| {
            DispatchError::Internal(anyhow::anyhow!(
                "frame read failed for streaming subprocess: {error}"
            ))
        })?;

        let frame_count = frames.len();
        let frames = serde_json::to_value(&frames).map_err(|error| {
            DispatchError::Internal(anyhow::anyhow!("frame serialize: {error}"))
        })?;
        Ok((frames, frame_count, framed_stdout_bytes))
    }
    .await;

    match execution {
        Ok((frames, frame_count, framed_stdout_bytes)) => finalize_streaming_success(
            state,
            &thread_id,
            &launch_owner,
            frames,
            frame_count,
            framed_stdout_bytes,
            &mut lifecycle_owner,
        ),
        Err(error) => Err(finalize_streaming_failure(
            state,
            &thread_id,
            &launch_owner,
            error,
            &mut lifecycle_owner,
        )),
    }
}

fn finalize_streaming_success(
    state: &AppState,
    thread_id: &str,
    launch_owner: &str,
    frames: Value,
    frame_count: usize,
    framed_stdout_bytes: usize,
    lifecycle_owner: &mut crate::execution::process_attachment::LifecycleOwnerGuard,
) -> Result<Value, DispatchError> {
    let outcome = crate::dispatch::finalize_method_thread_if_needed(
        state,
        thread_id,
        launch_owner,
        "completed",
        Some(json!({
            "streaming": {
                "frame_count": frame_count,
                "framed_stdout_bytes": framed_stdout_bytes,
            }
        })),
    )
    .map_err(|error| {
        DispatchError::Internal(anyhow::anyhow!(
            "finalize successful streaming subprocess: {error:#}"
        ))
    })?;
    lifecycle_owner.disarm();
    match outcome {
        crate::dispatch::MethodFinalizeOutcome::Finalized => Ok(frames),
        crate::dispatch::MethodFinalizeOutcome::AlreadyTerminal => {
            Err(DispatchError::Internal(anyhow::anyhow!(
                "streaming thread {thread_id} became terminal before its successful result was committed"
            )))
        }
        crate::dispatch::MethodFinalizeOutcome::DurableStopSettled => Err(DispatchError::Internal(
            anyhow::anyhow!("streaming thread {thread_id} completed after a durable stop won"),
        )),
        crate::dispatch::MethodFinalizeOutcome::PreservedForShutdown => {
            Err(DispatchError::Internal(anyhow::anyhow!(
                "streaming thread {thread_id} was interrupted by daemon shutdown and preserved for recovery"
            )))
        }
    }
}

fn finalize_streaming_failure(
    state: &AppState,
    thread_id: &str,
    launch_owner: &str,
    execution_error: DispatchError,
    lifecycle_owner: &mut crate::execution::process_attachment::LifecycleOwnerGuard,
) -> DispatchError {
    let error_code = execution_error.code();
    let error_message = execution_error.to_string();
    match crate::dispatch::finalize_method_thread_if_needed(
        state,
        thread_id,
        launch_owner,
        "failed",
        Some(json!({
            "code": error_code,
            "reason": error_message,
        })),
    ) {
        Ok(_) => {
            lifecycle_owner.disarm();
            execution_error
        }
        Err(cleanup_error) => DispatchError::Internal(anyhow::anyhow!(
            "streaming subprocess execution failed: {}; terminal cleanup also failed: {:#}",
            execution_error,
            cleanup_error,
        )),
    }
}

async fn dispatch_tool_subprocess(
    current_ref: &CanonicalRef,
    thread_profile: &str,
    verified: Option<&VerifiedItem>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    launch_handoff: Option<&crate::execution::launch::LaunchHandoff>,
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
    let node_history_policy = std::sync::Arc::clone(&state.node_history_policy);
    let resolution_ref_bindings = request.ref_bindings.clone();
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
            .and_then(|admission| admission.execution_workspace())
            .or(Some(request.project_path)),
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
        return Box::pin(dispatch_via_method_executor(
            &resolved,
            request,
            ctx,
            state,
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
            .and_then(|admission| admission.execution_workspace())
            .or(Some(request.project_path));
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
