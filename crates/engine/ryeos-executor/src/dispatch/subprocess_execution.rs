//! Subprocess terminator execution for managed runtimes, streaming tools, and
//! ordinary tool subprocesses.

use super::*;

// ── Unified subprocess terminator ─────────────────────────────────────

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
            dispatch_managed_subprocess(
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
                },
                protocol,
            )
            .await
        }
        LifecycleMode::DetachedOk => {
            dispatch_tool_subprocess(
                current_ref,
                thread_profile,
                hop_verified,
                request,
                ctx,
                state,
            )
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
    } = sctx;

    use ryeos_engine::protocol_vocabulary::CallbackChannel;
    if protocol.descriptor.callback_channel == CallbackChannel::None {
        return dispatch_streaming_subprocess(
            canonical_ref,
            hop_verified,
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
    )?;

    // Runtime callback caps (bundle-events / runtime-vault) are minted inside
    // `build_and_launch` from the *composed* `requires` block — after the
    // extends-chain composer has narrowed a child directive against its parent.
    // Minting here (pre-composition) would miss that narrowing.
    let result = launch::build_and_launch(launch::BuildAndLaunchParams {
        state,
        executor_ref: &prepared.executor_ref,
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
        required_envelope_fields: &prepared.required_envelope_fields,
        pre_minted_thread_id: request.pre_minted_thread_id.as_deref(),
        previous_thread_id: request.previous_thread_id.as_deref(),
        parent_execution_context: request.parent_execution_context.as_ref(),
        // Fresh launches and operator follow-ups inject their inputs as the
        // opening stimulus; only an autonomous machine continuation suppresses it.
        suppress_stimulus: false,
        // Fresh resolution: use the freshly-resolved caps (no captured set to pin).
        capability_policy: crate::execution::launch::CapabilityPolicy::Fresh,
        // Fresh launch: cold start, no checkpoint resume.
        checkpoint_resume_mode: crate::execution::launch::CheckpointResumeMode::None,
    })
    .await
    .map_err(|e| match &e {
        launch::BuildAndLaunchError::MissingSecrets { item_ref, secrets } => {
            let first = secrets.first().expect("missing secret error has a secret");
            let source = first.primary_source();
            DispatchError::RequiredSecretMissing {
                item_ref: item_ref.clone(),
                env_var: first.name.clone(),
                source_kind: source.kind_for_wire().to_string(),
                source_name: source.name_for_wire(),
                remediation: crate::dispatch_error::required_secret_remediation(&first.name),
            }
        }
        launch::BuildAndLaunchError::CapabilityRejected { reason } => {
            DispatchError::CapabilityRejected {
                reason: reason.clone(),
            }
        }
        _ => {
            let msg = e.to_string();
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
                DispatchError::Internal(e.into())
            }
        }
    })?;

    Ok(json!({
        "thread": result.thread,
        "result": result.result,
    }))
}

