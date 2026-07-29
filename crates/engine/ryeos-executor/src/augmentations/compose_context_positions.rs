//! Handler for `ComposeContextPositions` launch augmentation.
//!
//! This augmentation:
//! 1. Reads `source_derived` from the composed view (position → refs map).
//! 2. Validates that all refs are canonical (prefixed with `target_kind:`).
//! 3. Pre-resolves each unique ref via the engine resolution pipeline.
//! 4. Resolves signed child authority and attempts the opt-in result cache.
//! 5. On a miss, projects to a slim multi-root payload.
//! 6. Mints a child thread record + callback token.
//! 7. Spawns the target kind's runtime via lillux.
//! 8. Parses the child's `MethodCallResult` and writes `rendered_contexts`
//!    + `rendered_contexts_meta` into the parent's composed view.
//!
//! Rule 1: the daemon never calls compose logic in-process.
//! Rule 2: all kind-specific decisions come from `decl.target_kind`,
//!          never hardcoded.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::kind_registry::LaunchAugmentationDecl;
use ryeos_engine::resolution::ResolutionOutput;
use serde_json::{Value, json};

use super::LaunchAugmentationError;
use super::compose_cache::{CacheLookup, CacheOutcome, CacheReason, CachedComposeProjection};

const AUGMENTATION_RUNTIME_TIMEOUT_SECS: u64 = 60;
const MAX_COMPOSE_AUTHORITY_RETRIES: usize = 3;

struct ComposeResolvedAuthority {
    request_snapshot: ryeos_engine::engine::EffectiveRequestAuthoritySnapshot,
    per_root: BTreeMap<String, ResolutionOutput>,
    referenced_closures: Vec<(
        ryeos_app::resolution_cache::ResolutionCacheKey,
        Arc<ryeos_app::resolution_cache::ResolvedClosure>,
    )>,
}

