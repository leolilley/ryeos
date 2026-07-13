//! Handler for `ComposeContextPositions` launch augmentation.
//!
//! This augmentation:
//! 1. Reads `source_derived` from the composed view (position → refs map).
//! 2. Validates that all refs are canonical (prefixed with `target_kind:`).
//! 3. Pre-resolves each unique ref via the engine resolution pipeline.
//! 4. Projects to a slim multi-root payload.
//! 5. Mints a child thread record + callback token.
//! 6. Spawns the target kind's runtime via lillux.
//! 7. Parses the child's `MethodCallResult` and writes `rendered_contexts`
//!    + `rendered_contexts_meta` into the parent's composed view.
//!
//! Rule 1: the daemon never calls compose logic in-process.
//! Rule 2: all kind-specific decisions come from `decl.target_kind`,
//!          never hardcoded.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::kind_registry::LaunchAugmentationDecl;
use ryeos_engine::resolution::ResolutionOutput;
use serde_json::{json, Value};

use super::LaunchAugmentationError;

/// Run the `ComposeContextPositions` augmentation.
// Execution plumbing: each argument is a distinct leg of the thread's
// auth/provenance context, threaded verbatim — a struct would rename,
// not simplify. Restructure with a compiler in the loop, not here.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    decl: &LaunchAugmentationDecl,
    resolution: &mut ResolutionOutput,
    parent_thread_id: &str,
    project_path: &Path,
    engine: &ryeos_engine::engine::Engine,
    provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    plan_ctx: &ryeos_engine::contracts::PlanContext,
    principal_fingerprint: &str,
    state: &ryeos_app::state::AppState,
) -> Result<(), LaunchAugmentationError> {
    let (
        target_kind,
        target_method,
        source_derived,
        output_derived,
        meta_output_derived,
        per_position_budget,
        runtime_config,
    ) = match decl {
        LaunchAugmentationDecl::ComposeContextPositions {
            target_kind,
            target_method,
            source_derived,
            output_derived,
            meta_output_derived,
            per_position_budget,
            runtime_config,
        } => (
            target_kind,
            target_method,
            source_derived,
            output_derived,
            meta_output_derived,
            per_position_budget,
            runtime_config,
        ),
    };

    // 1. Read source_derived from composed view.
    let positions = read_positions(resolution, source_derived)?;

    // Short-circuit: no positions to render.
    if positions.values().all(|v| v.is_empty()) {
        write_empty(resolution, output_derived, meta_output_derived);
        return Ok(());
    }

    // 2. Validate canonical refs: every value must start with
    //    `<target_kind>:` (e.g. `knowledge:`).
    validate_canonical_refs(&positions, target_kind)?;

    // 3. Pre-resolve unique refs in-process via engine pipeline.
    let unique_refs: BTreeSet<&str> = positions
        .values()
        .flat_map(|v| v.iter().map(|s| s.as_str()))
        .collect();
    let engine_roots = engine.resolution_roots(Some(project_path.to_path_buf()));
    let effective_parsers = engine
        .effective_parser_dispatcher(Some(project_path))
        .map_err(|e| LaunchAugmentationError::RuntimeRegistry(format!("parsers: {e}")))?;

    let mut per_root: BTreeMap<String, ResolutionOutput> = BTreeMap::new();
    for r in &unique_refs {
        let canonical =
            CanonicalRef::parse(r).map_err(|e| LaunchAugmentationError::ParseRef(e.to_string()))?;
        let resolution_output = ryeos_engine::resolution::run_resolution_pipeline(
            &canonical,
            &engine.kinds,
            &effective_parsers,
            &engine_roots,
            &engine.trust_store,
            &engine.composers,
        )
        .map_err(|e| LaunchAugmentationError::ResolutionFailed {
            ref_: r.to_string(),
            source: e,
        })?;
        crate::execution::launch::enforce_effective_trust(
            resolution_output.effective_trust_class,
            r,
            target_kind,
        )
        .map_err(|e| LaunchAugmentationError::EffectiveTrustRejected(e.to_string()))?;
        per_root.insert(r.to_string(), resolution_output);
    }

    // 4. Project to slim payload.
    let payload = super::projection::build_compose_context_payload(
        &per_root,
        &positions,
        per_position_budget,
    )?;

    // 5. Look up the target kind's runtime.
    let verified_runtime = engine.runtimes.lookup_for(target_kind).map_err(|_| {
        LaunchAugmentationError::RuntimeRegistry(format!("no runtime serves kind '{target_kind}'"))
    })?;

    let executor_ref = format!(
        "native:{}",
        crate::dispatch::strip_binary_ref_prefix(&verified_runtime.yaml.binary_ref)
            .map_err(|e| LaunchAugmentationError::RuntimeRegistry(e.to_string()))?
    );

    // 6. Mint child thread record under parent.
    let child_thread_id = ryeos_app::thread_lifecycle::new_thread_id();
    // Derive the child thread's kind from the target kind's schema-declared
    // thread_profile. This keeps thread kinds in sync with kind schemas
    // rather than hardcoding "system_task".
    let child_thread_kind = engine
        .kinds
        .get(target_kind)
        .and_then(|schema| schema.execution())
        .and_then(|exec| exec.thread_profile.as_ref())
        .map(|tp| tp.name.as_str())
        .ok_or_else(|| {
            LaunchAugmentationError::RuntimeRegistry(format!(
                "target kind '{target_kind}' must declare execution.thread_profile"
            ))
        })?;
    state
        .threads
        .create_thread(&ryeos_app::thread_lifecycle::ThreadCreateParams {
            thread_id: child_thread_id.clone(),
            chain_root_id: parent_thread_id.to_string(),
            kind: child_thread_kind.to_string(),
            item_ref: format!("{target_kind}://{target_method}"),
            executor_ref: executor_ref.clone(),
            launch_mode: "inline".to_string(),
            current_site_id: plan_ctx.current_site_id.clone(),
            origin_site_id: plan_ctx.origin_site_id.clone(),
            upstream_thread_id: Some(parent_thread_id.to_string()),
            requested_by: Some(principal_fingerprint.to_string()),
            usage_subject: None,
            usage_subject_asserted_by: None,
        })
        .map_err(|e| LaunchAugmentationError::Threads(e.to_string()))?;

    // 7. Generate callback token.
    let ttl = ryeos_app::callback_token::compute_ttl(None);
    let child_provenance = provenance.clone_for_borrowed_child();
    let cap = state.callback_tokens.generate_with_context(
        &child_thread_id,
        project_path.to_path_buf(),
        ttl,
        Vec::new(), // augmentation children have no caps
        child_provenance,
        None,
        Some(format!("{target_kind}://{target_method}")),
        serde_json::Value::Null,
        0,
    );

    // 8. Mint thread auth token (runtime expects RYEOSD_THREAD_AUTH_TOKEN).
    let thread_auth = state.thread_auth.mint(
        &child_thread_id,
        principal_fingerprint.to_string(),
        vec!["execute".to_string()],
        ttl,
    );
    let tat_owned = thread_auth.token.clone();

    // 9-12. All post-mint subprocess work runs inside this guarded block.
    //        Any failure — envelope serialize, native executor resolution,
    //        env build, spawn join, or result parse — returns `Err`; the
    //        token revocation and failure-finalization below then run
    //        regardless, so a pre-spawn failure can no longer leak tokens
    //        or leave the child thread non-terminal.
    let spawn_outcome: Result<
        ryeos_runtime::method_wire::MethodCallResult,
        LaunchAugmentationError,
    > = async {
        let runtime_config = crate::dispatch::method_runtime_config_snapshot(
            target_kind,
            runtime_config,
            &engine_roots,
            state,
        )
        .map_err(|e| LaunchAugmentationError::RuntimeRegistry(format!("runtime config: {e}")))?;

        let envelope = ryeos_runtime::method_wire::MethodCallEnvelope {
            schema_version: 1,
            kind: target_kind.clone(),
            method: target_method.clone(),
            thread_id: child_thread_id.clone(),
            callback: ryeos_runtime::envelope::EnvelopeCallback {
                socket_path: state.config.uds_path.clone(),
                token: cap.token.clone(),
            },
            project_root: project_path.to_path_buf(),
            runtime_config,
            payload,
        };

        // Resolve the native executor path and spawn.
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
        let cache_root = state
            .config
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("state");
        let executor_path = crate::execution::launch::resolve_native_executor_path(
            &bundle_roots,
            &executor_ref,
            &cache_root,
            &engine.trust_store,
            ryeos_engine::resolution::TrustClass::TrustedBundle,
        )
        .map_err(|e| LaunchAugmentationError::RuntimeRegistry(e.to_string()))?;

        let executor_path_str = executor_path.to_string_lossy().to_string();
        let stdin_data = serde_json::to_string(&envelope)?;
        let roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
            &engine_roots,
            &state.config.app_root,
        );
        let envs = ryeos_app::process::build_subprocess_envs_with_roots(
            &std::collections::BTreeMap::new(),
            &[("RYEOSD_THREAD_AUTH_TOKEN".to_string(), tat_owned)],
            roots,
        )
        .map_err(|e| LaunchAugmentationError::Threads(format!("build subprocess env: {e}")))?;
        let subprocess_request = lillux::SubprocessRequest {
                cmd: executor_path_str,
                args: vec![],
                cwd: Some(project_path.to_string_lossy().into_owned()),
                envs,
                stdin_data: Some(stdin_data),
                timeout: 60.0,
            };
        let subprocess_request = ryeos_engine::subprocess_spec::sandbox_lillux_request(
            subprocess_request,
            &state.config.app_root,
            project_path,
            &format!("runtime:{target_kind}"),
            &child_thread_id,
        )
        .map_err(|error| LaunchAugmentationError::Threads(format!("sandbox: {error}")))?;
        let result = tokio::task::spawn_blocking(move || lillux::run(subprocess_request))
        .await
        .map_err(|e| LaunchAugmentationError::Threads(format!("spawn join: {e}")))?;

        if !result.success {
            return Err(LaunchAugmentationError::ChildBootstrap {
                kind: target_kind.clone(),
                method: target_method.clone(),
                exit_code: result.exit_code,
                stderr: result.stderr,
            });
        }

        let batch_result: ryeos_runtime::method_wire::MethodCallResult =
            serde_json::from_str(&result.stdout)?;

        // The runtime must echo back the dispatched kind/method.
        if batch_result.kind != *target_kind || batch_result.method != *target_method {
            return Err(LaunchAugmentationError::ChildFailed {
                kind: target_kind.clone(),
                method: target_method.clone(),
                error: None,
            });
        }

        if !batch_result.success {
            return Err(LaunchAugmentationError::ChildFailed {
                kind: target_kind.clone(),
                method: target_method.clone(),
                error: batch_result.error.map(Box::new),
            });
        }

        Ok(batch_result)
    }
    .await;

    // 13. Revoke callback + thread-auth tokens now that the subprocess has
    //     run (success or failure).
    state.callback_tokens.invalidate(&cap.token);
    state
        .callback_tokens
        .invalidate_for_thread(&child_thread_id);
    state.thread_auth.invalidate(&thread_auth.token);
    state.thread_auth.invalidate_for_thread(&child_thread_id);

    let batch_result = match spawn_outcome {
        Ok(br) => br,
        Err(e) => {
            crate::dispatch::finalize_method_thread_if_needed(
                state,
                &child_thread_id,
                "failed",
                None,
            );
            return Err(e);
        }
    };

    // 14. Extract rendered contexts + metadata and write them into the
    //     parent's composed view. A serialization failure here must also
    //     finalize the child as failed, not leave it dangling.
    let write_result = (|| -> Result<(), LaunchAugmentationError> {
        let output = batch_result.output.as_ref().ok_or_else(|| {
            LaunchAugmentationError::ProjectionInvariant {
                reason: format!(
                    "child {target_kind}/{target_method} succeeded without an output payload"
                ),
            }
        })?;
        let rendered_positions = extract_rendered_positions(output, &positions)?;
        resolution.composed.derived.insert(
            output_derived.clone(),
            serde_json::to_value(&rendered_positions)?,
        );
        let meta = extract_rendered_meta(output, &positions)?;
        resolution
            .composed
            .derived
            .insert(meta_output_derived.clone(), serde_json::to_value(&meta)?);
        Ok(())
    })();
    if let Err(e) = write_result {
        crate::dispatch::finalize_method_thread_if_needed(state, &child_thread_id, "failed", None);
        return Err(e);
    }

    // Success: the runtime self-finalized via its callback. Finalize as a
    // fallback only if that callback did not land.
    crate::dispatch::finalize_method_thread_if_needed(
        state,
        &child_thread_id,
        "completed",
        batch_result.output,
    );

    tracing::info!(
        kind = %target_kind,
        method = %target_method,
        positions = positions.len(),
        "compose_context_positions augmentation completed"
    );

    Ok(())
}