async fn dispatch_streaming_subprocess(
    current_ref: &CanonicalRef,
    verified: Option<&VerifiedItem>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
    _protocol: &ryeos_engine::protocols::VerifiedProtocol,
) -> Result<Value, DispatchError> {
    let item_ref_str = current_ref.to_string();
    let engine_roots = ctx
        .engine
        .resolution_roots(Some(request.project_path.to_path_buf()));

    let bundle_roots: Vec<std::path::PathBuf> = engine_roots
        .ordered
        .iter()
        .filter(|r| r.space == ryeos_engine::contracts::ItemSpace::Bundle)
        .map(|r| {
            r.ai_root
                .parent()
                .map(|pp| pp.to_path_buf())
                .unwrap_or(r.ai_root.clone())
        })
        .collect();

    let effective_parsers = ctx
        .engine
        .effective_parser_dispatcher(Some(request.project_path))
        .map_err(|e| {
            DispatchError::InvalidRef(current_ref.to_string(), format!("parser dispatcher: {e}"))
        })?;

    let resolution_output = ryeos_engine::resolution::run_resolution_pipeline(
        current_ref,
        &ctx.engine.kinds,
        &effective_parsers,
        &engine_roots,
        &ctx.engine.trust_store,
        &ctx.engine.composers,
    )
    .map_err(|e| {
        DispatchError::InvalidRef(
            current_ref.to_string(),
            format!("resolution pipeline failed: {e}"),
        )
    })?;

    let single_root = project_single_root(&resolution_output)?;

    let stdin_data =
        serde_json::to_string(&single_root).map_err(|e| DispatchError::Internal(e.into()))?;

    // Streaming tools are *items*, not runtime-hosted kinds — each
    // tool ships its own binary in the bundle (e.g.
    // `bin/<triple>/ryeos-core-tools`) and the dispatcher
    // resolves it from the verified item's `executor_id` metadata.
    // Phase2's first cut wrongly routed this through
    // `RuntimeRegistry::lookup_for(kind)` — which only finds runtimes
    // declared via `kind: runtime` YAMLs (directive/graph/knowledge),
    // never streaming tools — so every streaming dispatch hit
    // `no runtime serves kind 'streaming_tool'`. The fix is to use the
    // streaming tool's own binary, which is the `executor_id` field
    // the kind-schema's `inventory_schema_keys` already surfaces on
    // `ResolvedItem.metadata`.
    let verified_item = verified.ok_or_else(|| DispatchError::SchemaMisconfigured {
        kind: current_ref.kind.clone(),
        detail: format!(
            "streaming tool '{item_ref_str}' dispatched without a verified item — \
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
                "streaming tool '{item_ref_str}' has no `executor_id` in its YAML \
                 — every `kind: streaming_tool` item must declare \
                 `executor_id: <bare-binary-name>` so the daemon can resolve the \
                 binary against the system bundle's `bin/<triple>/` CAS"
            ),
        })?;
    let executor_ref = format!("native:{executor_id}");

    let cache_root = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("state");
    let executor_path = crate::execution::launch::resolve_native_executor_path(
        &bundle_roots,
        &executor_ref,
        &cache_root,
        &ctx.engine.trust_store,
        ryeos_engine::resolution::TrustClass::TrustedBundle,
    )
    .map_err(|e| DispatchError::RuntimeMaterializationFailed {
        executor_ref: executor_ref.clone(),
        detail: e.to_string(),
    })?;

    let executor_path_str = executor_path.to_string_lossy().to_string();
    let roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
        &engine_roots,
        &state.config.app_root,
    );
    let envs = ryeos_app::process::build_subprocess_envs_with_roots(
        &std::collections::BTreeMap::new(),
        &[],
        roots,
    )
    .map_err(|e| DispatchError::Internal(e.into()))?;
    let subprocess_request = lillux::SubprocessRequest {
        cmd: executor_path_str,
        args: vec![],
        cwd: Some(request.project_path.to_string_lossy().into_owned()),
        envs,
        stdin_data: Some(stdin_data),
        timeout: 120.0,
        limits: None,
    };
    let subprocess_request = ryeos_engine::subprocess_spec::sandbox_lillux_request(
        subprocess_request,
        &state.config.app_root,
        request.project_path,
        &item_ref_str,
        "streaming-tool",
    )
    .map_err(|error| DispatchError::Internal(anyhow::anyhow!(error)))?;
    let result = tokio::task::spawn_blocking(move || lillux::run(subprocess_request))
        .await
        .map_err(|e| DispatchError::Internal(e.into()))?;

    if !result.success {
        return Err(DispatchError::SubprocessRunFailed {
            item_ref: item_ref_str,
            detail: format!(
                "streaming tool exited with code {}: {}",
                result.exit_code,
                &result.stderr[..result.stderr.len().min(500)]
            ),
        });
    }

    let frames = ryeos_engine::protocol_vocabulary::read_all_frames(std::io::Cursor::new(
        result.stdout.as_bytes(),
    ))
    .map_err(|e| {
        DispatchError::Internal(anyhow::anyhow!("frame read failed for streaming tool: {e}"))
    })?;

    serde_json::to_value(&frames)
        .map_err(|e| DispatchError::Internal(anyhow::anyhow!("frame serialize: {e}")))
}