/// Run the `ComposeContextPositions` augmentation.
// Execution plumbing: each argument is a distinct leg of the thread's
// auth/provenance context, threaded verbatim — a struct would rename,
// not simplify. Restructure with a compiler in the loop, not here.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    decl: &LaunchAugmentationDecl,
    resolution: &mut ResolutionOutput,
    _prospective_thread_id: &str,
    project_path: &Path,
    engine: &ryeos_engine::engine::Engine,
    provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    plan_ctx: &ryeos_engine::contracts::PlanContext,
    principal_fingerprint: &str,
    state: &ryeos_app::state::AppState,
    launch_timings: Option<&ryeos_app::launch_stage_timings::LaunchStageTimings>,
    admitted_request_snapshot: Option<&Arc<ryeos_engine::engine::AdmittedRequestAuthoritySnapshot>>,
) -> Result<Vec<super::LaunchAugmentationAudit>, LaunchAugmentationError> {
    let (
        target_kind,
        target_method,
        source_derived,
        output_derived,
        meta_output_derived,
        per_position_budget,
    ) = match decl {
        LaunchAugmentationDecl::ComposeContextPositions {
            target_kind,
            target_method,
            source_derived,
            output_derived,
            meta_output_derived,
            per_position_budget,
        } => (
            target_kind,
            target_method,
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
        return Ok(Vec::new());
    }

    // The target implementation, not this consumer, owns authority to omit a
    // private method execution. Absence or mismatch is a signed-schema error,
    // never an inferred cache-safety fallback.
    let target_method_decl = engine
        .kinds
        .get(target_kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.augmentation_methods.get(target_method))
        .ok_or_else(|| {
            LaunchAugmentationError::RuntimeRegistry(format!(
                "target kind `{target_kind}` does not declare private augmentation method `{target_method}`"
            ))
        })?;
    let runtime_config = &target_method_decl.runtime_config;

    // 2. Validate canonical refs: every value must start with
    //    `<target_kind>:` (e.g. `knowledge:`).
    validate_canonical_refs(&positions, target_kind)?;

    // 3. Pre-resolve unique refs in-process via engine pipeline.
    let unique_refs: BTreeSet<&str> = positions
        .values()
        .flat_map(|v| v.iter().map(|s| s.as_str()))
        .collect();
    let subject_authority = provenance.subject_resolution_authority();
    let resolution_project_root = (!matches!(
        subject_authority,
        ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
    ))
    .then_some(project_path);
    let engine_roots = engine.resolution_roots(resolution_project_root.map(Path::to_path_buf));
    let node_config_root = engine.node_config_root();
    let resolution_materialization =
        ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
            provenance.request_engine(),
            plan_ctx,
            provenance,
        )
        .and_then(|binding| binding.resolution_materialization_binding())
        .map_err(|error| {
            LaunchAugmentationError::RuntimeRegistry(format!(
                "compose resolution materialization authority: {error:#}"
            ))
        })?;

    // 4. Resolve the signed child authority needed for both the cache key and
    //    an eventual subprocess launch.
    let verified_runtime = engine.runtimes.lookup_for(target_kind).map_err(|_| {
        LaunchAugmentationError::RuntimeRegistry(format!("no runtime serves kind '{target_kind}'"))
    })?;
    let runtime_protocol = crate::dispatch::require_method_runtime_protocol(
        engine,
        target_kind,
        verified_runtime,
        "augmentation",
    )
    .map_err(|error| LaunchAugmentationError::RuntimeRegistry(error.to_string()))?;
    let runtime_item_ref = verified_runtime.canonical_ref.clone();
    let runtime_item_ref_string = runtime_item_ref.to_string();

    let executor_ref = format!(
        "native:{}",
        crate::dispatch::strip_binary_ref_prefix(&verified_runtime.yaml.binary_ref)
            .map_err(|e| LaunchAugmentationError::RuntimeRegistry(e.to_string()))?
    );

    let bundle_roots: Vec<std::path::PathBuf> = engine_roots
        .ordered
        .iter()
        .filter(|r| r.space == ryeos_engine::contracts::ItemSpace::Bundle)
        .map(|r| {
            r.ai_root
                .parent()
                .map(|parent| parent.to_path_buf())
                .unwrap_or_else(|| r.ai_root.clone())
        })
        .collect();
    let cache_root = state
        .config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("state");
    let executor_attestation =
        verify_augmentation_executor_attestation(engine, &bundle_roots, &executor_ref).await?;

    // Rebuild the complete authority tuple and cold child after any failed
    // mutable proof. A stale key may never be paired with freshly resolved
    // closures/config or a child result produced under their old authority.
    let mut authority_retry_count = 0_usize;
    'execution: loop {
        let (
            request_snapshot,
            per_root,
            referenced_closures,
            runtime_config_snapshot,
            mut cache_fill,
        ) = 'authority: loop {
            let ComposeResolvedAuthority {
                request_snapshot,
                per_root,
                referenced_closures,
            } = resolve_compose_authority_off_thread(
                engine,
                resolution_project_root,
                &engine_roots,
                &unique_refs,
                target_kind,
                provenance,
                &resolution_materialization,
                state,
                admitted_request_snapshot,
            )
            .await?;
            let runtime_config_snapshot = load_runtime_config_snapshot_cached(
                target_kind,
                runtime_config,
                &engine_roots,
                engine,
                &request_snapshot,
                &subject_authority,
                &resolution_materialization,
                state,
            )
            .await
            .map_err(|error| {
                LaunchAugmentationError::RuntimeRegistry(format!("runtime config: {error}"))
            })?;
            let cache_key = build_cache_key(
                decl,
                &positions,
                &per_root,
                &request_snapshot,
                provenance,
                plan_ctx,
                principal_fingerprint,
                verified_runtime,
                runtime_protocol,
                &executor_ref,
                &executor_attestation,
                &runtime_config_snapshot,
            )?;
            loop {
                let cache_lookup = super::compose_cache::cache().begin(&cache_key);
                match cache_lookup {
                    CacheLookup::Hit {
                        projection,
                        entry_bytes,
                    } => {
                        if !validate_hit_snapshot_off_thread(
                            engine,
                            resolution_project_root,
                            &request_snapshot,
                            admitted_request_snapshot,
                        )
                        .await?
                            || !validate_referenced_closures_off_thread(
                                state,
                                &resolution_materialization,
                                &referenced_closures,
                            )
                            .await?
                            || !runtime_config_snapshot_is_valid(
                                &runtime_config_snapshot,
                                &engine_roots,
                                node_config_root.as_deref(),
                                &resolution_materialization,
                                state,
                            )
                            .await?
                        {
                            // Rebuild every mutable authority input before taking
                            // the cold path. Reusing the pre-mismatch roots or
                            // runtime config would only disguise a stale hit as a
                            // cold launch.
                            super::compose_cache::cache().discard_if_same(
                                &cache_key,
                                &projection,
                                CacheReason::AuthorityRevalidationFailed,
                            );
                            super::compose_cache::emit_metric(
                                CacheOutcome::Bypass,
                                CacheReason::AuthorityRevalidationFailed,
                                entry_bytes,
                                0,
                            );
                            authority_retry_count = authority_retry_count.saturating_add(1);
                            if authority_retry_count >= MAX_COMPOSE_AUTHORITY_RETRIES {
                                return Err(
                                    LaunchAugmentationError::AuthorityChangedDuringExecution {
                                        attempts: authority_retry_count,
                                    },
                                );
                            }
                            continue 'authority;
                        }
                        // A trust rejection is an authored authority failure, not
                        // a cache miss. It must remain fail-closed.
                        enforce_current_trust(&per_root, target_kind)?;
                        apply_projection(
                            resolution,
                            output_derived,
                            meta_output_derived,
                            &projection,
                        )?;
                        let audit =
                            super::compose_cache::record_hit_audit(&cache_key, &projection, false)
                                .map_err(LaunchAugmentationError::RuntimeRegistry)?;
                        super::compose_cache::emit_metric(
                            CacheOutcome::Hit,
                            CacheReason::Ready,
                            entry_bytes,
                            0,
                        );
                        return Ok(vec![audit]);
                    }
                    CacheLookup::Wait { pending } => {
                        let wait_started = Instant::now();
                        if let Some(projection) = pending
                            .wait()
                            .await
                            .map_err(LaunchAugmentationError::Shared)?
                        {
                            let wait_milliseconds =
                                u64::try_from(wait_started.elapsed().as_millis())
                                    .unwrap_or(u64::MAX);
                            let entry_bytes = serde_json::to_vec(projection.as_ref())
                                .map(|bytes| bytes.len())
                                .unwrap_or(0);
                            if !validate_hit_snapshot_off_thread(
                                engine,
                                resolution_project_root,
                                &request_snapshot,
                                admitted_request_snapshot,
                            )
                            .await?
                                || !validate_referenced_closures_off_thread(
                                    state,
                                    &resolution_materialization,
                                    &referenced_closures,
                                )
                                .await?
                                || !runtime_config_snapshot_is_valid(
                                    &runtime_config_snapshot,
                                    &engine_roots,
                                    node_config_root.as_deref(),
                                    &resolution_materialization,
                                    state,
                                )
                                .await?
                            {
                                // As with a ready hit, rebuild all mutable
                                // authority inputs before launching the cold child.
                                super::compose_cache::cache().discard_if_same(
                                    &cache_key,
                                    &projection,
                                    CacheReason::AuthorityRevalidationFailed,
                                );
                                super::compose_cache::emit_metric(
                                    CacheOutcome::Bypass,
                                    CacheReason::AuthorityRevalidationFailed,
                                    entry_bytes,
                                    wait_milliseconds,
                                );
                                authority_retry_count = authority_retry_count.saturating_add(1);
                                if authority_retry_count >= MAX_COMPOSE_AUTHORITY_RETRIES {
                                    return Err(
                                        LaunchAugmentationError::AuthorityChangedDuringExecution {
                                            attempts: authority_retry_count,
                                        },
                                    );
                                }
                                continue 'authority;
                            }
                            enforce_current_trust(&per_root, target_kind)?;
                            apply_projection(
                                resolution,
                                output_derived,
                                meta_output_derived,
                                &projection,
                            )?;
                            let audit = super::compose_cache::record_hit_audit(
                                &cache_key,
                                &projection,
                                true,
                            )
                            .map_err(LaunchAugmentationError::RuntimeRegistry)?;
                            super::compose_cache::emit_metric(
                                CacheOutcome::Hit,
                                CacheReason::SingleFlight,
                                entry_bytes,
                                wait_milliseconds,
                            );
                            return Ok(vec![audit]);
                        }
                        // The builder failed. Failures are never cached; race to
                        // become the next single-flight builder only if the
                        // authority used for this lookup is still current.
                        authority_retry_count = authority_retry_count.saturating_add(1);
                        if authority_retry_count >= MAX_COMPOSE_AUTHORITY_RETRIES {
                            return Err(LaunchAugmentationError::AuthorityChangedDuringExecution {
                                attempts: authority_retry_count,
                            });
                        }
                        if !validate_hit_snapshot_off_thread(
                            engine,
                            resolution_project_root,
                            &request_snapshot,
                            admitted_request_snapshot,
                        )
                        .await?
                            || !validate_referenced_closures_off_thread(
                                state,
                                &resolution_materialization,
                                &referenced_closures,
                            )
                            .await?
                            || !runtime_config_snapshot_is_valid(
                                &runtime_config_snapshot,
                                &engine_roots,
                                node_config_root.as_deref(),
                                &resolution_materialization,
                                state,
                            )
                            .await?
                        {
                            super::compose_cache::emit_metric(
                                CacheOutcome::Bypass,
                                CacheReason::AuthorityRevalidationFailed,
                                0,
                                u64::try_from(wait_started.elapsed().as_millis())
                                    .unwrap_or(u64::MAX),
                            );
                            continue 'authority;
                        }
                        continue;
                    }
                    CacheLookup::Build(fill) => {
                        super::compose_cache::emit_metric(
                            CacheOutcome::Miss,
                            CacheReason::Cold,
                            0,
                            0,
                        );
                        break 'authority (
                            request_snapshot,
                            per_root,
                            referenced_closures,
                            runtime_config_snapshot,
                            Some(fill),
                        );
                    }
                    CacheLookup::Bypass => {
                        super::compose_cache::emit_metric(
                            CacheOutcome::Bypass,
                            CacheReason::PendingCapacity,
                            0,
                            0,
                        );
                        break 'authority (
                            request_snapshot,
                            per_root,
                            referenced_closures,
                            runtime_config_snapshot,
                            None,
                        );
                    }
                }
            }
        };

        enum ColdComposeOutcome {
            Retry,
            Completed(CachedComposeProjection),
        }

        let cold_outcome: Result<ColdComposeOutcome, LaunchAugmentationError> = async {
        // The result cache missed. Only now open/repair the executable
        // materialization, then bind it back to the exact verified-chain
        // attestation that participated in the cache key.
        let executor =
            materialize_augmentation_executor(engine, &bundle_roots, &executor_ref, &cache_root)
                .await?;
        if !executor_attestation.matches_materialized(&executor) {
            return Err(LaunchAugmentationError::RuntimeRegistry(
                "materialized augmentation executor differs from its verified-chain attestation"
                    .to_string(),
            ));
        }

        // 5. Cache hits never need to construct the child wire payload.
        let payload = super::projection::build_compose_context_payload(
            &per_root,
            &positions,
            per_position_budget,
        )?;

        // 6. Mint the augmentation worker as an independently admitted executable
        // root. Launch augmentations are part of the authoritative pre-birth pass,
        // so the prospective parent thread deliberately does not exist yet. The
        // worker executes the verified runtime item directly and therefore uses
        // the runtime kind's schema-owned thread profile. It must cross the same
        // sealed root-admission boundary as every other executable root; the
        // generic `create_thread` child boundary correctly refuses root rows.
        let child_thread_id = ryeos_app::thread_lifecycle::new_thread_id();
        if let Some(timings) = launch_timings {
            timings.record_augmentation_child_thread_id(&child_thread_id);
        }
        let launch_claim = crate::execution::launch_claim::ThreadLaunchClaim::acquire_fresh(
            state,
            &child_thread_id,
        )
        .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?;
        let launch_owner = launch_claim
            .canonical_owner()
            .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?;
        let child_provenance = provenance.clone_for_borrowed_child();
        // RuntimeRegistry is built exclusively from verified bundle roots. Resolve
        // the admission subject without the caller's project overlay, then bind it
        // back to the registry entry's exact bundle and signature-stripped bytes.
        let mut runtime_plan_ctx = plan_ctx.clone();
        runtime_plan_ctx.project_context = ryeos_engine::contracts::ProjectContext::None;
        runtime_plan_ctx.subject_resolution_authority =
            ryeos_engine::contracts::SubjectResolutionAuthority::Projectless;
        let runtime_resolved = engine
            .resolve(&runtime_plan_ctx, &runtime_item_ref)
            .map_err(|error| LaunchAugmentationError::RuntimeRegistry(error.to_string()))?;
        let expected_bundle_ai = verified_runtime.bundle_root.join(ryeos_engine::AI_DIR);
        if runtime_resolved.source_space != ryeos_engine::contracts::ItemSpace::Bundle
            || !runtime_resolved
                .source_path
                .starts_with(&expected_bundle_ai)
            || runtime_resolved.raw_content_digest != verified_runtime.raw_content_digest
        {
            return Err(LaunchAugmentationError::RuntimeRegistry(format!(
            "resolved augmentation runtime `{runtime_item_ref}` does not match its verified registry authority"
        )));
        }
        let runtime_verified = engine
            .verify_attested(&runtime_plan_ctx, runtime_resolved)
            .map_err(|error| LaunchAugmentationError::RuntimeRegistry(error.to_string()))?;
        let child_thread_kind = engine
        .kinds
        .get(&runtime_item_ref.kind)
        .and_then(|schema| schema.execution())
        .and_then(|exec| exec.thread_profile.as_ref())
        .map(|tp| tp.name.as_str())
        .ok_or_else(|| {
            LaunchAugmentationError::RuntimeRegistry(format!(
                "runtime kind for augmentation worker `{runtime_item_ref}` must declare execution.thread_profile"
            ))
        })?;
        let root_admission = ryeos_app::thread_lifecycle::admit_verified_root_execution(
            child_provenance.request_engine(),
            plan_ctx,
            &runtime_plan_ctx,
            ryeos_app::thread_lifecycle::AdmittedProjectBinding::from_provenance(
                child_provenance.request_engine(),
                plan_ctx,
                &child_provenance,
            )
            .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?,
            runtime_verified,
            &state.node_history_policy,
            child_thread_kind.to_string(),
            BTreeMap::new(),
            None,
            None,
        )
        .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?;
        let admitted_request = root_admission
            .execution_request(
                ryeos_app::thread_lifecycle::RootExecutionRoute::RuntimeDescriptorExecutor(
                    &runtime_item_ref,
                ),
                "wait".to_string(),
                Value::Null,
            )
            .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?;
        state
            .threads
            .create_root_thread_with_id(
                &child_thread_id,
                &admitted_request,
                child_provenance.project_authority().clone(),
            )
            .map_err(|e| LaunchAugmentationError::Threads(e.to_string()))?;
        let mut lifecycle_owner =
            crate::execution::process_attachment::LifecycleOwnerGuard::new(state, &child_thread_id);

        // 7. Generate callback token.
        let ttl =
            ryeos_app::callback_token::launch_token_ttl(Some(AUGMENTATION_RUNTIME_TIMEOUT_SECS));
        let callback_project_path = provenance
            .state_root_override()
            .unwrap_or(project_path)
            .to_path_buf();
        let cap = state.callback_tokens.generate_with_context(
            &child_thread_id,
            callback_project_path.clone(),
            ttl,
            Vec::new(), // augmentation children have no caps
            child_provenance,
            None,
            Some(runtime_item_ref_string.clone()),
            verified_runtime.raw_content_digest.clone(),
            serde_json::Value::Null,
            0,
        );
        if !state
            .callback_tokens
            .set_launch_owner(&cap.token, launch_owner.clone())
        {
            return Err(LaunchAugmentationError::Threads(
                "augmentation callback capability disappeared before launch-owner binding".into(),
            ));
        }
        lifecycle_owner.track_callback_token(cap.token.clone());

        // 8. Mint thread-auth authority only when the verified protocol requests
        //    that source. The protocol also owns the eventual environment name.
        let needs_thread_auth =
            runtime_protocol
                .descriptor
                .env_injections
                .iter()
                .any(|injection| {
                    injection.source
                        == ryeos_engine::protocol_vocabulary::EnvInjectionSource::ThreadAuthToken
                });
        let thread_auth = needs_thread_auth.then(|| {
            state.thread_auth.mint(
                &child_thread_id,
                principal_fingerprint.to_string(),
                vec!["execute".to_string()],
                ttl,
            )
        });
        if let Some(thread_auth) = &thread_auth {
            lifecycle_owner.track_thread_auth_token(thread_auth.token.clone());
        }

        // 9-12. All post-mint subprocess work runs inside this guarded block.
        //        Any failure — envelope serialize, native executor resolution,
        //        env build, spawn join, or result parse — returns `Err`; the
        //        token revocation and failure-finalization below then run
        //        regardless, so a pre-spawn failure can no longer leak tokens
        //        or leave the child thread non-terminal.
        let spawn_outcome: Result<
        ryeos_engine::method_wire::MethodCallResult,
        LaunchAugmentationError,
    > = async {
        let envelope = ryeos_engine::method_wire::MethodCallEnvelope {
            schema_version: ryeos_engine::method_wire::METHOD_CALL_SCHEMA_VERSION,
            kind: target_kind.clone(),
            method: target_method.clone(),
            thread_id: child_thread_id.clone(),
            callback: ryeos_runtime::envelope::EnvelopeCallback {
                socket_path: state.config.uds_path.clone(),
                token: cap.token.clone(),
            },
            callback_project_path: callback_project_path.clone(),
            project_root: project_path.to_path_buf(),
            runtime_config: runtime_config_snapshot.values.clone(),
            payload,
        };

        let executor_path = executor.path.clone();
        let executor_path_str = executor_path
            .to_str()
            .ok_or_else(|| {
                LaunchAugmentationError::RuntimeRegistry(
                    "resolved executor path is not valid UTF-8".to_string(),
                )
            })?
            .to_owned();
        let isolation_verified_command = executor.verified_command.clone();
        let project_path_str = project_path
            .to_str()
            .ok_or_else(|| {
                LaunchAugmentationError::RuntimeRegistry(
                    "augmentation project path is not valid UTF-8".to_string(),
                )
            })?
            .to_owned();
        let stdin_data = ryeos_engine::protocols::build_method_call_stdin(
            &runtime_protocol.descriptor,
            &envelope,
        )
        .map_err(|error| {
            LaunchAugmentationError::RuntimeRegistry(format!(
                "method protocol '{}' stdin: {error}",
                runtime_protocol.canonical_ref
            ))
        })?;
        let stdin_data = String::from_utf8(stdin_data)
            .map_err(|error| LaunchAugmentationError::RuntimeRegistry(error.to_string()))?;
        let roots = ryeos_app::env_contract::DaemonRootEnv::from_resolution_roots(
            &engine_roots,
            &state.config.app_root,
        )
        .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?;
        let callback_socket_requested = runtime_protocol
            .descriptor
            .env_injections
            .iter()
            .any(|injection| {
                injection.source
                    == ryeos_engine::protocol_vocabulary::EnvInjectionSource::CallbackSocketPath
            });
        let callback_ipc_requested = runtime_protocol.descriptor.callback_channel
            != ryeos_engine::protocol_vocabulary::CallbackChannel::None
            || callback_socket_requested;
        let env_request = ryeos_engine::subprocess_spec::SubprocessBuildRequest {
            cmd: executor_path,
            args: Vec::new(),
            cwd: project_path.to_path_buf(),
            timeout: std::time::Duration::from_secs(AUGMENTATION_RUNTIME_TIMEOUT_SECS),
            item_ref: runtime_item_ref.clone(),
            thread_id: child_thread_id.clone(),
            project_path: project_path.to_path_buf(),
            acting_principal: principal_fingerprint.to_string(),
            cas_root: state
                .state_store
                .cas_root()
                .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?,
            callback_token: Some(cap.token.clone()),
            callback_socket_path: if callback_socket_requested {
                Some(
                    state
                        .config
                        .uds_path
                        .to_str()
                        .ok_or_else(|| {
                            LaunchAugmentationError::RuntimeRegistry(
                                "callback socket path is not valid UTF-8".to_string(),
                            )
                        })?
                        .to_owned(),
                )
            } else {
                None
            },
            callback_project_path: Some(callback_project_path.clone()),
            thread_auth_token: thread_auth.as_ref().map(|auth| auth.token.clone()),
            params: envelope.payload.clone(),
            resolution_output: None,
        };
        let protocol_bindings = runtime_protocol
            .descriptor
            .env_injections
            .iter()
            .map(|injection| {
                let value = ryeos_engine::protocol_vocabulary::produce_env_value(
                    injection.source,
                    &env_request,
                )
                .map_err(|error| {
                    LaunchAugmentationError::RuntimeRegistry(format!(
                        "protocol '{}' env injection '{}' is unavailable for augmentation runtime '{}': {error}",
                        runtime_protocol.canonical_ref,
                        injection.name,
                        runtime_item_ref,
                    ))
                })?;
                Ok(ryeos_app::env_contract::EnvBinding::new(
                    injection.name.clone(),
                    value,
                    ryeos_app::env_contract::EnvSourceDetail::ProtocolInjection {
                        source: injection.source,
                    },
                ))
            })
            .collect::<Result<Vec<_>, LaunchAugmentationError>>()?;
        let envs = ryeos_app::env_contract::EnvContractBuilder::new()
            .with_base_allowlist(std::env::vars_os().map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            }))
            .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?
            .with_daemon_roots(roots)
            .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?
            .with_typed_bindings(protocol_bindings)
            .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?
            .build();
        let subprocess_request = lillux::SubprocessRequest {
            cmd: executor_path_str,
            argv0: None,
            args: vec![],
            cwd: Some(project_path_str),
            envs,
            stdin_data: Some(stdin_data),
            timeout: AUGMENTATION_RUNTIME_TIMEOUT_SECS as f64,
            limits: None,
            inherited_fds: Vec::new(),
            supervised_status: None,
        };
        let live_access = provenance
            .isolation_live_access_authority()
            .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?;
        let applied = state
            .isolation
            .apply_awaiting_attachment_with_provenance(
                subprocess_request,
                ryeos_engine::isolation::IsolationLaunchContext {
                    project_path,
                    project_authority: provenance.isolation_project_authority(),
                    live_access: live_access.as_ref(),
                    state_root: provenance.state_root_override(),
                    checkpoint_dir: None,
                    daemon_socket_path: callback_ipc_requested
                        .then_some(state.config.uds_path.as_path()),
                    bundle_roots: &bundle_roots,
                    node_trusted_keys_dir: Some(&state.config.runtime_root().trusted_keys_dir()),
                    verified_code: &[],
                    verified_command: Some(&isolation_verified_command),
                    item_ref: &runtime_item_ref_string,
                    thread_id: &child_thread_id,
                },
            )
            .map_err(|error| LaunchAugmentationError::Threads(format!("isolation: {error}")))?;
        state
            .state_store
            .seed_isolation_provenance(&child_thread_id, applied.provenance)
            .map_err(|error| {
                LaunchAugmentationError::Threads(format!(
                    "persist isolation provenance: {error}"
                ))
            })?;
        let subprocess_request = applied.request;
        let workspace_lifeline = provenance.workspace_lifeline();
        let process_state = state.clone();
        let process_thread_id = child_thread_id.clone();
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
        .map_err(|e| LaunchAugmentationError::Threads(format!("spawn join: {e}")))?
        .map_err(|e| LaunchAugmentationError::Threads(format!("spawn/attach: {e:#}")))?;

        if !result.success {
            return Err(LaunchAugmentationError::ChildBootstrap {
                kind: target_kind.clone(),
                method: target_method.clone(),
                exit_code: result.exit_code,
                stderr: result.stderr,
            });
        }

        let batch_result = crate::dispatch::decode_method_runtime_result(
            runtime_protocol,
            &result.stdout,
        )
        .map_err(LaunchAugmentationError::RuntimeRegistry)?;

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
        if let Some(thread_auth) = &thread_auth {
            state.thread_auth.invalidate(&thread_auth.token);
        }
        state.thread_auth.invalidate_for_thread(&child_thread_id);

        let batch_result = match spawn_outcome {
            Ok(br) => br,
            Err(e) => {
                match crate::dispatch::finalize_method_thread_if_needed(
                    state,
                    &child_thread_id,
                    &launch_owner,
                    "failed",
                    None,
                ) {
                    Ok(_) => lifecycle_owner.disarm(),
                    Err(cleanup_error) => tracing::error!(
                        child_thread_id,
                        execution_error = %e,
                        cleanup_error = %cleanup_error,
                        "augmentation child execution and cleanup both failed"
                    ),
                }
                return Err(e);
            }
        };

        // The live project and node-local runtime config may change while the
        // child runs. Prove the complete authority tuple again before its output
        // can affect the parent or enter the cache. A mismatch invalidates this
        // child result and restarts from fresh authority; it is never applied or
        // published under the old key.
        if !validate_hit_snapshot_off_thread(
            engine,
            resolution_project_root,
            &request_snapshot,
            admitted_request_snapshot,
        )
        .await?
            || !validate_referenced_closures_off_thread(
                state,
                &resolution_materialization,
                &referenced_closures,
            )
            .await?
            || !runtime_config_snapshot_is_valid(
                &runtime_config_snapshot,
                &engine_roots,
                node_config_root.as_deref(),
                &resolution_materialization,
                state,
            )
            .await?
        {
            super::compose_cache::emit_metric(
                CacheOutcome::Bypass,
                CacheReason::AuthorityRevalidationFailed,
                0,
                0,
            );
            match crate::dispatch::finalize_method_thread_if_needed(
                state,
                &child_thread_id,
                &launch_owner,
                "failed",
                None,
            ) {
                Ok(_) => lifecycle_owner.disarm(),
                Err(cleanup_error) => {
                    return Err(LaunchAugmentationError::Threads(format!(
                    "compose authority changed during child execution and failed to finalize stale child {child_thread_id}: {cleanup_error}"
                )));
                }
            }
            authority_retry_count = authority_retry_count.saturating_add(1);
            if authority_retry_count >= MAX_COMPOSE_AUTHORITY_RETRIES {
                return Err(LaunchAugmentationError::AuthorityChangedDuringExecution {
                    attempts: authority_retry_count,
                });
            }
            return Ok(ColdComposeOutcome::Retry);
        }

        // 14. Extract rendered contexts + metadata and write them into the
        //     parent's composed view. A serialization failure here must also
        //     finalize the child as failed, not leave it dangling.
        let projection_result = (|| -> Result<CachedComposeProjection, LaunchAugmentationError> {
            let output = batch_result.output.as_ref().ok_or_else(|| {
                LaunchAugmentationError::ProjectionInvariant {
                    reason: format!(
                        "child {target_kind}/{target_method} succeeded without an output payload"
                    ),
                }
            })?;
            normalized_projection(output, &positions)
        })();
        let projection = match projection_result {
            Ok(projection) => projection,
            Err(e) => {
                match crate::dispatch::finalize_method_thread_if_needed(
                    state,
                    &child_thread_id,
                    &launch_owner,
                    "failed",
                    None,
                ) {
                    Ok(_) => lifecycle_owner.disarm(),
                    Err(cleanup_error) => tracing::error!(
                        child_thread_id,
                        projection_error = %e,
                        cleanup_error = %cleanup_error,
                        "augmentation projection and child cleanup both failed"
                    ),
                }
                return Err(e);
            }
        };

        // Normalization is deliberately side-effect free. Re-prove the complete
        // referenced closure immediately before the projection can mutate the
        // parent; a source/shadow change during result decoding invalidates the
        // child just like a change while it was running.
        if !validate_hit_snapshot_off_thread(
            engine,
            resolution_project_root,
            &request_snapshot,
            admitted_request_snapshot,
        )
        .await?
            || !validate_referenced_closures_off_thread(
                state,
                &resolution_materialization,
                &referenced_closures,
            )
            .await?
            || !runtime_config_snapshot_is_valid(
                &runtime_config_snapshot,
                &engine_roots,
                node_config_root.as_deref(),
                &resolution_materialization,
                state,
            )
            .await?
        {
            match crate::dispatch::finalize_method_thread_if_needed(
                state,
                &child_thread_id,
                &launch_owner,
                "failed",
                None,
            ) {
                Ok(_) => lifecycle_owner.disarm(),
                Err(cleanup_error) => {
                    return Err(LaunchAugmentationError::Threads(format!(
                    "compose authority changed before projection and failed to finalize stale child {child_thread_id}: {cleanup_error}"
                )));
                }
            }
            authority_retry_count = authority_retry_count.saturating_add(1);
            if authority_retry_count >= MAX_COMPOSE_AUTHORITY_RETRIES {
                return Err(LaunchAugmentationError::AuthorityChangedDuringExecution {
                    attempts: authority_retry_count,
                });
            }
            return Ok(ColdComposeOutcome::Retry);
        }

        if let Err(e) =
            apply_projection(resolution, output_derived, meta_output_derived, &projection)
        {
            match crate::dispatch::finalize_method_thread_if_needed(
                state,
                &child_thread_id,
                &launch_owner,
                "failed",
                None,
            ) {
                Ok(_) => lifecycle_owner.disarm(),
                Err(cleanup_error) => tracing::error!(
                    child_thread_id,
                    projection_error = %e,
                    cleanup_error = %cleanup_error,
                    "augmentation projection and child cleanup both failed"
                ),
            }
            return Err(e);
        }
        // Success: the daemon publishes terminal child state only after the method
        // result and its parent-view projection have both been validated.
        let finalization = crate::dispatch::finalize_method_thread_if_needed(
            state,
            &child_thread_id,
            &launch_owner,
            "completed",
            batch_result.output,
        )
        .map_err(|error| LaunchAugmentationError::Threads(error.to_string()))?;
        lifecycle_owner.disarm();
        match finalization {
        crate::dispatch::MethodFinalizeOutcome::Finalized => {}
        crate::dispatch::MethodFinalizeOutcome::AlreadyTerminal => {
            return Err(LaunchAugmentationError::Threads(format!(
                "augmentation child {child_thread_id} became terminal before its validated projection was committed"
            )));
        }
        crate::dispatch::MethodFinalizeOutcome::DurableStopSettled => {
            return Err(LaunchAugmentationError::Threads(format!(
                "augmentation child {child_thread_id} completed after a durable stop won"
            )));
        }
        crate::dispatch::MethodFinalizeOutcome::PreservedForShutdown => {
            return Err(LaunchAugmentationError::Threads(format!(
                "augmentation child {child_thread_id} was preserved for daemon shutdown recovery"
            )));
        }
    }
        Ok(ColdComposeOutcome::Completed(projection))
        }
        .await;

        match cold_outcome {
            Ok(ColdComposeOutcome::Retry) => {
                if let Some(fill) = cache_fill.take() {
                    fill.cancel();
                }
                continue 'execution;
            }
            Ok(ColdComposeOutcome::Completed(projection)) => {
                if let Some(fill) = cache_fill.take() {
                    match target_method_decl.execution_cache {
                        ryeos_engine::kind_registry::AugmentationMethodCacheContract::ContentAddressed => {
                            fill.complete(projection);
                        }
                    }
                }
                tracing::info!(
                    kind = %target_kind,
                    method = %target_method,
                    positions = positions.len(),
                    "compose_context_positions augmentation completed"
                );
                return Ok(Vec::new());
            }
            Err(error) => {
                return Err(match cache_fill.take() {
                    Some(fill) => LaunchAugmentationError::Shared(fill.fail(error)),
                    None => error,
                });
            }
        }
    }
}