/// Read the position → refs map from the composed view's derived map.
fn read_positions(
    resolution: &ResolutionOutput,
    source_derived: &str,
) -> Result<BTreeMap<String, Vec<String>>, LaunchAugmentationError> {
    let value = resolution
        .composed
        .derived
        .get(source_derived)
        .ok_or_else(|| LaunchAugmentationError::ProjectionInvariant {
            reason: format!("derived field '{source_derived}' not found in composed view"),
        })?;

    let obj = value
        .as_object()
        .ok_or_else(|| LaunchAugmentationError::ProjectionInvariant {
            reason: format!("derived field '{source_derived}' must be an object, got {value}"),
        })?;

    let mut positions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, val) in obj {
        let arr = val
            .as_array()
            .ok_or_else(|| LaunchAugmentationError::ProjectionInvariant {
                reason: format!(
                    "derived field '{source_derived}': position '{key}' must be an array, got {val}"
                ),
            })?;
        let refs: Vec<String> = arr
            .iter()
            .map(|v| {
                v.as_str().map(|s| s.to_string()).ok_or_else(|| {
                    LaunchAugmentationError::ProjectionInvariant {
                        reason: format!(
                            "derived field '{source_derived}': position '{key}': \
                             ref must be a string, got {v}"
                        ),
                    }
                })
            })
            .collect::<Result<_, _>>()?;
        positions.insert(key.clone(), refs);
    }

    Ok(positions)
}