async fn dispatch_tool_subprocess(
    current_ref: &CanonicalRef,
    thread_profile: &str,
    verified: Option<&VerifiedItem>,
    request: &DispatchRequest<'_>,
    ctx: &ExecutionContext,
    state: &AppState,
) -> Result<Value, DispatchError> {
    let item_ref = current_ref.to_string();

    require_terminal_executor_id(verified, &item_ref)?;

    let mut resolved = ryeos_app::thread_lifecycle::resolve_root_execution(
        ryeos_app::thread_lifecycle::ResolveRootExecutionParams {
            engine: &ctx.engine,
            site_id: &ctx.plan_ctx.current_site_id,
            project_path: request.project_path,
            item_ref: &item_ref,
            launch_mode: request.launch_mode,
            parameters: request.params.clone(),
            requested_by: Some(request.acting_principal.to_string()),
            usage_subject: request.usage_subject.clone(),
            usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
            caller_scopes: ctx.caller_scopes.clone(),
            validate_only: request.validate_only,
        },
    )?;

    resolved.kind = thread_profile.to_string();

    if resolved.executor_ref.starts_with("runtime:") {
        return Err(DispatchError::SchemaMisconfigured {
            kind: current_ref.kind.clone(),
            detail: format!(
                "subprocess terminator received an item whose resolved executor is a runtime ref ('{}'); this should have been routed through Managed lifecycle — fix the kind schema",
                resolved.executor_ref
            ),
        });
    }

    // Data-driven execution routine: walk the wrapper's executor chain to its
    // terminal and branch on the terminal's typed `terminal_executor.kind` —
    // never on the alias name or the terminal ref. Every terminal must declare
    // `terminal_executor`; a missing/invalid descriptor is a hard error (no
    // silent subprocess fallback).
    let terminal = ctx
        .engine
        .resolve_terminal_executor(
            &resolved.resolved_item.source_path,
            &resolved.executor_ref,
            &resolved.resolved_item.kind,
            Some(request.project_path.to_path_buf()),
        )
        .map_err(|e| DispatchError::SchemaMisconfigured {
            kind: current_ref.kind.clone(),
            detail: format!("failed to resolve executor-chain terminal for '{item_ref}': {e}"),
        })?;
    if terminal.kind == ryeos_engine::plan_builder::TerminalExecutorKind::MethodDispatch {
        return dispatch_via_method_executor(&resolved, request, ctx, state).await;
    }

    if let Some(target) = request.target_site_id {
        resolved.target_site_id = Some(target.to_string());
    }

    if request.validate_only {
        let engine = ctx.engine.clone();
        let resolved_clone = resolved.clone();
        let validated = tokio::task::spawn_blocking(move || {
            ryeos_app::thread_lifecycle::validate_item(&engine, &resolved_clone)
        })
        .await
        .map_err(|e| DispatchError::SubprocessRunFailed {
            item_ref: resolved.item_ref.clone(),
            detail: format!("validate_only join failure: {e}"),
        })??;

        return Ok(json!({
            "validated": true,
            "item_ref": resolved.item_ref,
            "kind": resolved.kind,
            "executor_ref": resolved.executor_ref,
            "trust_class": validated.trust_class,
            "plan_id": validated.plan_id,
        }));
    }

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
        // Fresh dispatch: no captured runtime ref. The thread's runtime identity
        // is captured in launch metadata; resume reads it back from there.
        runtime_ref: None,
    };

    if request.launch_mode == "detached" {
        let result = crate::execution::runner::run_detached(state.clone(), params)
            .await
            .map_err(|e| DispatchError::SubprocessRunFailed {
                item_ref: item_ref_for_error.clone(),
                detail: e.to_string(),
            })?;
        Ok(json!({
            "thread": result.running_thread,
            "detached": true,
        }))
    } else {
        let result = crate::execution::runner::run_inline(state.clone(), params)
            .await
            .map_err(|e| DispatchError::SubprocessRunFailed {
                item_ref: item_ref_for_error,
                detail: e.to_string(),
            })?;
        let mut envelope = json!({
            "thread": result.finalized_thread,
            "result": result.result,
        });
        if let Some(debug) = result.debug {
            envelope["debug"] = debug;
        }
        Ok(envelope)
    }
}