async fn resolve_compose_authority_off_thread(
    engine: &ryeos_engine::engine::Engine,
    resolution_project_root: Option<&Path>,
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    unique_refs: &BTreeSet<&str>,
    target_kind: &str,
    provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    resolution_materialization: &ryeos_app::resolution_cache::ResolutionMaterializationBinding,
    state: &ryeos_app::state::AppState,
    admitted_request_snapshot: Option<&Arc<ryeos_engine::engine::AdmittedRequestAuthoritySnapshot>>,
) -> Result<ComposeResolvedAuthority, LaunchAugmentationError> {
    let engine = engine.clone();
    let resolution_project_root = resolution_project_root.map(Path::to_path_buf);
    let engine_roots = engine_roots.clone();
    let unique_refs = unique_refs
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    let target_kind = target_kind.to_owned();
    let provenance = provenance.clone();
    let resolution_materialization = resolution_materialization.clone();
    let state = state.clone();
    let admitted_request_snapshot = admitted_request_snapshot.cloned();
    tokio::task::spawn_blocking(move || {
        let borrowed_refs = unique_refs
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        resolve_compose_authority(
            &engine,
            resolution_project_root.as_deref(),
            &engine_roots,
            &borrowed_refs,
            &target_kind,
            &provenance,
            &resolution_materialization,
            &state,
            admitted_request_snapshot.as_deref(),
        )
    })
    .await
    .map_err(|error| {
        LaunchAugmentationError::RuntimeRegistry(format!(
            "compose authority blocking worker failed: {error}"
        ))
    })?
}