/// Validate that every ref in every position is a canonical ref
/// prefixed with `target_kind:`.
fn validate_canonical_refs(
    positions: &BTreeMap<String, Vec<String>>,
    target_kind: &str,
) -> Result<(), LaunchAugmentationError> {
    let expected_prefix = format!("{target_kind}:");
    for (position, refs) in positions {
        for r in refs {
            if !r.starts_with(&expected_prefix) {
                return Err(LaunchAugmentationError::BadRef {
                    position: position.clone(),
                    bad_ref: r.clone(),
                    expected_prefix,
                });
            }
        }
    }
    Ok(())
}

/// Write empty maps when there are no positions to render.
fn write_empty(resolution: &mut ResolutionOutput, output_derived: &str, meta_output_derived: &str) {
    resolution
        .composed
        .derived
        .insert(output_derived.to_string(), json!({}));
    resolution
        .composed
        .derived
        .insert(meta_output_derived.to_string(), json!({}));
}

fn rendered_output_object(
    output: &Value,
) -> Result<&serde_json::Map<String, Value>, LaunchAugmentationError> {
    output
        .get("rendered")
        .and_then(|v| v.as_object())
        .ok_or_else(|| LaunchAugmentationError::ProjectionInvariant {
            reason: format!("child output must contain object field `rendered`, got {output}"),
        })
}

