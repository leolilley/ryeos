//! Handler for `ComposeContextPositions` launch augmentation.
//!
//! This augmentation:
//! 1. Reads `source_derived` from the composed view (position → refs map).
//! 2. Validates that all refs are canonical (prefixed with `target_kind:`).
//! 3. Pre-resolves each unique ref via the engine resolution pipeline.
//! 4. Projects to a slim multi-root payload.
//! 5. Mints a child thread record + callback token.
//! 6. Spawns the target kind's runtime via lillux.
//! 7. Parses the child's `BatchOpResult` and writes `rendered_contexts`
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
pub async fn run(
    decl: &LaunchAugmentationDecl,
    resolution: &mut ResolutionOutput,
    parent_thread_id: &str,
    project_path: &Path,
    engine: &ryeos_engine::engine::Engine,
    plan_ctx: &ryeos_engine::contracts::PlanContext,
    principal_fingerprint: &str,
    state: &crate::state::AppState,
) -> Result<(), LaunchAugmentationError> {
    let (target_kind, target_op, source_derived, output_derived, meta_output_derived, per_position_budget) =
        match decl {
            LaunchAugmentationDecl::ComposeContextPositions {
                target_kind,
                target_op,
                source_derived,
                output_derived,
                meta_output_derived,
                per_position_budget,
            } => (
                target_kind,
                target_op,
                source_derived,
                output_derived,
                meta_output_derived,
                per_position_budget,
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
        let canonical = CanonicalRef::parse(r)
            .map_err(|e| LaunchAugmentationError::ParseRef(e.to_string()))?;
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
        per_root.insert(r.to_string(), resolution_output);
    }

    // 4. Project to slim payload.
    let payload = super::projection::build_compose_context_payload(
        &per_root,
        &positions,
        per_position_budget,
    )?;

    // 5. Look up the target kind's runtime.
    let verified_runtime = engine
        .runtimes
        .lookup_for(target_kind)
        .map_err(|_| LaunchAugmentationError::RuntimeRegistry(format!(
            "no runtime serves kind '{target_kind}'"
        )))?;

    let executor_ref = format!(
        "native:{}",
        crate::dispatch::strip_binary_ref_prefix(&verified_runtime.yaml.binary_ref)
            .map_err(|e| LaunchAugmentationError::RuntimeRegistry(e.to_string()))?
    );

    // 6. Mint child thread record under parent.
    let child_thread_id = crate::services::thread_lifecycle::new_thread_id();
    state
        .threads
        .create_thread(
            &crate::services::thread_lifecycle::ThreadCreateParams {
                thread_id: child_thread_id.clone(),
                chain_root_id: parent_thread_id.to_string(),
                kind: "system_task".to_string(),
                item_ref: format!("{target_kind}://{target_op}"),
                executor_ref: executor_ref.clone(),
                launch_mode: "inline".to_string(),
                current_site_id: plan_ctx.current_site_id.clone(),
                origin_site_id: plan_ctx.origin_site_id.clone(),
                upstream_thread_id: Some(parent_thread_id.to_string()),
                requested_by: Some(principal_fingerprint.to_string()),
            },
        )
        .map_err(|e| LaunchAugmentationError::Threads(e.to_string()))?;

    // 7. Generate callback token.
    let ttl = crate::execution::callback_token::compute_ttl(None);
    let cap = state.callback_tokens.generate(
        &child_thread_id,
        project_path.to_path_buf(),
        ttl,
        Vec::new(), // augmentation children have no caps
    );

    // 8. Mint thread auth token (runtime expects RYEOSD_THREAD_AUTH_TOKEN).
    let thread_auth = state.thread_auth.mint(
        &child_thread_id,
        principal_fingerprint.to_string(),
        vec!["execute".to_string()],
        ttl,
    );
    let tat_owned = thread_auth.token.clone();

    // 9. Build envelope.
    let envelope = ryeos_runtime::op_wire::BatchOpEnvelope {
        schema_version: 1,
        kind: target_kind.clone(),
        op: target_op.clone(),
        thread_id: child_thread_id.clone(),
        callback: ryeos_runtime::envelope::EnvelopeCallback {
            socket_path: state.config.uds_path.clone(),
            token: cap.token.clone(),
        },
        project_root: project_path.to_path_buf(),
        payload,
    };

    // 10. Resolve the native executor path and spawn.
    let system_roots: Vec<std::path::PathBuf> = engine_roots
        .ordered
        .iter()
        .filter(|r| r.space == ryeos_engine::contracts::ItemSpace::System)
        .map(|r| {
            r.ai_root
                .parent()
                .map(|pp| pp.to_path_buf())
                .unwrap_or(r.ai_root.clone())
        })
        .collect();
    let executor_path = crate::execution::launch::resolve_native_executor_path(
        &system_roots,
        &executor_ref,
        project_path,
        &engine.trust_store,
        ryeos_engine::resolution::TrustClass::TrustedSystem,
    )
    .map_err(|e| LaunchAugmentationError::RuntimeRegistry(e.to_string()))?;

    let executor_path_str = executor_path.to_string_lossy().to_string();
    let stdin_data = serde_json::to_string(&envelope)?;
    let envs = vec![
        ("RYEOSD_THREAD_AUTH_TOKEN".to_string(), tat_owned),
    ];
    let result = tokio::task::spawn_blocking(move || {
        lillux::run(lillux::SubprocessRequest {
            cmd: executor_path_str,
            args: vec![],
            cwd: None,
            envs,
            stdin_data: Some(stdin_data),
            timeout: 60.0,
        })
    })
    .await
    .map_err(|e| LaunchAugmentationError::Threads(format!("spawn join: {e}")))?;

    // 11. Invalidate callback + thread auth tokens.
    state.callback_tokens.invalidate(&cap.token);
    state.callback_tokens.invalidate_for_thread(&child_thread_id);
    state.thread_auth.invalidate(&thread_auth.token);
    state.thread_auth.invalidate_for_thread(&child_thread_id);

    if !result.success {
        let _ = state.threads.finalize_thread(
            &crate::dispatch::finalize_params(&child_thread_id, "failed", None),
        );
        return Err(LaunchAugmentationError::ChildBootstrap {
            kind: target_kind.clone(),
            op: target_op.clone(),
            exit_code: result.exit_code,
            stderr: result.stderr,
        });
    }

    // 12. Parse BatchOpResult.
    let batch_result: ryeos_runtime::op_wire::BatchOpResult =
        serde_json::from_str(&result.stdout)?;
    if !batch_result.success {
        let _ = state.threads.finalize_thread(
            &crate::dispatch::finalize_params(&child_thread_id, "failed", None),
        );
        return Err(LaunchAugmentationError::ChildFailed {
            kind: target_kind.clone(),
            op: target_op.clone(),
            error: batch_result.error,
        });
    }

    // 13. Extract rendered contexts from output.
    let output = batch_result.output.clone().unwrap_or_default();
    let rendered_positions = extract_rendered_positions(&output);

    // 14. Write into parent's composed view.
    resolution.composed.derived.insert(
        output_derived.clone(),
        serde_json::to_value(&rendered_positions)?,
    );

    // Extract metadata (per-position composition details).
    let meta = extract_rendered_meta(&output);
    resolution
        .composed
        .derived
        .insert(meta_output_derived.clone(), serde_json::to_value(&meta)?);

    // Finalize child thread as completed.
    let _ = state.threads.finalize_thread(
        &crate::dispatch::finalize_params(&child_thread_id, "completed", batch_result.output),
    );

    tracing::info!(
        kind = %target_kind,
        op = %target_op,
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

    let obj = value.as_object().ok_or_else(|| {
        LaunchAugmentationError::ProjectionInvariant {
            reason: format!(
                "derived field '{source_derived}' must be an object, got {value}"
            ),
        }
    })?;

    let mut positions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, val) in obj {
        let arr = val.as_array().ok_or_else(|| {
            LaunchAugmentationError::ProjectionInvariant {
                reason: format!(
                    "derived field '{source_derived}': position '{key}' must be an array, got {val}"
                ),
            }
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
fn write_empty(
    resolution: &mut ResolutionOutput,
    output_derived: &str,
    meta_output_derived: &str,
) {
    resolution
        .composed
        .derived
        .insert(output_derived.to_string(), json!({}));
    resolution
        .composed
        .derived
        .insert(meta_output_derived.to_string(), json!({}));
}

/// Extract rendered position strings from the child's output.
fn extract_rendered_positions(output: &Value) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    let rendered = output
        .get("rendered")
        .and_then(|v| v.as_object())
        .into_iter()
        .flatten();

    for (position, data) in rendered {
        let content = data
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        result.insert(position.clone(), content);
    }

    result
}

/// Extract per-position metadata from the child's output.
fn extract_rendered_meta(output: &Value) -> BTreeMap<String, Value> {
    let mut result = BTreeMap::new();
    let rendered = output
        .get("rendered")
        .and_then(|v| v.as_object())
        .into_iter()
        .flatten();

    for (position, data) in rendered {
        if let Some(composition) = data.get("composition") {
            result.insert(position.clone(), composition.clone());
        } else {
            result.insert(position.clone(), data.clone());
        }
    }

    result
}