fn resolve_compose_authority(
    engine: &ryeos_engine::engine::Engine,
    resolution_project_root: Option<&Path>,
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    unique_refs: &BTreeSet<&str>,
    target_kind: &str,
    provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    resolution_materialization: &ryeos_app::resolution_cache::ResolutionMaterializationBinding,
    state: &ryeos_app::state::AppState,
    admitted_request_snapshot: Option<&ryeos_engine::engine::AdmittedRequestAuthoritySnapshot>,
) -> Result<ComposeResolvedAuthority, LaunchAugmentationError> {
    let full_request_snapshot = match admitted_request_snapshot {
        Some(admitted) => engine.effective_request_snapshot_under_admitted_authority(
            resolution_project_root.ok_or_else(|| {
                LaunchAugmentationError::RuntimeRegistry(
                    "admitted compose request snapshot has no project root".to_string(),
                )
            })?,
            admitted,
        ),
        None => engine
            .effective_request_snapshot(
                resolution_project_root,
                &provenance.subject_resolution_authority(),
            )
            .map(Arc::new),
    }
    .map_err(|error| {
        LaunchAugmentationError::RuntimeRegistry(format!("request snapshot: {error}"))
    })?;
    let request_snapshot = ryeos_engine::engine::EffectiveRequestAuthoritySnapshot {
        trust_store: full_request_snapshot.trust_store.clone(),
        registry_fingerprint: full_request_snapshot.registry_fingerprint.clone(),
        effective_trust_identity: full_request_snapshot.effective_trust_identity.clone(),
        request_engine_generation_identity: full_request_snapshot
            .request_engine_generation_identity
            .clone(),
        subject_resolution_authority: full_request_snapshot.subject_resolution_authority.clone(),
    };
    let mut per_root = BTreeMap::new();
    let mut referenced_closures = Vec::with_capacity(unique_refs.len());
    let subject_authority = provenance.subject_resolution_authority();
    let resolution_session = resolution_materialization
        .validate_once()
        .map_err(|error| {
            LaunchAugmentationError::RuntimeRegistry(format!(
                "compose materialization authority: {error:#}"
            ))
        })?;
    for requested_ref in unique_refs {
        let canonical = CanonicalRef::parse(requested_ref)
            .map_err(|error| LaunchAugmentationError::ParseRef(error.to_string()))?;
        let cache_key = ryeos_app::resolution_cache::build_resolution_cache_key_from_snapshot(
            engine,
            &canonical,
            resolution_project_root,
            &request_snapshot,
        );
        let build_closure = || {
            let resolution = match admitted_request_snapshot {
                Some(admitted) => {
                    let project_root = resolution_project_root.ok_or_else(|| {
                        LaunchAugmentationError::RuntimeRegistry(
                            "admitted compose resolution has no project root".to_string(),
                        )
                    })?;
                    let project_content =
                        admitted
                            .project_content_for_root(project_root)
                            .map_err(|error| {
                                LaunchAugmentationError::RuntimeRegistry(format!(
                                    "admitted compose project content: {error}"
                                ))
                            })?;
                    ryeos_engine::resolution::run_resolution_pipeline_with_probes_under_project_authority(
                        &canonical,
                        &engine.kinds,
                        &full_request_snapshot.parser_dispatcher,
                        engine_roots,
                        &full_request_snapshot.trust_store,
                        &engine.composers,
                        project_root,
                        project_content,
                    )
                }
                None => ryeos_engine::resolution::run_resolution_pipeline_with_probes(
                    &canonical,
                    &engine.kinds,
                    &full_request_snapshot.parser_dispatcher,
                    engine_roots,
                    &full_request_snapshot.trust_store,
                    &engine.composers,
                ),
            };
            let (resolution_output, probed_absent) =
                resolution.map_err(|source| LaunchAugmentationError::ResolutionFailed {
                    ref_: (*requested_ref).to_string(),
                    source,
                })?;
            let closure = std::sync::Arc::new(
                ryeos_app::resolution_cache::ResolvedClosure::new(
                    resolution_output,
                    subject_authority.clone(),
                    resolution_project_root.map(Path::to_path_buf),
                    (!matches!(
                        subject_authority,
                        ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
                    ))
                    .then(|| provenance.workspace_lifeline())
                    .flatten(),
                )
                .map_err(|error| {
                    LaunchAugmentationError::RuntimeRegistry(format!(
                        "retain compose referenced closure authority: {error:#}"
                    ))
                })?,
            );
            Ok::<_, LaunchAugmentationError>((closure, probed_absent))
        };
        let mut mutable_authority_races = 0_usize;
        let closure = loop {
            match resolution_session
                .begin(&state.resolution_cache, &cache_key)
                .map_err(|error| {
                    LaunchAugmentationError::RuntimeRegistry(format!(
                        "compose resolution cache authority: {error:#}"
                    ))
                })? {
                ryeos_app::resolution_cache::ResolutionLookup::Hit(cached) => {
                    ryeos_app::resolution_cache::emit_resolution_cache_metric(
                        ryeos_app::resolution_cache::ResolutionCacheMetric::Compose,
                        ryeos_app::resolution_cache::ResolutionCachePhase::Compose,
                        ryeos_app::resolution_cache::ResolutionCacheOutcome::Hit,
                        Some(ryeos_app::resolution_cache::ResolutionCacheReason::Ready),
                        0,
                    );
                    break cached;
                }
                ryeos_app::resolution_cache::ResolutionLookup::Wait(wait) => {
                    ryeos_app::resolution_cache::emit_resolution_cache_metric(
                        ryeos_app::resolution_cache::ResolutionCacheMetric::Compose,
                        ryeos_app::resolution_cache::ResolutionCachePhase::Compose,
                        ryeos_app::resolution_cache::ResolutionCacheOutcome::SingleFlightWait,
                        None,
                        0,
                    );
                    match wait.wait_blocking().map_err(|error| {
                        error
                            .downcast::<LaunchAugmentationError>()
                            .map(LaunchAugmentationError::Shared)
                            .unwrap_or_else(|| {
                                LaunchAugmentationError::RuntimeRegistry(format!(
                                    "compose resolution failed: {error}"
                                ))
                            })
                    })? {
                        Some(published) => break published,
                        None => {
                            mutable_authority_races = mutable_authority_races.saturating_add(1);
                        }
                    }
                }
                ryeos_app::resolution_cache::ResolutionLookup::Build(fill) => {
                    ryeos_app::resolution_cache::emit_resolution_cache_metric(
                        ryeos_app::resolution_cache::ResolutionCacheMetric::Compose,
                        ryeos_app::resolution_cache::ResolutionCachePhase::Compose,
                        ryeos_app::resolution_cache::ResolutionCacheOutcome::Miss,
                        None,
                        0,
                    );
                    let (closure, probed_absent) = match build_closure() {
                        Ok(built) => built,
                        Err(error) => {
                            return Err(LaunchAugmentationError::Shared(
                                fill.fail_typed_error(error),
                            ));
                        }
                    };
                    let published = fill.complete(closure, probed_absent).map_err(|error| {
                        LaunchAugmentationError::RuntimeRegistry(format!(
                            "compose resolution failed: {error}"
                        ))
                    })?;
                    if let Some(published) = published {
                        break published;
                    }
                    mutable_authority_races = mutable_authority_races.saturating_add(1);
                }
                ryeos_app::resolution_cache::ResolutionLookup::Bypass => {
                    ryeos_app::resolution_cache::emit_resolution_cache_metric(
                        ryeos_app::resolution_cache::ResolutionCacheMetric::Compose,
                        ryeos_app::resolution_cache::ResolutionCachePhase::Compose,
                        ryeos_app::resolution_cache::ResolutionCacheOutcome::Bypass,
                        Some(ryeos_app::resolution_cache::ResolutionCacheReason::PendingCapacity),
                        0,
                    );
                    break build_closure()?.0;
                }
            }
            if mutable_authority_races
                > ryeos_app::resolution_cache::MAX_MUTABLE_AUTHORITY_RACE_RETRIES
            {
                return Err(LaunchAugmentationError::RuntimeRegistry(
                    "compose resolution changed repeatedly under live project authority"
                        .to_string(),
                ));
            }
        };
        let resolution_output = closure.output();
        crate::execution::launch::enforce_effective_trust(
            resolution_output.effective_trust_class,
            requested_ref,
            target_kind,
        )
        .map_err(|error| LaunchAugmentationError::EffectiveTrustRejected(error.to_string()))?;
        per_root.insert((*requested_ref).to_string(), resolution_output.clone());
        referenced_closures.push((cache_key, closure));
    }
    Ok(ComposeResolvedAuthority {
        request_snapshot,
        per_root,
        referenced_closures,
    })
}