fn validate_rendered_positions_exact(
    rendered: &serde_json::Map<String, Value>,
    expected_positions: &BTreeMap<String, Vec<String>>,
) -> Result<(), LaunchAugmentationError> {
    for position in rendered.keys() {
        if !expected_positions.contains_key(position) {
            return Err(LaunchAugmentationError::ProjectionInvariant {
                reason: format!("child output contained unexpected rendered position `{position}`"),
            });
        }
    }
    Ok(())
}

/// Extract rendered position strings from the child's output.
fn extract_rendered_positions(
    output: &Value,
    expected_positions: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, String>, LaunchAugmentationError> {
    let mut result = BTreeMap::new();
    let rendered = rendered_output_object(output)?;
    validate_rendered_positions_exact(rendered, expected_positions)?;

    for position in expected_positions.keys() {
        let data =
            rendered
                .get(position)
                .ok_or_else(|| LaunchAugmentationError::ProjectionInvariant {
                    reason: format!("child output missing rendered position `{position}`"),
                })?;
        let content = data
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LaunchAugmentationError::ProjectionInvariant {
                reason: format!(
                    "child output rendered position `{position}` missing string `content`"
                ),
            })?
            .to_string();
        result.insert(position.clone(), content);
    }

    Ok(result)
}

/// Extract per-position metadata from the child's output.
fn extract_rendered_meta(
    output: &Value,
    expected_positions: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, Value>, LaunchAugmentationError> {
    let mut result = BTreeMap::new();
    let rendered = rendered_output_object(output)?;
    validate_rendered_positions_exact(rendered, expected_positions)?;

    for position in expected_positions.keys() {
        let data =
            rendered
                .get(position)
                .ok_or_else(|| LaunchAugmentationError::ProjectionInvariant {
                    reason: format!("child output missing rendered position `{position}`"),
                })?;
        let composition = data.get("composition").ok_or_else(|| {
            LaunchAugmentationError::ProjectionInvariant {
                reason: format!(
                    "child output rendered position `{position}` missing `composition`"
                ),
            }
        })?;
        result.insert(position.clone(), composition.clone());
    }

    Ok(result)
}