async fn validate_referenced_closures_off_thread(
    state: &ryeos_app::state::AppState,
    binding: &ryeos_app::resolution_cache::ResolutionMaterializationBinding,
    referenced_closures: &[(
        ryeos_app::resolution_cache::ResolutionCacheKey,
        Arc<ryeos_app::resolution_cache::ResolvedClosure>,
    )],
) -> Result<bool, LaunchAugmentationError> {
    let state = state.clone();
    let binding = binding.clone();
    let referenced_closures = referenced_closures.to_vec();
    tokio::task::spawn_blocking(move || {
        let borrowed = referenced_closures
            .iter()
            .map(|(key, closure)| (key, closure.as_ref()))
            .collect::<Vec<_>>();
        state
            .resolution_cache
            .validate_admitted_closures_status(&binding, &borrowed)
    })
    .await
    .map_err(|error| {
        LaunchAugmentationError::RuntimeRegistry(format!(
            "compose referenced-closure validation worker failed: {error}"
        ))
    })?
    .map_err(|error| {
        LaunchAugmentationError::RuntimeRegistry(format!(
            "compose referenced-closure validation failed: {error:#}"
        ))
    })
    .and_then(|status| match status {
        ryeos_app::resolution_cache::AdmittedClosureValidation::Current => Ok(true),
        ryeos_app::resolution_cache::AdmittedClosureValidation::MutableAuthorityChanged => {
            Ok(false)
        }
        ryeos_app::resolution_cache::AdmittedClosureValidation::ImmutableAuthorityMismatch => {
            Err(LaunchAugmentationError::RuntimeRegistry(
                "compose referenced closure contradicts immutable admitted authority".to_owned(),
            ))
        }
    })
}

fn runtime_config_snapshot_cache() -> &'static crate::resolved_config_cache::SnapshotCache<
    crate::dispatch::MethodRuntimeConfigSnapshot,
> {
    static CACHE: OnceLock<
        crate::resolved_config_cache::SnapshotCache<crate::dispatch::MethodRuntimeConfigSnapshot>,
    > = OnceLock::new();
    CACHE.get_or_init(crate::resolved_config_cache::SnapshotCache::default)
}

async fn load_runtime_config_snapshot_cached(
    kind: &str,
    requirements: &BTreeMap<String, ryeos_engine::kind_registry::MethodRuntimeConfigRequirement>,
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    engine: &ryeos_engine::engine::Engine,
    request_snapshot: &ryeos_engine::engine::EffectiveRequestAuthoritySnapshot,
    subject_authority: &ryeos_engine::contracts::SubjectResolutionAuthority,
    materialization: &ryeos_app::resolution_cache::ResolutionMaterializationBinding,
    state: &ryeos_app::state::AppState,
) -> Result<Arc<crate::dispatch::MethodRuntimeConfigSnapshot>, crate::dispatch_error::DispatchError>
{
    let kind = kind.to_owned();
    let requirements = requirements.clone();
    let engine_roots = engine_roots.clone();
    let node_config_root = engine.node_config_root();
    let key_has_node_config_layer = node_config_root.is_some();
    let trusted_keys_dir = state.config.runtime_root().trusted_keys_dir();
    let generation = engine.registered_bundle_generation_fingerprint();
    let generation_epoch = engine.registered_bundle_generation_epoch();
    let key_subject_authority = subject_authority.clone();
    let request_engine_generation_identity =
        request_snapshot.request_engine_generation_identity.clone();
    let registry_fingerprint = request_snapshot.registry_fingerprint.clone();
    let effective_trust_identity = request_snapshot.effective_trust_identity.clone();
    let node_trust_identity =
        ryeos_runtime::verified_loader::VerifiedLoader::node_trust_identity_from(&trusted_keys_dir)
            .map_err(crate::dispatch_error::DispatchError::Internal)?;
    let key_kind = kind.clone();
    let key_requirements = requirements.clone();
    let key_roots = engine_roots.clone();
    let key = tokio::task::spawn_blocking(move || {
        let include_live_paths = key_subject_authority.permits_mutable_revalidation();
        let roots = key_roots
            .ordered
            .iter()
            .map(|root| {
                serde_json::json!({
                    "label": root.label,
                    "space": root.space,
                    "live_path": include_live_paths.then(|| root.ai_root.clone()),
                })
            })
            .collect::<Vec<_>>();
        let value = serde_json::json!({
            "schema_version": 1,
            "kind": &key_kind,
            "requirements": &key_requirements,
            "subject_authority": &key_subject_authority,
            "request_engine_generation_identity": &request_engine_generation_identity,
            "registry_fingerprint": &registry_fingerprint,
            "effective_trust_identity": &effective_trust_identity,
            "node_trust_identity": &node_trust_identity,
            "node_config_layer": key_has_node_config_layer,
            "roots": roots,
        });
        let canonical = lillux::canonical_json(&value)?;
        Ok::<_, anyhow::Error>(crate::resolved_config_cache::SnapshotCacheKey {
            namespace: "method_runtime",
            retirement_scope: key_kind,
            generation,
            generation_epoch,
            identity: lillux::sha256_hex(canonical.as_bytes()),
        })
    })
    .await
    .map_err(|error| {
        crate::dispatch_error::DispatchError::Internal(anyhow::anyhow!(
            "runtime config cache-key worker failed: {error}"
        ))
    })?
    .map_err(crate::dispatch_error::DispatchError::Internal)?;
    let materialization = materialization.clone();
    let mut authority_retries = 0_usize;
    loop {
        match runtime_config_snapshot_cache().begin(key.clone()) {
            crate::resolved_config_cache::Lookup::Hit { value, entry_bytes } => {
                if cached_method_config_is_valid(
                    &value,
                    &engine_roots,
                    node_config_root.as_deref(),
                    &trusted_keys_dir,
                    Some(&materialization),
                )
                .await?
                {
                    crate::resolved_config_cache::emit_metric(
                        key.namespace,
                        crate::resolved_config_cache::CacheOutcome::Hit,
                        crate::resolved_config_cache::CacheReason::Ready,
                        entry_bytes,
                        0,
                    );
                    return Ok(value);
                }
                crate::resolved_config_cache::emit_metric(
                    key.namespace,
                    crate::resolved_config_cache::CacheOutcome::Eviction,
                    crate::resolved_config_cache::CacheReason::StaleProof,
                    entry_bytes,
                    0,
                );
                runtime_config_snapshot_cache().discard_if_same(&key, &value);
                authority_retries = authority_retries.saturating_add(1);
            }
            crate::resolved_config_cache::Lookup::Wait { pending } => {
                let wait_started = Instant::now();
                let value = match pending.wait().await {
                    Ok(Some(value)) => value,
                    Ok(None) => {
                        authority_retries = authority_retries.saturating_add(1);
                        if authority_retries >= 3 {
                            return Err(
                                crate::dispatch_error::DispatchError::LaunchPreparationFailed {
                                    code: "method_runtime_config_authority_changed".to_owned(),
                                    message: "method runtime config authority changed repeatedly while waiting for its verified snapshot".to_owned(),
                                    classification: "internal".to_owned(),
                                    binding: None,
                                    details: Box::default(),
                                },
                            );
                        }
                        continue;
                    }
                    Err(error) => {
                        return Err(crate::dispatch_error::DispatchError::Shared(error));
                    }
                };
                if cached_method_config_is_valid(
                    &value,
                    &engine_roots,
                    node_config_root.as_deref(),
                    &trusted_keys_dir,
                    Some(&materialization),
                )
                .await?
                {
                    crate::resolved_config_cache::emit_metric(
                        key.namespace,
                        crate::resolved_config_cache::CacheOutcome::Hit,
                        crate::resolved_config_cache::CacheReason::SingleFlight,
                        0,
                        wait_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    );
                    return Ok(value);
                }
                crate::resolved_config_cache::emit_metric(
                    key.namespace,
                    crate::resolved_config_cache::CacheOutcome::Eviction,
                    crate::resolved_config_cache::CacheReason::StaleProof,
                    0,
                    wait_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                );
                runtime_config_snapshot_cache().discard_if_same(&key, &value);
                authority_retries = authority_retries.saturating_add(1);
            }
            crate::resolved_config_cache::Lookup::Build(fill) => {
                let load_kind = kind.clone();
                let load_requirements = requirements.clone();
                let load_roots = engine_roots.clone();
                let load_node_config_root = node_config_root.clone();
                let load_trusted_keys_dir = trusted_keys_dir.clone();
                let load_materialization = materialization.clone();
                let loaded = tokio::task::spawn_blocking(move || {
                    let snapshot = method_runtime_config_snapshot_under_current_authority(
                        &load_kind,
                        &load_requirements,
                        &load_roots,
                        load_node_config_root.as_deref(),
                        &load_trusted_keys_dir,
                        &load_materialization,
                    )?;
                    let estimated_bytes = snapshot.estimated_bytes();
                    Ok::<_, crate::dispatch_error::DispatchError>((snapshot, estimated_bytes))
                })
                .await
                .map_err(|error| {
                    crate::dispatch_error::DispatchError::Internal(anyhow::anyhow!(
                        "runtime config blocking worker failed: {error}"
                    ))
                })
                .and_then(|result| result);
                let (snapshot, estimated_bytes) = match loaded {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        let error = fill.fail(error);
                        return Err(crate::dispatch_error::DispatchError::Shared(error));
                    }
                };
                crate::resolved_config_cache::emit_metric(
                    key.namespace,
                    crate::resolved_config_cache::CacheOutcome::Miss,
                    crate::resolved_config_cache::CacheReason::Cold,
                    estimated_bytes,
                    0,
                );
                let authority_is_valid = match cached_method_config_is_valid(
                    &Arc::new(snapshot.clone()),
                    &engine_roots,
                    node_config_root.as_deref(),
                    &trusted_keys_dir,
                    Some(&materialization),
                )
                .await
                {
                    Ok(valid) => valid,
                    Err(error) => {
                        return Err(crate::dispatch_error::DispatchError::Shared(
                            fill.fail(error),
                        ));
                    }
                };
                if !authority_is_valid {
                    authority_retries = authority_retries.saturating_add(1);
                    if authority_retries >= 3 {
                        let error =
                            crate::dispatch_error::DispatchError::LaunchPreparationFailed {
                                code: "method_runtime_config_authority_changed".to_owned(),
                                message: "method runtime config authority changed repeatedly while loading its verified snapshot".to_owned(),
                                classification: "internal".to_owned(),
                                binding: None,
                                details: Box::default(),
                            };
                        return Err(crate::dispatch_error::DispatchError::Shared(
                            fill.fail(error),
                        ));
                    }
                    fill.cancel();
                    continue;
                }
                return Ok(fill.complete(snapshot, estimated_bytes));
            }
            crate::resolved_config_cache::Lookup::Bypass => {
                crate::resolved_config_cache::emit_metric(
                    key.namespace,
                    crate::resolved_config_cache::CacheOutcome::Bypass,
                    crate::resolved_config_cache::CacheReason::PendingCapacity,
                    0,
                    0,
                );
                let load_kind = kind.clone();
                let load_requirements = requirements.clone();
                let load_roots = engine_roots.clone();
                let load_node_config_root = node_config_root.clone();
                let load_trusted_keys_dir = trusted_keys_dir.clone();
                let load_materialization = materialization.clone();
                let snapshot = tokio::task::spawn_blocking(move || {
                    method_runtime_config_snapshot_under_current_authority(
                        &load_kind,
                        &load_requirements,
                        &load_roots,
                        load_node_config_root.as_deref(),
                        &load_trusted_keys_dir,
                        &load_materialization,
                    )
                })
                .await
                .map_err(|error| {
                    crate::dispatch_error::DispatchError::Internal(anyhow::anyhow!(
                        "runtime config blocking worker failed: {error}"
                    ))
                })??;
                let snapshot = Arc::new(snapshot);
                if cached_method_config_is_valid(
                    &snapshot,
                    &engine_roots,
                    node_config_root.as_deref(),
                    &trusted_keys_dir,
                    Some(&materialization),
                )
                .await?
                {
                    return Ok(snapshot);
                }
                authority_retries = authority_retries.saturating_add(1);
            }
        }
        if authority_retries >= 3 {
            return Err(
                crate::dispatch_error::DispatchError::LaunchPreparationFailed {
                    code: "method_runtime_config_authority_changed".to_owned(),
                    message: "method runtime config authority changed repeatedly while loading its verified snapshot".to_owned(),
                    classification: "internal".to_owned(),
                    binding: None,
                    details: Box::default(),
                },
            );
        }
    }
}

fn method_runtime_config_snapshot_under_current_authority(
    kind: &str,
    requirements: &BTreeMap<String, ryeos_engine::kind_registry::MethodRuntimeConfigRequirement>,
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    node_config_root: Option<&Path>,
    node_trusted_keys_dir: &Path,
    materialization: &ryeos_app::resolution_cache::ResolutionMaterializationBinding,
) -> Result<crate::dispatch::MethodRuntimeConfigSnapshot, crate::dispatch_error::DispatchError> {
    match materialization
        .authoritative_project_content()
        .map_err(|error| {
            crate::dispatch_error::DispatchError::Internal(anyhow::anyhow!(
                "open admitted method runtime config authority: {error:#}"
            ))
        })? {
        Some((project_root, content)) => {
            crate::dispatch::method_runtime_config_snapshot_with_proof_under_project_authority(
                kind,
                requirements,
                engine_roots,
                node_config_root,
                node_trusted_keys_dir,
                None,
                project_root,
                content,
            )
        }
        None => crate::dispatch::method_runtime_config_snapshot_with_proof(
            kind,
            requirements,
            engine_roots,
            node_config_root,
            node_trusted_keys_dir,
            None,
        ),
    }
}

async fn cached_method_config_is_valid(
    snapshot: &Arc<crate::dispatch::MethodRuntimeConfigSnapshot>,
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    node_config_root: Option<&Path>,
    trusted_keys_dir: &Path,
    materialization: Option<&ryeos_app::resolution_cache::ResolutionMaterializationBinding>,
) -> Result<bool, crate::dispatch_error::DispatchError> {
    let snapshot = snapshot.clone();
    let roots = engine_roots.clone();
    let node_config_root = node_config_root.map(Path::to_path_buf);
    let trusted_keys_dir = trusted_keys_dir.to_path_buf();
    let materialization = materialization.cloned();
    tokio::task::spawn_blocking(move || {
        snapshot.revalidate_against_status(
            &roots,
            node_config_root.as_deref(),
            &trusted_keys_dir,
            materialization.as_ref(),
        )
    })
    .await
    .map_err(|error| {
        crate::dispatch_error::DispatchError::Internal(anyhow::anyhow!(
            "runtime config revalidation worker failed: {error}"
        ))
    })
    .and_then(|status| match status {
        crate::dispatch::MethodRuntimeConfigProofStatus::Current => Ok(true),
        crate::dispatch::MethodRuntimeConfigProofStatus::MutableAuthorityChanged => Ok(false),
        crate::dispatch::MethodRuntimeConfigProofStatus::ImmutableAuthorityMismatch => Err(
            crate::dispatch_error::DispatchError::LaunchPreparationFailed {
                code: "method_runtime_config_authority_invariant".to_owned(),
                message: "method runtime config contradicts its immutable admitted authority"
                    .to_owned(),
                classification: "internal".to_owned(),
                binding: None,
                details: Box::default(),
            },
        ),
    })
}

async fn runtime_config_snapshot_is_valid(
    snapshot: &Arc<crate::dispatch::MethodRuntimeConfigSnapshot>,
    engine_roots: &ryeos_engine::item_resolution::ResolutionRoots,
    node_config_root: Option<&Path>,
    materialization: &ryeos_app::resolution_cache::ResolutionMaterializationBinding,
    state: &ryeos_app::state::AppState,
) -> Result<bool, LaunchAugmentationError> {
    cached_method_config_is_valid(
        snapshot,
        engine_roots,
        node_config_root,
        &state.config.runtime_root().trusted_keys_dir(),
        Some(materialization),
    )
    .await
    .map_err(|error| {
        LaunchAugmentationError::RuntimeRegistry(format!("runtime config revalidation: {error}"))
    })
}

async fn materialize_augmentation_executor(
    engine: &ryeos_engine::engine::Engine,
    bundle_roots: &[std::path::PathBuf],
    executor_ref: &str,
    cache_root: &Path,
) -> Result<crate::execution::launch::MaterializedExecutor, LaunchAugmentationError> {
    let materialization_engine = (*engine).clone();
    let materialization_bundle_roots = bundle_roots.to_vec();
    let materialization_executor_ref = executor_ref.to_string();
    let materialization_cache_root = cache_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        crate::execution::launch::materialize_native_executor_for_engine(
            &materialization_engine,
            &materialization_bundle_roots,
            &materialization_executor_ref,
            &materialization_cache_root,
            ryeos_engine::resolution::TrustClass::TrustedBundle,
            None,
        )
    })
    .await
    .map_err(|error| {
        LaunchAugmentationError::RuntimeRegistry(format!(
            "augmentation executor materialization worker failed: {error}"
        ))
    })?
    .map_err(|error| LaunchAugmentationError::RuntimeRegistry(error.to_string()))
}

async fn verify_augmentation_executor_attestation(
    engine: &ryeos_engine::engine::Engine,
    bundle_roots: &[std::path::PathBuf],
    executor_ref: &str,
) -> Result<crate::execution::launch::VerifiedExecutorChainAttestation, LaunchAugmentationError> {
    let verification_engine = (*engine).clone();
    let verification_bundle_roots = bundle_roots.to_vec();
    let verification_executor_ref = executor_ref.to_string();
    tokio::task::spawn_blocking(move || {
        crate::execution::launch::verify_native_executor_chain_attestation_for_engine(
            &verification_engine,
            &verification_bundle_roots,
            &verification_executor_ref,
            ryeos_engine::resolution::TrustClass::TrustedBundle,
            None,
        )
    })
    .await
    .map_err(|error| {
        LaunchAugmentationError::RuntimeRegistry(format!(
            "augmentation executor verification worker failed: {error}"
        ))
    })?
    .map_err(|error| LaunchAugmentationError::RuntimeRegistry(error.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn build_cache_key(
    decl: &LaunchAugmentationDecl,
    positions: &BTreeMap<String, Vec<String>>,
    per_root: &BTreeMap<String, ResolutionOutput>,
    request_snapshot: &ryeos_engine::engine::EffectiveRequestAuthoritySnapshot,
    provenance: &ryeos_app::execution_provenance::ExecutionProvenance,
    plan_ctx: &ryeos_engine::contracts::PlanContext,
    principal_fingerprint: &str,
    verified_runtime: &ryeos_engine::runtime_registry::VerifiedRuntime,
    runtime_protocol: &ryeos_engine::protocols::VerifiedProtocol,
    executor_ref: &str,
    executor_attestation: &crate::execution::launch::VerifiedExecutorChainAttestation,
    runtime_config_snapshot: &crate::dispatch::MethodRuntimeConfigSnapshot,
) -> Result<String, LaunchAugmentationError> {
    let subject_authority = provenance.subject_resolution_authority();
    let project_authority_identity = provenance
        .project_authority()
        .stable_cache_identity()
        .map_err(|error| {
            LaunchAugmentationError::RuntimeRegistry(format!(
                "derive stable compose project authority identity: {error:#}"
            ))
        })?;

    let authorization_context = json!({
        "requested_by": &plan_ctx.requested_by,
        "current_site_id": &plan_ctx.current_site_id,
        "origin_site_id": &plan_ctx.origin_site_id,
        "execution_hints": &plan_ctx.execution_hints,
        "validate_only": plan_ctx.validate_only,
        "project_authority": &project_authority_identity,
    });
    let authorization_context_digest = digest_json(&authorization_context)?;
    let augmentation_declaration = serde_json::to_value(decl)?;
    let runtime_config_digest = runtime_config_snapshot
        .identity_digest()
        .map_err(|error| LaunchAugmentationError::RuntimeRegistry(error.to_string()))?;
    let executor_attestation_digest = executor_attestation
        .identity_digest()
        .map_err(|error| LaunchAugmentationError::RuntimeRegistry(error.to_string()))?;
    let closure_identities = per_root
        .iter()
        .map(|(root_ref, output)| closure_identity(root_ref, output))
        .collect::<Vec<_>>();

    let key_material = json!({
        "schema_version": 1,
        "subject_resolution_authority": subject_authority,
        "execution_project_authority": project_authority_identity,
        "request_engine_generation_identity":
            &request_snapshot.request_engine_generation_identity,
        "principal_fingerprint": principal_fingerprint,
        "authorization_context_digest": authorization_context_digest,
        "effective_trust_identity": &request_snapshot.effective_trust_identity,
        "effective_parser_registry_fingerprint": &request_snapshot.registry_fingerprint,
        "positions": positions,
        "augmentation_declaration": augmentation_declaration,
        "runtime_config_snapshot_digest": runtime_config_digest,
        "runtime": {
            "canonical_ref": verified_runtime.canonical_ref.to_string(),
            "raw_content_digest": &verified_runtime.raw_content_digest,
            "signer_fingerprint": &verified_runtime.signer_fingerprint,
            "trust_class": verified_runtime.trust_class,
            "bundle_root": &verified_runtime.bundle_root,
        },
        "protocol": {
            "canonical_ref": &runtime_protocol.canonical_ref,
            "raw_content_digest": &runtime_protocol.raw_content_digest,
            "signer_fingerprint": &runtime_protocol.signer_fingerprint,
            "trust_class": runtime_protocol.trust_class,
            "bundle_root": &runtime_protocol.bundle_root,
        },
        "executor": {
            "executor_ref": executor_ref,
            "verified_chain_attestation": executor_attestation_digest,
        },
        "compose_closures": closure_identities,
    });
    digest_json(&key_material)
}

fn closure_identity(root_ref: &str, output: &ResolutionOutput) -> Value {
    let mut referenced = output
        .referenced_items
        .iter()
        .map(resolved_identity)
        .collect::<Vec<_>>();
    referenced.sort_by_key(|value| value.to_string());
    let mut edges = output
        .references_edges
        .iter()
        .map(|edge| {
            json!({
                "from_ref": &edge.from_ref,
                "to_ref": &edge.to_ref,
                "to_source_space": edge.to_source_space,
                "trust_class": edge.trust_class,
                "added_by": edge.added_by,
            })
        })
        .collect::<Vec<_>>();
    edges.sort_by_key(|value| value.to_string());
    json!({
        "requested_root": root_ref,
        "root": resolved_identity(&output.root),
        "ancestors": output.ancestors.iter().map(resolved_identity).collect::<Vec<_>>(),
        "referenced_items": referenced,
        "reference_edges": edges,
        "effective_trust_class": output.effective_trust_class,
    })
}

fn resolved_identity(item: &ryeos_engine::resolution::ResolvedAncestor) -> Value {
    json!({
        "requested_id": &item.requested_id,
        "resolved_ref": &item.resolved_ref,
        "source_space": item.source_space,
        "trust_class": item.trust_class,
        "source_content_digest": &item.source_content_digest,
        "raw_content_digest": &item.raw_content_digest,
    })
}

fn digest_json(value: &Value) -> Result<String, LaunchAugmentationError> {
    let canonical = lillux::canonical_json(value).map_err(|error| {
        LaunchAugmentationError::RuntimeRegistry(format!(
            "compose-cache key is not canonical JSON: {error}"
        ))
    })?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

fn enforce_current_trust(
    per_root: &BTreeMap<String, ResolutionOutput>,
    target_kind: &str,
) -> Result<(), LaunchAugmentationError> {
    for (root_ref, output) in per_root {
        crate::execution::launch::enforce_effective_trust(
            output.effective_trust_class,
            root_ref,
            target_kind,
        )
        .map_err(|error| LaunchAugmentationError::EffectiveTrustRejected(error.to_string()))?;
    }
    Ok(())
}

fn validate_hit_snapshot(
    engine: &ryeos_engine::engine::Engine,
    resolution_project_root: Option<&Path>,
    expected: &ryeos_engine::engine::EffectiveRequestAuthoritySnapshot,
    admitted_request_snapshot: Option<&ryeos_engine::engine::AdmittedRequestAuthoritySnapshot>,
) -> Result<bool, LaunchAugmentationError> {
    let current_result = match admitted_request_snapshot {
        Some(admitted) => {
            let root = resolution_project_root.ok_or_else(|| {
                LaunchAugmentationError::RuntimeRegistry(
                    "admitted compose-cache snapshot has no project root".to_string(),
                )
            })?;
            engine
                .effective_request_snapshot_under_admitted_authority(root, admitted)
                .map(
                    |full| ryeos_engine::engine::EffectiveRequestAuthoritySnapshot {
                        trust_store: full.trust_store.clone(),
                        registry_fingerprint: full.registry_fingerprint.clone(),
                        effective_trust_identity: full.effective_trust_identity.clone(),
                        request_engine_generation_identity: full
                            .request_engine_generation_identity
                            .clone(),
                        subject_resolution_authority: full.subject_resolution_authority.clone(),
                    },
                )
        }
        None => engine.effective_request_authority_snapshot(
            resolution_project_root,
            &expected.subject_resolution_authority,
        ),
    };
    let current = match current_result {
        Ok(current) => current,
        Err(_)
            if expected
                .subject_resolution_authority
                .permits_mutable_revalidation() =>
        {
            return Ok(false);
        }
        Err(error) => {
            return Err(LaunchAugmentationError::RuntimeRegistry(format!(
                "revalidate immutable compose-cache request snapshot: {error}"
            )));
        }
    };
    if current.request_engine_generation_identity != expected.request_engine_generation_identity
        || current.effective_trust_identity != expected.effective_trust_identity
        || current.registry_fingerprint != expected.registry_fingerprint
        || current.subject_resolution_authority != expected.subject_resolution_authority
    {
        if expected
            .subject_resolution_authority
            .permits_mutable_revalidation()
        {
            return Ok(false);
        }
        return Err(LaunchAugmentationError::RuntimeRegistry(
            "compose-cache request snapshot contradicts immutable admitted authority".to_string(),
        ));
    }
    Ok(true)
}

async fn validate_hit_snapshot_off_thread(
    engine: &ryeos_engine::engine::Engine,
    resolution_project_root: Option<&Path>,
    expected: &ryeos_engine::engine::EffectiveRequestAuthoritySnapshot,
    admitted_request_snapshot: Option<&Arc<ryeos_engine::engine::AdmittedRequestAuthoritySnapshot>>,
) -> Result<bool, LaunchAugmentationError> {
    let engine = engine.clone();
    let resolution_project_root = resolution_project_root.map(Path::to_path_buf);
    let expected = expected.clone();
    let admitted_request_snapshot = admitted_request_snapshot.cloned();
    tokio::task::spawn_blocking(move || {
        validate_hit_snapshot(
            &engine,
            resolution_project_root.as_deref(),
            &expected,
            admitted_request_snapshot.as_deref(),
        )
    })
    .await
    .map_err(|error| {
        LaunchAugmentationError::RuntimeRegistry(format!(
            "compose-cache snapshot revalidation worker failed: {error}"
        ))
    })?
}

fn normalized_projection(
    output: &Value,
    positions: &BTreeMap<String, Vec<String>>,
) -> Result<CachedComposeProjection, LaunchAugmentationError> {
    let rendered_positions = extract_rendered_positions(output, positions)?;
    let rendered_meta = extract_rendered_meta(output, positions)?;
    Ok(CachedComposeProjection {
        rendered_positions,
        rendered_meta,
    })
}

fn apply_projection(
    resolution: &mut ResolutionOutput,
    output_derived: &str,
    meta_output_derived: &str,
    projection: &CachedComposeProjection,
) -> Result<(), LaunchAugmentationError> {
    resolution.composed.derived.insert(
        output_derived.to_string(),
        serde_json::to_value(&projection.rendered_positions)?,
    );
    resolution.composed.derived.insert(
        meta_output_derived.to_string(),
        serde_json::to_value(&projection.rendered_meta)?,
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

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::sync::{Arc, mpsc};
    #[cfg(target_os = "linux")]
    use std::time::{Duration, Instant};

    #[cfg(target_os = "linux")]
    fn lifecycle_test_state() -> (tempfile::TempDir, ryeos_app::state::AppState) {
        let temp = tempfile::tempdir().expect("test tempdir");
        let runtime_state_dir = temp.path().join(".ai/state");
        let runtime_db_path = temp.path().join("runtime.sqlite3");
        let key_path = temp.path().join("identity/node-key.pem");
        let config = ryeos_app::config::Config {
            bind: "127.0.0.1:0".parse().expect("test bind"),
            db_path: runtime_db_path.clone(),
            uds_path: temp.path().join("test.sock"),
            app_root: temp.path().to_path_buf(),
            node_signing_key_path: key_path.clone(),
            operator_signing_key_path: temp.path().join("user-key.pem"),
            require_auth: false,
            authorized_keys_dir: temp.path().join("auth"),
            tool_env_passthrough: Vec::new(),
            accounting_issue_acceptance_window_ms: 60_000,
        };
        let identity =
            ryeos_app::identity::NodeIdentity::create(&key_path).expect("test node identity");
        let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();
        let state_store = Arc::new(
            ryeos_app::state_store::StateStore::new_with_head_trust(
                temp.path().to_path_buf(),
                runtime_state_dir,
                runtime_db_path,
                signer,
                write_barrier.clone(),
                Arc::new(head_trust),
            )
            .expect("test state store"),
        );
        let engine = Arc::new(ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::ParserDispatcher::new(
                ryeos_engine::parsers::ParserRegistry::empty(),
                Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
            ),
            Vec::new(),
        ));
        let kind_profiles = Arc::new(ryeos_app::kind_profiles::KindProfileRegistry::build(None));
        let events = Arc::new(ryeos_app::event_store_service::EventStoreService::new(
            state_store.clone(),
        ));
        let event_streams = Arc::new(ryeos_app::event_stream::ThreadEventHub::new(16));
        let threads = Arc::new(
            ryeos_app::thread_lifecycle::ThreadLifecycleService::new_for_test_with_site_id(
                state_store.clone(),
                engine.clone(),
                kind_profiles.clone(),
                events.clone(),
                event_streams.clone(),
                "site:augmentation-cancellation-test",
            )
            .expect("test thread lifecycle"),
        );
        let commands = Arc::new(ryeos_app::command_service::CommandService::new(
            state_store.clone(),
            kind_profiles,
            events.clone(),
        ));
        let node_config = ryeos_app::node_config::NodeConfigSnapshot {
            bundles: Vec::new(),
            routes: Vec::new(),
            commands: Vec::new(),
            hosted_node_policies: Vec::new(),
            command_registration_policy: Default::default(),
        };
        let state = ryeos_app::state::AppState {
            config: Arc::new(config),
            daemon_build: ryeos_app::build_info::get(),
            isolation: Arc::new(ryeos_engine::isolation::IsolationRuntime::default()),
            state_store,
            engine,
            resolution_cache: std::sync::Arc::new(ryeos_app::resolution_cache::ResolutionCache::new(128)),
            engine_cache: ryeos_app::engine_cache::EngineCache::new(Default::default()),
            identity: Arc::new(identity),
            threads,
            live_input: Arc::new(ryeos_app::live_input_queue::LiveInputQueue::new()),
            events,
            event_streams,
            commands,
            callback_tokens: Arc::new(
                ryeos_app::callback_token::CallbackCapabilityStore::new(),
            ),
            thread_auth: Arc::new(ryeos_app::callback_token::ThreadAuthStore::new()),
            extensions: Arc::new(ryeos_app::extension_state::ExtensionState::new()),
            write_barrier: Arc::new(write_barrier),
            started_at: Instant::now(),
            started_at_iso: String::new(),
            catalog_health: ryeos_app::state::CatalogHealth {
                status: "ok".to_string(),
                missing_services: Vec::new(),
            },
            services: Arc::new(ryeos_app::service_registry::ServiceRegistry::new()),
            service_descriptors: &[],
            node_config: Arc::new(node_config),
            node_history_policy: Arc::new(
                ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::durable_without_config(
                ),
            ),
            vault: Arc::new(ryeos_app::vault::EmptyVault),
            command_registry: Arc::new(
                ryeos_runtime::CommandRegistry::from_records(&[], &Default::default())
                    .expect("test command registry"),
            ),
            authorizer: Arc::new(ryeos_runtime::authorizer::Authorizer::new()),
            scheduler_db: Arc::new(
                ryeos_scheduler::db::SchedulerDb::new_in_memory().expect("test scheduler db"),
            ),
            scheduler_runtime_gate: Arc::new(tokio::sync::RwLock::new(())),
            scheduler_reload_tx: None,
            ignore_matcher: Arc::new(ryeos_app::ignore::matcher_from_builtins()),
            vault_fingerprint: None,
            accounting: None,
        };
        (temp, state)
    }

    #[cfg(target_os = "linux")]
    fn augmentation_child_record(thread_id: &str) -> ryeos_app::state_store::NewThreadRecord {
        ryeos_app::state_store::NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: "runtime".to_string(),
            item_ref: "runtime:test/compose-context".to_string(),
            executor_ref: "executor:test/compose-context".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("user:test".to_string()),
            project_root: None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(ryeos_state::objects::CapturedThreadHistoryPolicy {
                retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
                canonical_item_ref: "runtime:test/compose-context".to_string(),
                item_content_hash: "a".repeat(64),
                item_signer_fingerprint: Some("b".repeat(64)),
                item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: "c".repeat(64),
                resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                    node_policy:
                        ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
                },
            }),
        }
    }

    #[test]
    fn projection_preserves_runtime_metadata_without_daemon_heuristics() {
        let positions =
            BTreeMap::from([("system".to_string(), vec!["knowledge:example".to_string()])]);
        let output = json!({
            "rendered": {
                "system": {
                    "content": "rendered knowledge",
                    "composition": {
                        "request_id": "foreign-request",
                        "nested": {
                            "thread_id": "foreign-thread",
                            "child_thread_id": "foreign-child",
                            "callback_token": "foreign-token",
                            "provider_trace_id": "foreign-trace",
                            "access_token": "foreign-credential",
                            "kept": true,
                        },
                    },
                },
            },
        });

        let projection = normalized_projection(&output, &positions).unwrap();
        assert_eq!(
            projection.rendered_positions["system"],
            "rendered knowledge"
        );
        assert_eq!(
            projection.rendered_meta["system"],
            json!({
                "request_id": "foreign-request",
                "nested": {
                    "thread_id": "foreign-thread",
                    "child_thread_id": "foreign-child",
                    "callback_token": "foreign-token",
                    "provider_trace_id": "foreign-trace",
                    "access_token": "foreign-credential",
                    "kept": true,
                },
            })
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cancellation_settles_attached_augmentation_child_while_blocking_wait_is_in_flight() {
        let (temp, state) = lifecycle_test_state();
        let thread_id = "T-augmentation-cancel-inflight";
        let descendant_pid_path = temp.path().join("augmentation-descendant.pid");
        let (worker_done_tx, worker_done_rx) = mpsc::channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .expect("test Tokio runtime");

        let (attached_pid, descendant_pid) = runtime.block_on(async {
            let task_state = state.clone();
            let worker_descendant_pid_path = descendant_pid_path.clone();
            let owner_task = tokio::spawn(async move {
                let launch_claim =
                    crate::execution::launch_claim::ThreadLaunchClaim::acquire_fresh(
                        &task_state,
                        thread_id,
                    )?;
                let launch_owner = launch_claim.canonical_owner()?;
                task_state
                    .state_store
                    .create_thread_for_test(&augmentation_child_record(thread_id))?;
                // This declaration order matches compose-context execution:
                // cancellation drops the lifecycle owner before its launch
                // claim, synchronously exact-killing and settling the child.
                let _lifecycle_owner =
                    crate::execution::process_attachment::LifecycleOwnerGuard::new(
                        &task_state,
                        thread_id,
                    );
                let process_state = task_state.clone();
                let worker = tokio::task::spawn_blocking(move || {
                    let request = lillux::SubprocessRequest {
                        cmd: "/bin/sh".to_string(),
                        argv0: None,
                        args: vec![
                            "-c".to_string(),
                            "/bin/sleep 30 & printf '%s' \"$!\" > \"$1\"; wait".to_string(),
                            "augmentation-child".to_string(),
                            worker_descendant_pid_path.to_string_lossy().into_owned(),
                        ],
                        cwd: None,
                        envs: Vec::new(),
                        stdin_data: None,
                        timeout: 30.0,
                        limits: None,
                        inherited_fds: Vec::new(),
                        supervised_status: None,
                    };
                    let result = match lillux::spawn_awaiting_attachment(request) {
                        Ok(spawned) => {
                            crate::execution::process_attachment::run_spawned_lillux_attached(
                                &process_state,
                                thread_id,
                                &launch_owner,
                                spawned,
                                None,
                            )
                        }
                        Err(result) => Err(anyhow::anyhow!(
                            "spawn augmentation fixture: {}",
                            result.stderr
                        )),
                    };
                    let completion = result
                        .as_ref()
                        .map(|result| result.pid)
                        .map_err(|error| format!("{error:#}"));
                    let _ = worker_done_tx.send(completion);
                    result
                });
                worker
                    .await
                    .map_err(|error| anyhow::anyhow!("blocking worker join: {error}"))??;
                Ok::<(), anyhow::Error>(())
            });

            let expected_executable =
                std::fs::canonicalize("/bin/sh").expect("canonical shell executable");
            let deadline = Instant::now() + Duration::from_secs(5);
            let process_group = |pid: u32| {
                let pid = libc::pid_t::try_from(pid).ok()?;
                let pgid = unsafe { libc::getpgid(pid) };
                (pgid > 1).then_some(i64::from(pgid))
            };
            let (attached_pid, descendant_pid) = loop {
                if let Some(thread) = state
                    .threads
                    .get_thread(thread_id)
                    .expect("read augmentation child")
                    && let (Some(identity), Some(persisted_pgid)) =
                        (thread.runtime.process_identity, thread.runtime.pgid)
                {
                    let pid = u32::try_from(identity.target_pid).expect("positive child pid");
                    if let Ok(encoded) = std::fs::read_to_string(&descendant_pid_path)
                        && let Ok(descendant_pid) = encoded.trim().parse::<u32>()
                    {
                        let actual_pgid = process_group(pid);
                        if descendant_pid != pid
                            && lillux::is_alive(descendant_pid)
                            && actual_pgid == Some(persisted_pgid)
                            && process_group(descendant_pid) == actual_pgid
                            && std::fs::read_link(format!("/proc/{pid}/exe"))
                                .is_ok_and(|executable| executable == expected_executable)
                        {
                            break (pid, descendant_pid);
                        }
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "augmentation child and descendant were not both alive in the persisted process group"
                );
                std::thread::sleep(Duration::from_millis(10));
                tokio::task::yield_now().await;
            };

            owner_task.abort();
            let join_error = owner_task
                .await
                .expect_err("cancelled augmentation owner must not complete normally");
            assert!(
                join_error.is_cancelled(),
                "augmentation owner must exit through Tokio cancellation"
            );
            (attached_pid, descendant_pid)
        });

        let settled = state
            .threads
            .get_thread(thread_id)
            .expect("read settled augmentation child")
            .expect("settled augmentation child row");
        assert_eq!(settled.status, "killed");
        assert!(
            settled.runtime.process_identity.is_none(),
            "terminal cancellation settlement must clear the exact child identity"
        );
        let worker_pid = worker_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("in-flight blocking wait must observe the exact kill and return")
            .unwrap_or_else(|error| panic!("blocking worker failed after attachment: {error}"));
        assert_eq!(
            worker_pid, attached_pid,
            "blocking worker must reap the exact attached augmentation child"
        );
        for (pid, role) in [
            (attached_pid, "augmentation child"),
            (descendant_pid, "augmentation descendant"),
        ] {
            let deadline = Instant::now() + Duration::from_secs(5);
            while lillux::is_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                !lillux::is_alive(pid),
                "{role} outlived parent cancellation settlement"
            );
        }
    }
}
