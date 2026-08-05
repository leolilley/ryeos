//! Generic runtime launch-contract preparation.
//!
//! This module knows only signed declaration shapes, canonical refs, trust,
//! configuration snapshots, and bounded handler protocol values. Runtime-domain
//! key names and value schemas remain opaque.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use ryeos_engine::contracts::{EffectivePrincipal, ItemSpace};
use ryeos_engine::error::EngineError;
use ryeos_engine::item_resolution::ResolutionRoots;
use ryeos_engine::parsers::ParserDispatcher;
use ryeos_engine::resolution::{ResolutionOutput, TrustClass};
use ryeos_engine::runtime_registry::{
    LaunchItemSpace, LaunchPreparationDecl, RuntimeFactKind, VerifiedRuntime,
};
use ryeos_handler_protocol::{
    ItemSpaceWire, LaunchComposedViewWire, LaunchConfigContributorWire, LaunchConfigSnapshotWire,
    LaunchDiagnosticScalarWire, LaunchPrepareError, LaunchPrepareErrorClass, LaunchPrepareRequest,
    LaunchPrepareResponse, LaunchPreparedItemWire, LaunchSecretOriginWire, TrustClassWire,
};
use ryeos_runtime::authorizer::{AuthorizationPolicy, canonical_cap};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dispatch_error::DispatchError;

const MAX_RUNTIME_DATA_VALUE_BYTES: usize = 1024 * 1024;
const MAX_RUNTIME_DATA_BYTES: usize = 2 * 1024 * 1024;
const MAX_RUNTIME_FACT_BYTES: usize = 64 * 1024;
const MAX_SECRET_ORIGINS: usize = 64;
const MAX_SECRET_NAMES: usize = 32;
const MAX_JSON_DEPTH: usize = 32;
const MAX_HANDLER_ERROR_CODE_BYTES: usize = 64;
const MAX_HANDLER_ERROR_MESSAGE_BYTES: usize = 512;
const MAX_HANDLER_ERROR_DETAILS: usize = 32;
const MAX_HANDLER_ERROR_DETAIL_STRING_BYTES: usize = 256;
const MAX_HANDLER_ERROR_DETAILS_BYTES: usize = 8 * 1024;
const MAX_REF_BINDINGS: usize = 32;
const MAX_REF_BINDING_NAME_BYTES: usize = 64;
const MAX_REF_BINDING_VALUE_BYTES: usize = 2_048;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefBindingLaunchRecord {
    pub canonical_ref: String,
    pub source_space: ItemSpace,
    pub effective_trust_class: TrustClass,
    pub resolution: ryeos_engine::resolution::AsLaunchedResolutionDigest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedSecret {
    pub name: String,
    pub origin: LaunchSecretOriginWire,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedRuntimeLaunch {
    pub runtime_data: BTreeMap<String, Value>,
    pub required_secrets: Vec<PreparedSecret>,
    pub runtime_facts: BTreeMap<String, Value>,
    pub binding_records: BTreeMap<String, RefBindingLaunchRecord>,
    /// Exact signed configuration contributors that influenced launch
    /// preparation. The prepared values remain sealed in `runtime_data`; this
    /// list exists so recovery can apply current signer revocation without
    /// reloading mutable config names.
    pub config_contributors: Vec<LaunchConfigContributorWire>,
    /// Validated financial authority sealed with this launch, exactly as
    /// declared by the runtime launch contract. `None` for runtimes whose
    /// contract declares no direct paid provider work.
    pub financial_authority: Option<PreparedFinancialAuthority>,
}

/// The executor-validated, canonicalized financial authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedFinancialAuthority {
    /// Always `"provider_accounting_authority_v1"` in this version.
    pub kind: String,
    /// The exact validated authority payload (canonical value form). Opaque
    /// past the validation boundary: generic code consumes the typed fields
    /// below, never this payload's structure.
    pub authority: Value,
    /// sha-256 of the canonical JSON of `authority`.
    pub authority_digest: String,
    /// Kind-neutral admission fact extracted during strict validation: what
    /// class of spend bound this authority seals. This is the only property
    /// the executor may branch on.
    pub spend_bound: SealedSpendBound,
}

/// Generic classification of a sealed spend bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SealedSpendBound {
    /// Mechanically proven paid maximum — hard-spend eligible.
    Paid,
    /// Explicitly-free contract — hard-spend eligible at exact zero.
    ExplicitlyFree,
    /// Declared bound without mechanical proof — ineligible for hard spend.
    AdvisoryOnly,
}

impl SealedSpendBound {
    pub const fn hard_spend_eligible(self) -> bool {
        matches!(self, Self::Paid | Self::ExplicitlyFree)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Paid => "paid",
            Self::ExplicitlyFree => "explicitly_free",
            Self::AdvisoryOnly => "advisory_only",
        }
    }
}

pub const FINANCIAL_AUTHORITY_KIND_PROVIDER_ACCOUNTING_V1: &str =
    "provider_accounting_authority_v1";
const MAX_FINANCIAL_AUTHORITY_BYTES: usize = 64 * 1024;

pub struct PrepareRuntimeLaunchRequest<'a> {
    pub engine: &'a ryeos_engine::engine::Engine,
    pub runtime: &'a VerifiedRuntime,
    pub primary: &'a ResolutionOutput,
    pub ref_bindings: &'a BTreeMap<String, String>,
    pub roots: &'a ResolutionRoots,
    pub parsers: &'a ParserDispatcher,
    /// Exact trust half of the same effective request snapshot as `parsers`.
    pub trust_store: &'a ryeos_engine::trust::TrustStore,
    pub principal: &'a EffectivePrincipal,
    /// Exact subject authority for this preparation. Cached managed launch
    /// additionally supplies the matching admitted materialization below;
    /// threadless diagnostics are limited to Projectless or LiveFs.
    pub subject_resolution_authority: &'a ryeos_engine::contracts::SubjectResolutionAuthority,
    /// Shared content-addressed resolution service for binding closures.
    /// Threadless preflight may omit it; authoritative managed launch provides
    /// the exact provenance and materialization lease.
    pub resolution_cache: Option<PreparedResolutionCacheContext<'a>>,
    pub ref_binding_resolution_timings:
        Option<&'a ryeos_app::launch_stage_timings::LaunchStageTimings>,
}

#[derive(Clone, Copy)]
pub struct PreparedResolutionCacheContext<'a> {
    pub cache: &'a std::sync::Arc<ryeos_app::resolution_cache::ResolutionCache>,
    pub materialization: &'a ryeos_app::resolution_cache::ResolutionMaterializationBinding,
    pub generation_identity: &'a str,
    pub plan_context_identity: &'a str,
}

/// Exact immutable authority legs required to reuse launch-preparer output.
///
/// This contains no invocation identity or authorization result. Principal
/// authorization is re-evaluated while building [`PreparedRuntimeLaunchInputs`]
/// for every launch, before this authority is used.
pub struct PreparedLaunchSkeletonAuthority<'a> {
    pub subject_resolution_authority: &'a ryeos_engine::contracts::SubjectResolutionAuthority,
    pub execution_project_authority: &'a ryeos_state::objects::ExecutionProjectAuthority,
    pub lifecycle_authority: &'a ryeos_state::objects::ExecutionLifecycleAuthority,
    pub protocol: &'a ryeos_engine::protocols::VerifiedProtocol,
    pub executor_chain_identity: &'a str,
    pub request_engine_generation_identity: &'a str,
    pub effective_trust_identity: &'a str,
}

#[derive(Clone)]
struct OwnedPreparedLaunchSkeletonAuthority {
    subject_resolution_authority: ryeos_engine::contracts::SubjectResolutionAuthority,
    execution_project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    protocol: ryeos_engine::protocols::VerifiedProtocol,
    executor_chain_identity: String,
    request_engine_generation_identity: String,
    effective_trust_identity: String,
}

impl OwnedPreparedLaunchSkeletonAuthority {
    fn capture(authority: &PreparedLaunchSkeletonAuthority<'_>) -> Self {
        Self {
            subject_resolution_authority: authority.subject_resolution_authority.clone(),
            execution_project_authority: authority.execution_project_authority.clone(),
            lifecycle_authority: *authority.lifecycle_authority,
            protocol: authority.protocol.clone(),
            executor_chain_identity: authority.executor_chain_identity.to_owned(),
            request_engine_generation_identity: authority
                .request_engine_generation_identity
                .to_owned(),
            effective_trust_identity: authority.effective_trust_identity.to_owned(),
        }
    }
}

#[derive(Clone)]
struct PreparedRuntimeLaunchInputs {
    primary: LaunchPreparedItemWire,
    binding_wires: BTreeMap<String, LaunchPreparedItemWire>,
    binding_records: BTreeMap<String, RefBindingLaunchRecord>,
    config_inputs: BTreeMap<String, LaunchConfigSnapshotWire>,
    config_dependency_proof: ryeos_engine::launch_config::LaunchConfigDependencyProof,
}

struct OwnedPreparedResolutionCacheContext {
    cache: std::sync::Arc<ryeos_app::resolution_cache::ResolutionCache>,
    materialization: ryeos_app::resolution_cache::ResolutionMaterializationBinding,
    generation_identity: String,
    plan_context_identity: String,
}

struct OwnedPrepareRuntimeLaunchRequest {
    engine: ryeos_engine::engine::Engine,
    runtime: VerifiedRuntime,
    primary: ResolutionOutput,
    ref_bindings: BTreeMap<String, String>,
    roots: ResolutionRoots,
    parsers: ParserDispatcher,
    trust_store: ryeos_engine::trust::TrustStore,
    principal: EffectivePrincipal,
    subject_resolution_authority: ryeos_engine::contracts::SubjectResolutionAuthority,
    resolution_cache: Option<OwnedPreparedResolutionCacheContext>,
    ref_binding_resolution_timings: Option<ryeos_app::launch_stage_timings::LaunchStageTimings>,
}

impl OwnedPrepareRuntimeLaunchRequest {
    fn capture(request: &PrepareRuntimeLaunchRequest<'_>) -> Self {
        Self {
            engine: request.engine.clone(),
            runtime: request.runtime.clone(),
            primary: request.primary.clone(),
            ref_bindings: request.ref_bindings.clone(),
            roots: request.roots.clone(),
            parsers: request.parsers.clone(),
            trust_store: request.trust_store.clone(),
            principal: request.principal.clone(),
            subject_resolution_authority: request.subject_resolution_authority.clone(),
            resolution_cache: request.resolution_cache.map(|context| {
                OwnedPreparedResolutionCacheContext {
                    cache: std::sync::Arc::clone(context.cache),
                    materialization: context.materialization.clone(),
                    generation_identity: context.generation_identity.to_owned(),
                    plan_context_identity: context.plan_context_identity.to_owned(),
                }
            }),
            ref_binding_resolution_timings: request.ref_binding_resolution_timings.cloned(),
        }
    }

    fn with_request<T>(&self, operation: impl FnOnce(&PrepareRuntimeLaunchRequest<'_>) -> T) -> T {
        let cache_context =
            self.resolution_cache
                .as_ref()
                .map(|context| PreparedResolutionCacheContext {
                    cache: &context.cache,
                    materialization: &context.materialization,
                    generation_identity: &context.generation_identity,
                    plan_context_identity: &context.plan_context_identity,
                });
        operation(&PrepareRuntimeLaunchRequest {
            engine: &self.engine,
            runtime: &self.runtime,
            primary: &self.primary,
            ref_bindings: &self.ref_bindings,
            roots: &self.roots,
            parsers: &self.parsers,
            trust_store: &self.trust_store,
            principal: &self.principal,
            subject_resolution_authority: &self.subject_resolution_authority,
            resolution_cache: cache_context,
            ref_binding_resolution_timings: self.ref_binding_resolution_timings.as_ref(),
        })
    }
}

pub fn prepare_runtime_launch(
    request: PrepareRuntimeLaunchRequest<'_>,
) -> Result<PreparedRuntimeLaunch, DispatchError> {
    let inputs = prepare_runtime_launch_inputs(&request, None)?;
    finish_runtime_launch_preparation(&request, &inputs)
}

/// Prepare one managed launch through the bounded secret-free skeleton cache.
///
/// Current principal authorization and ref-binding resolution run before every
/// lookup. Complete positive/negative config discovery is itself served by a
/// bounded content-addressed cache whose mutable roots are re-proved before
/// use. A hit skips only the deterministic launch-preparer handler and returns immutable static
/// output; capability filtering, secret reads, budget reservation,
/// cancellation, thread identity, and capsule construction remain downstream
/// per invocation.
pub async fn prepare_runtime_launch_cached(
    request: PrepareRuntimeLaunchRequest<'_>,
    authority: PreparedLaunchSkeletonAuthority<'_>,
) -> Result<PreparedRuntimeLaunch, DispatchError> {
    let mut owned_request = OwnedPrepareRuntimeLaunchRequest::capture(&request);
    validate_prepared_skeleton_authority_pair(&authority)?;
    let mut authority_retries = 0_usize;
    'authority: loop {
        let prepared_config_roots = owned_request
            .engine
            .launch_config_roots(&owned_request.roots);
        let config_materialization = owned_request
            .resolution_cache
            .as_ref()
            .map(|context| context.materialization.clone());
        let config_set = load_launch_config_snapshot_cached(&owned_request, &authority).await?;
        let cache_generation = owned_request
            .engine
            .registered_bundle_generation_fingerprint();
        let cache_generation_epoch = owned_request.engine.registered_bundle_generation_epoch();
        let cache_retirement_scope = owned_request.runtime.canonical_ref.to_string();
        let owned_authority = OwnedPreparedLaunchSkeletonAuthority::capture(&authority);
        let (returned_request, inputs, cache_identity) = tokio::task::spawn_blocking(move || {
            let inputs = owned_request.with_request(|request| {
                prepare_runtime_launch_inputs(request, Some(&config_set))
            })?;
            let cache_identity = owned_request.with_request(|request| {
                prepared_launch_skeleton_key(request, &inputs, &owned_authority)
            })?;
            Ok::<_, DispatchError>((owned_request, inputs, cache_identity))
        })
        .await
        .map_err(|error| {
            DispatchError::Internal(anyhow::anyhow!(
                "runtime preparation blocking worker failed: {error}"
            ))
        })??;
        owned_request = returned_request;
        let cache_key = super::prepared_launch_cache::PreparedCacheKey {
            retirement_scope: cache_retirement_scope,
            generation: cache_generation,
            generation_epoch: cache_generation_epoch,
            identity: cache_identity,
        };
        match super::prepared_launch_cache::cache().begin(cache_key.clone()) {
            super::prepared_launch_cache::Lookup::Hit {
                skeleton,
                entry_bytes,
            } => {
                // Revalidate the dependency proof captured from this launch's
                // active roots, not the cached skeleton's original
                // materialization paths. The stable proof digest is in the
                // key, so identical pinned generations share the entry even
                // after an earlier temporary checkout is reclaimed.
                let proof_status = prepared_config_proof_status(
                    &inputs.config_dependency_proof,
                    &prepared_config_roots,
                    config_materialization.as_ref(),
                )
                .await?;
                if proof_status != ryeos_engine::launch_config::LaunchConfigProofStatus::Current {
                    super::prepared_launch_cache::cache().discard_if_same(
                        &cache_key,
                        &skeleton,
                        super::prepared_launch_cache::CacheReason::AuthorityRevalidationFailed,
                    );
                    consume_prepared_proof_status(proof_status, &mut authority_retries)?;
                    continue 'authority;
                }
                super::prepared_launch_cache::emit_metric(
                    super::prepared_launch_cache::CacheOutcome::Hit,
                    super::prepared_launch_cache::CacheReason::Ready,
                    entry_bytes,
                    0,
                );
                return Ok(skeleton.prepared.clone());
            }
            super::prepared_launch_cache::Lookup::Wait { pending } => {
                let wait_started = Instant::now();
                let Some(skeleton) = pending.wait().await.map_err(DispatchError::Shared)? else {
                    consume_mutable_prepared_authority_retry(&mut authority_retries)?;
                    continue 'authority;
                };
                let proof_status = prepared_config_proof_status(
                    &inputs.config_dependency_proof,
                    &prepared_config_roots,
                    config_materialization.as_ref(),
                )
                .await?;
                if proof_status != ryeos_engine::launch_config::LaunchConfigProofStatus::Current {
                    super::prepared_launch_cache::cache().discard_if_same(
                        &cache_key,
                        &skeleton,
                        super::prepared_launch_cache::CacheReason::AuthorityRevalidationFailed,
                    );
                    consume_prepared_proof_status(proof_status, &mut authority_retries)?;
                    continue 'authority;
                }
                super::prepared_launch_cache::emit_metric(
                    super::prepared_launch_cache::CacheOutcome::Hit,
                    super::prepared_launch_cache::CacheReason::SingleFlight,
                    0,
                    wait_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                );
                return Ok(skeleton.prepared.clone());
            }
            super::prepared_launch_cache::Lookup::Build(fill) => {
                let finish_engine = owned_request.engine.clone();
                let finish_runtime = owned_request.runtime.clone();
                let finish_ref_bindings = owned_request.ref_bindings.clone();
                let finish_inputs = inputs.clone();
                let loaded = tokio::task::spawn_blocking(move || {
                    let prepared = finish_runtime_launch_preparation_parts(
                        &finish_engine,
                        &finish_runtime,
                        &finish_ref_bindings,
                        &finish_inputs,
                    )?;
                    let serialized_bytes = serde_json::to_vec(&prepared)
                        .map(|serialized| serialized.len())
                        .unwrap_or(usize::MAX);
                    Ok::<_, DispatchError>((prepared, serialized_bytes))
                })
                .await
                .map_err(|error| {
                    DispatchError::Internal(anyhow::anyhow!(
                        "runtime launch-preparer blocking worker failed: {error}"
                    ))
                })
                .and_then(|result| result);
                let (prepared, serialized_bytes) = match loaded {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        return Err(DispatchError::Shared(fill.fail(error)));
                    }
                };
                let proof_status = match prepared_config_proof_status(
                    &inputs.config_dependency_proof,
                    &prepared_config_roots,
                    config_materialization.as_ref(),
                )
                .await
                {
                    Ok(valid) => valid,
                    Err(error) => {
                        return Err(DispatchError::Shared(fill.fail(error)));
                    }
                };
                if proof_status != ryeos_engine::launch_config::LaunchConfigProofStatus::Current {
                    if let Err(error) =
                        consume_prepared_proof_status(proof_status, &mut authority_retries)
                    {
                        return Err(DispatchError::Shared(fill.fail(error)));
                    }
                    fill.cancel();
                    continue 'authority;
                }
                let skeleton = fill.complete(
                    super::prepared_launch_cache::PreparedManagedLaunchSkeleton { prepared },
                    serialized_bytes,
                );
                super::prepared_launch_cache::emit_metric(
                    super::prepared_launch_cache::CacheOutcome::Miss,
                    super::prepared_launch_cache::CacheReason::Cold,
                    0,
                    0,
                );
                return Ok(skeleton.prepared.clone());
            }
            super::prepared_launch_cache::Lookup::Bypass => {
                super::prepared_launch_cache::emit_metric(
                    super::prepared_launch_cache::CacheOutcome::Bypass,
                    super::prepared_launch_cache::CacheReason::PendingCapacity,
                    0,
                    0,
                );
                let finish_engine = owned_request.engine.clone();
                let finish_runtime = owned_request.runtime.clone();
                let finish_ref_bindings = owned_request.ref_bindings.clone();
                let finish_inputs = inputs.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    finish_runtime_launch_preparation_parts(
                        &finish_engine,
                        &finish_runtime,
                        &finish_ref_bindings,
                        &finish_inputs,
                    )
                })
                .await
                .map_err(|error| {
                    DispatchError::Internal(anyhow::anyhow!(
                        "runtime launch-preparer blocking worker failed: {error}"
                    ))
                })??;
                let proof_status = prepared_config_proof_status(
                    &inputs.config_dependency_proof,
                    &prepared_config_roots,
                    config_materialization.as_ref(),
                )
                .await?;
                if proof_status != ryeos_engine::launch_config::LaunchConfigProofStatus::Current {
                    consume_prepared_proof_status(proof_status, &mut authority_retries)?;
                    continue 'authority;
                }
                return Ok(prepared);
            }
        }
    }
}

fn validate_prepared_skeleton_authority_pair(
    authority: &PreparedLaunchSkeletonAuthority<'_>,
) -> Result<(), DispatchError> {
    use ryeos_engine::contracts::SubjectResolutionAuthority;
    use ryeos_state::objects::{ExecutionProjectAuthority, PinnedProjectRealization};

    match (
        authority.subject_resolution_authority,
        authority.execution_project_authority,
    ) {
        (
            SubjectResolutionAuthority::Projectless,
            ExecutionProjectAuthority::Projectless { .. },
        ) => Ok(()),
        (SubjectResolutionAuthority::LiveFs, ExecutionProjectAuthority::LiveProject { .. }) => {
            Ok(())
        }
        (
            SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: subject_hash,
            },
            ExecutionProjectAuthority::PinnedGeneration {
                snapshot_hash,
                realization: PinnedProjectRealization::ReadOnly,
                ..
            },
        ) if subject_hash == snapshot_hash => Ok(()),
        (
            SubjectResolutionAuthority::CowWorkspace {
                base_snapshot_hash: subject_base,
                current_operational_generation: subject_current,
            },
            ExecutionProjectAuthority::PinnedGeneration {
                base_snapshot_hash,
                snapshot_hash,
                realization: PinnedProjectRealization::Cow { .. },
                ..
            },
        ) if subject_base == base_snapshot_hash && subject_current == snapshot_hash => Ok(()),
        _ => Err(preparation_error(
            "launch_config_authority_invariant",
            "prepared launch subject authority contradicts its execution project authority",
            LaunchPrepareErrorClass::Internal,
        )),
    }
}

fn consume_prepared_proof_status(
    status: ryeos_engine::launch_config::LaunchConfigProofStatus,
    authority_retries: &mut usize,
) -> Result<(), DispatchError> {
    match status {
        ryeos_engine::launch_config::LaunchConfigProofStatus::Current => return Ok(()),
        ryeos_engine::launch_config::LaunchConfigProofStatus::ImmutableAuthorityMismatch => {
            return Err(preparation_error(
                "launch_config_authority_invariant",
                "immutable admitted launch config authority failed exact revalidation",
                LaunchPrepareErrorClass::Internal,
            ));
        }
        ryeos_engine::launch_config::LaunchConfigProofStatus::MutableAuthorityChanged => {}
    }
    consume_mutable_prepared_authority_retry(authority_retries)
}

fn consume_mutable_prepared_authority_retry(
    authority_retries: &mut usize,
) -> Result<(), DispatchError> {
    *authority_retries = authority_retries.saturating_add(1);
    if *authority_retries >= 3 {
        return Err(preparation_error(
            "launch_config_authority_changed",
            "launch config authority changed repeatedly while preparing the managed launch",
            LaunchPrepareErrorClass::Internal,
        ));
    }
    Ok(())
}

async fn prepared_config_proof_status(
    proof: &ryeos_engine::launch_config::LaunchConfigDependencyProof,
    roots: &ResolutionRoots,
    materialization: Option<&ryeos_app::resolution_cache::ResolutionMaterializationBinding>,
) -> Result<ryeos_engine::launch_config::LaunchConfigProofStatus, DispatchError> {
    let proof = proof.clone();
    let roots = roots.clone();
    let materialization = materialization.cloned();
    tokio::task::spawn_blocking(move || {
        let project = match materialization.as_ref() {
            Some(binding) => match binding.authoritative_project_content() {
                Ok(project) => project.map(|(root, content)| {
                    (
                        root,
                        content as &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
                    )
                }),
                Err(_) => {
                    return ryeos_engine::launch_config::LaunchConfigProofStatus::ImmutableAuthorityMismatch;
                }
            },
            None => None,
        };
        proof.revalidate_under_authority_status(&roots, project)
    })
    .await
    .map_err(|error| {
        DispatchError::Internal(anyhow::anyhow!(
            "prepared launch config revalidation worker failed: {error}"
        ))
    })
}

fn launch_config_snapshot_cache() -> &'static crate::resolved_config_cache::SnapshotCache<
    ryeos_engine::launch_config::LaunchConfigSnapshotSet,
> {
    static CACHE: OnceLock<
        crate::resolved_config_cache::SnapshotCache<
            ryeos_engine::launch_config::LaunchConfigSnapshotSet,
        >,
    > = OnceLock::new();
    CACHE.get_or_init(crate::resolved_config_cache::SnapshotCache::default)
}

pub(crate) fn load_launch_config_set_under_current_authority(
    declarations: &BTreeMap<String, ryeos_engine::runtime_registry::LaunchConfigInputDecl>,
    roots: &ResolutionRoots,
    parsers: &ParserDispatcher,
    engine: &ryeos_engine::engine::Engine,
    trust_store: &ryeos_engine::trust::TrustStore,
    materialization: Option<&ryeos_app::resolution_cache::ResolutionMaterializationBinding>,
) -> Result<ryeos_engine::launch_config::LaunchConfigSnapshotSet, DispatchError> {
    let project = match materialization {
        Some(binding) => binding.authoritative_project_content().map_err(|error| {
            preparation_error(
                "launch_config_authority_invalid",
                format!("open admitted launch config authority: {error:#}"),
                LaunchPrepareErrorClass::Internal,
            )
        })?,
        None => None,
    };
    match project {
        Some((project_root, content)) => {
            ryeos_engine::launch_config::load_launch_config_snapshots_with_proof_under_project_authority(
                declarations,
                roots,
                parsers,
                &engine.parser_dispatcher,
                &engine.kinds,
                trust_store,
                &engine.node_trust_store,
                project_root,
                content,
            )
        }
        None => ryeos_engine::launch_config::load_launch_config_snapshots_with_proof(
            declarations,
            roots,
            parsers,
            &engine.parser_dispatcher,
            &engine.kinds,
            trust_store,
            &engine.node_trust_store,
        ),
    }
    .map_err(map_launch_config_error)
}

async fn load_launch_config_snapshot_cached(
    request: &OwnedPrepareRuntimeLaunchRequest,
    authority: &PreparedLaunchSkeletonAuthority<'_>,
) -> Result<Arc<ryeos_engine::launch_config::LaunchConfigSnapshotSet>, DispatchError> {
    let roots = request.engine.launch_config_roots(&request.roots);
    let generation = request.engine.registered_bundle_generation_fingerprint();
    let generation_epoch = request.engine.registered_bundle_generation_epoch();
    let retirement_scope = request.runtime.canonical_ref.to_string();
    let declarations = request.runtime.yaml.launch_contract.config_inputs.clone();
    let subject_authority = authority.subject_resolution_authority.clone();
    let request_engine_generation_identity =
        authority.request_engine_generation_identity.to_owned();
    let effective_trust_identity = authority.effective_trust_identity.to_owned();
    let actual_trust_identity = request.trust_store.fingerprint();
    let parser_identity = request.parsers.fingerprint();
    let node_parser_identity = request.engine.parser_dispatcher.fingerprint();
    let kind_identity = request.engine.kinds.fingerprint().to_owned();
    let node_trust_identity = request.engine.node_trust_store.fingerprint();
    let key_roots = roots.clone();
    let key_declarations = declarations.clone();
    let key_retirement_scope = retirement_scope.clone();
    let key = tokio::task::spawn_blocking(move || {
        let include_live_paths = subject_authority.permits_mutable_revalidation();
        let root_identity = key_roots
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
            "declarations": key_declarations,
            "subject_authority": subject_authority,
            "request_engine_generation_identity": request_engine_generation_identity,
            "effective_trust_identity": effective_trust_identity,
            "actual_trust_identity": actual_trust_identity,
            "parser_identity": parser_identity,
            "node_parser_identity": node_parser_identity,
            "kind_identity": kind_identity,
            "node_trust_identity": node_trust_identity,
            "roots": root_identity,
        });
        let canonical = lillux::canonical_json(&value).map_err(|error| {
            preparation_error(
                "launch_config_cache_key_invalid",
                format!("canonicalize launch config cache key: {error}"),
                LaunchPrepareErrorClass::Internal,
            )
        })?;
        Ok::<_, DispatchError>(crate::resolved_config_cache::SnapshotCacheKey {
            namespace: "managed_launch",
            retirement_scope: key_retirement_scope,
            generation,
            generation_epoch,
            identity: lillux::sha256_hex(canonical.as_bytes()),
        })
    })
    .await
    .map_err(|error| {
        DispatchError::Internal(anyhow::anyhow!(
            "launch config cache-key worker failed: {error}"
        ))
    })??;
    let config_materialization = request
        .resolution_cache
        .as_ref()
        .map(|context| context.materialization.clone());

    let mut authority_retries = 0_usize;
    loop {
        match launch_config_snapshot_cache().begin(key.clone()) {
            crate::resolved_config_cache::Lookup::Hit { value, entry_bytes } => {
                let status =
                    launch_config_hit_status(&value, &roots, config_materialization.as_ref())
                        .await?;
                if status == ryeos_engine::launch_config::LaunchConfigProofStatus::Current {
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
                launch_config_snapshot_cache().discard_if_same(&key, &value);
                consume_prepared_proof_status(status, &mut authority_retries)?;
            }
            crate::resolved_config_cache::Lookup::Wait { pending } => {
                let wait_started = Instant::now();
                let value = match pending.wait().await {
                    Ok(Some(value)) => value,
                    Ok(None) => {
                        consume_mutable_prepared_authority_retry(&mut authority_retries)?;
                        continue;
                    }
                    Err(error) => return Err(DispatchError::Shared(error)),
                };
                let status =
                    launch_config_hit_status(&value, &roots, config_materialization.as_ref())
                        .await?;
                if status == ryeos_engine::launch_config::LaunchConfigProofStatus::Current {
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
                launch_config_snapshot_cache().discard_if_same(&key, &value);
                consume_prepared_proof_status(status, &mut authority_retries)?;
            }
            crate::resolved_config_cache::Lookup::Build(fill) => {
                let engine = request.engine.clone();
                let parsers = request.parsers.clone();
                let trust_store = request.trust_store.clone();
                let load_roots = roots.clone();
                let load_declarations = declarations.clone();
                let load_materialization = config_materialization.clone();
                let loaded = tokio::task::spawn_blocking(move || {
                    let snapshot = load_launch_config_set_under_current_authority(
                        &load_declarations,
                        &load_roots,
                        &parsers,
                        &engine,
                        &trust_store,
                        load_materialization.as_ref(),
                    )?;
                    let estimated_bytes = snapshot.estimated_bytes();
                    Ok::<_, DispatchError>((snapshot, estimated_bytes))
                })
                .await
                .map_err(|error| {
                    DispatchError::Internal(anyhow::anyhow!(
                        "launch config blocking worker failed: {error}"
                    ))
                })
                .and_then(|result| result);
                let (snapshot, estimated_bytes) = match loaded {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        let error = fill.fail(error);
                        return Err(DispatchError::Shared(error));
                    }
                };
                crate::resolved_config_cache::emit_metric(
                    key.namespace,
                    crate::resolved_config_cache::CacheOutcome::Miss,
                    crate::resolved_config_cache::CacheReason::Cold,
                    estimated_bytes,
                    0,
                );
                let authority_status = match launch_config_hit_status(
                    &Arc::new(snapshot.clone()),
                    &roots,
                    config_materialization.as_ref(),
                )
                .await
                {
                    Ok(valid) => valid,
                    Err(error) => return Err(DispatchError::Shared(fill.fail(error))),
                };
                if authority_status != ryeos_engine::launch_config::LaunchConfigProofStatus::Current
                {
                    if let Err(error) =
                        consume_prepared_proof_status(authority_status, &mut authority_retries)
                    {
                        return Err(DispatchError::Shared(fill.fail(error)));
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
                let engine = request.engine.clone();
                let parsers = request.parsers.clone();
                let trust_store = request.trust_store.clone();
                let load_roots = roots.clone();
                let load_declarations = declarations.clone();
                let load_materialization = config_materialization.clone();
                let snapshot = tokio::task::spawn_blocking(move || {
                    load_launch_config_set_under_current_authority(
                        &load_declarations,
                        &load_roots,
                        &parsers,
                        &engine,
                        &trust_store,
                        load_materialization.as_ref(),
                    )
                })
                .await
                .map_err(|error| {
                    DispatchError::Internal(anyhow::anyhow!(
                        "launch config blocking worker failed: {error}"
                    ))
                })??;
                let snapshot = Arc::new(snapshot);
                let status =
                    launch_config_hit_status(&snapshot, &roots, config_materialization.as_ref())
                        .await?;
                if status == ryeos_engine::launch_config::LaunchConfigProofStatus::Current {
                    return Ok(snapshot);
                }
                consume_prepared_proof_status(status, &mut authority_retries)?;
            }
        }
        if authority_retries >= 3 {
            return Err(preparation_error(
                "launch_config_authority_changed",
                "launch config authority changed repeatedly while loading its verified snapshot",
                LaunchPrepareErrorClass::Internal,
            ));
        }
    }
}

async fn launch_config_hit_status(
    snapshot: &Arc<ryeos_engine::launch_config::LaunchConfigSnapshotSet>,
    roots: &ResolutionRoots,
    materialization: Option<&ryeos_app::resolution_cache::ResolutionMaterializationBinding>,
) -> Result<ryeos_engine::launch_config::LaunchConfigProofStatus, DispatchError> {
    let snapshot = snapshot.clone();
    let roots = roots.clone();
    let materialization = materialization.cloned();
    tokio::task::spawn_blocking(move || {
        let project = match materialization.as_ref() {
            Some(binding) => match binding.authoritative_project_content() {
                Ok(project) => project.map(|(root, content)| {
                    (
                        root,
                        content as &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
                    )
                }),
                Err(_) => {
                    return ryeos_engine::launch_config::LaunchConfigProofStatus::ImmutableAuthorityMismatch;
                }
            },
            None => None,
        };
        snapshot
            .dependency_proof
            .revalidate_under_authority_status(&roots, project)
    })
    .await
    .map_err(|error| {
        DispatchError::Internal(anyhow::anyhow!(
            "launch config revalidation worker failed: {error}"
        ))
    })
}

fn prepare_runtime_launch_inputs(
    request: &PrepareRuntimeLaunchRequest<'_>,
    cached_config_set: Option<&ryeos_engine::launch_config::LaunchConfigSnapshotSet>,
) -> Result<PreparedRuntimeLaunchInputs, DispatchError> {
    validate_ref_bindings(request.ref_bindings)?;
    let contract = &request.runtime.yaml.launch_contract;
    validate_prepared_item(
        PreparedItemRole::Primary,
        &request.primary.root.resolved_ref,
        request.primary.root.source_space,
        request.primary.effective_trust_class,
        &contract.primary_allowed_kinds,
        &contract.primary_allowed_spaces,
        &contract.primary_allowed_trust,
    )?;

    for (name, declaration) in &contract.ref_bindings {
        if declaration.required && !request.ref_bindings.contains_key(name) {
            return Err(preparation_error_with_binding(
                "ref_binding_required",
                format!("required ref binding `{name}` is missing"),
                LaunchPrepareErrorClass::Caller,
                Some(name.clone()),
            ));
        }
    }
    for name in request.ref_bindings.keys() {
        if !contract.ref_bindings.contains_key(name) {
            return Err(preparation_error_with_binding(
                "invalid_ref_binding",
                format!("ref binding `{name}` is not declared by the selected runtime"),
                LaunchPrepareErrorClass::Caller,
                Some(name.clone()),
            ));
        }
    }

    let scopes = principal_scopes(request.principal);
    let mut binding_wires = BTreeMap::new();
    let mut binding_records = BTreeMap::new();
    let ref_binding_resolution_timer = request
        .ref_binding_resolution_timings
        .map(|timings| timings.nested("background_dispatch", "ref_binding_resolution"));
    for (name, raw_ref) in request.ref_bindings {
        let declaration = &contract.ref_bindings[name];
        let canonical =
            ryeos_engine::canonical_ref::CanonicalRef::parse(raw_ref).map_err(|_| {
                preparation_error_with_binding(
                    "invalid_ref_binding",
                    format!("ref binding `{name}` is not a canonical ref"),
                    LaunchPrepareErrorClass::Caller,
                    Some(name.clone()),
                )
            })?;
        let required_cap = canonical_cap(&canonical.kind, &canonical.bare_id, "execute");
        ryeos_runtime::authorizer::Authorizer::new()
            .authorize(&scopes, &AuthorizationPolicy::require(&required_cap))
            .map_err(|_| {
                launch_policy_forbidden(
                    "ref_binding_unauthorized",
                    format!("ref binding `{name}` is not authorized"),
                    Some(name.clone()),
                )
            })?;
        if !declaration.allowed_kinds.contains(&canonical.kind) {
            return Err(preparation_error_with_binding(
                "ref_binding_kind_not_allowed",
                format!("binding `{name}` kind `{}` is not allowed", canonical.kind),
                LaunchPrepareErrorClass::Caller,
                Some(name.clone()),
            ));
        }
        let resolution = resolve_binding_closure(request, name, &canonical)?;
        validate_prepared_item(
            PreparedItemRole::Binding(name),
            &resolution.root.resolved_ref,
            resolution.root.source_space,
            resolution.effective_trust_class,
            &declaration.allowed_kinds,
            &declaration.allowed_spaces,
            &declaration.allowed_trust,
        )?;
        binding_records.insert(
            name.clone(),
            RefBindingLaunchRecord {
                canonical_ref: canonical.to_string(),
                source_space: resolution.root.source_space,
                effective_trust_class: resolution.effective_trust_class,
                resolution: resolution.as_launched_digest(),
            },
        );
        binding_wires.insert(name.clone(), prepared_item_wire(&resolution)?);
    }
    drop(ref_binding_resolution_timer);

    let loaded_config_set;
    let config_set = if let Some(config_set) = cached_config_set {
        config_set
    } else {
        let launch_config_roots = request.engine.launch_config_roots(request.roots);
        loaded_config_set = load_launch_config_set_under_current_authority(
            &contract.config_inputs,
            &launch_config_roots,
            request.parsers,
            request.engine,
            request.trust_store,
            request
                .resolution_cache
                .map(|context| context.materialization),
        )?;
        &loaded_config_set
    };
    Ok(PreparedRuntimeLaunchInputs {
        primary: prepared_item_wire(request.primary)?,
        binding_wires,
        binding_records,
        config_inputs: config_set.snapshots.clone(),
        config_dependency_proof: config_set.dependency_proof.clone(),
    })
}

fn resolve_binding_closure(
    request: &PrepareRuntimeLaunchRequest<'_>,
    name: &str,
    canonical: &ryeos_engine::canonical_ref::CanonicalRef,
) -> Result<std::sync::Arc<ResolutionOutput>, DispatchError> {
    let Some(cache_context) = request.resolution_cache else {
        let project_root = request
            .roots
            .ordered
            .iter()
            .find(|root| root.space == ryeos_engine::contracts::ItemSpace::Project)
            .and_then(|root| root.ai_root.parent())
            .map(std::path::Path::to_path_buf);
        match request.subject_resolution_authority {
            ryeos_engine::contracts::SubjectResolutionAuthority::Projectless => {
                if project_root.is_some() {
                    return Err(preparation_error_with_binding(
                        "ref_binding_resolution_authority_invalid",
                        format!("projectless binding `{name}` unexpectedly has a project root"),
                        LaunchPrepareErrorClass::Internal,
                        Some(name.to_owned()),
                    ));
                }
            }
            ryeos_engine::contracts::SubjectResolutionAuthority::LiveFs => {
                if project_root.is_none() {
                    return Err(preparation_error_with_binding(
                        "ref_binding_resolution_authority_invalid",
                        format!("live binding `{name}` has no project root"),
                        LaunchPrepareErrorClass::Internal,
                        Some(name.to_owned()),
                    ));
                }
            }
            ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration { .. }
            | ryeos_engine::contracts::SubjectResolutionAuthority::CowWorkspace { .. } => {
                return Err(preparation_error_with_binding(
                    "ref_binding_resolution_authority_missing",
                    format!(
                        "content-addressed binding `{name}` requires an admitted project materialization authority"
                    ),
                    LaunchPrepareErrorClass::Internal,
                    Some(name.to_owned()),
                ));
            }
        }
        let (output, probed_absent) =
            ryeos_engine::resolution::run_resolution_pipeline_with_probes(
                canonical,
                &request.engine.kinds,
                request.parsers,
                request.roots,
                request.trust_store,
                &request.engine.composers,
            )
            .map_err(|error| map_binding_resolution_error(name, error))?;
        let closure = ryeos_app::resolution_cache::ResolvedClosure::new_with_probes(
            output,
            request.subject_resolution_authority.clone(),
            project_root,
            None,
            probed_absent,
        )
        .map_err(|error| {
            preparation_error_with_binding(
                "ref_binding_resolution_invalid",
                format!("retain diagnostic ref binding `{name}`: {error:#}"),
                LaunchPrepareErrorClass::Internal,
                Some(name.to_owned()),
            )
        })?;
        if !closure
            .validates_current_diagnostic_authority()
            .map_err(|error| {
                preparation_error_with_binding(
                    "ref_binding_resolution_authority_invalid",
                    format!("validate diagnostic ref binding `{name}` authority: {error:#}"),
                    LaunchPrepareErrorClass::Internal,
                    Some(name.to_owned()),
                )
            })?
        {
            return Err(preparation_error_with_binding(
                "ref_binding_resolution_authority_changed",
                format!("diagnostic ref binding `{name}` changed during launch preparation"),
                LaunchPrepareErrorClass::Internal,
                Some(name.to_owned()),
            ));
        }
        return Ok(closure.output_arc());
    };
    if cache_context.materialization.subject_authority() != request.subject_resolution_authority {
        return Err(preparation_error_with_binding(
            "ref_binding_resolution_authority_mismatch",
            format!("binding `{name}` request and admitted materialization authorities differ"),
            LaunchPrepareErrorClass::Internal,
            Some(name.to_owned()),
        ));
    }
    let engine_generation = request.engine.registered_bundle_generation_fingerprint();
    if cache_context.generation_identity != engine_generation {
        return Err(preparation_error_with_binding(
            "ref_binding_resolution_generation_mismatch",
            format!("binding `{name}` cache generation differs from its request engine"),
            LaunchPrepareErrorClass::Internal,
            Some(name.to_owned()),
        ));
    }
    let cache_key = ryeos_app::resolution_cache::build_resolution_cache_key_from_identity(
        request.engine,
        canonical,
        cache_context.materialization.subject_authority().clone(),
        cache_context.materialization.active_project_root(),
        cache_context.plan_context_identity.to_owned(),
    );
    let build_closure = || {
        let project_content = cache_context
            .materialization
            .authoritative_project_content()
            .map_err(|error| {
                preparation_error_with_binding(
                    "ref_binding_resolution_authority_invalid",
                    format!("open admitted ref binding `{name}` authority: {error:#}"),
                    LaunchPrepareErrorClass::Internal,
                    Some(name.to_owned()),
                )
            })?;
        let resolution = match project_content {
            Some((project_root, content)) => {
                ryeos_engine::resolution::run_resolution_pipeline_with_probes_under_project_authority(
                    canonical,
                    &request.engine.kinds,
                    request.parsers,
                    request.roots,
                    request.trust_store,
                    &request.engine.composers,
                    project_root,
                    content,
                )
            }
            None => ryeos_engine::resolution::run_resolution_pipeline_with_probes(
                canonical,
                &request.engine.kinds,
                request.parsers,
                request.roots,
                request.trust_store,
                &request.engine.composers,
            ),
        };
        let (output, probed_absent) =
            resolution.map_err(|error| map_binding_resolution_error(name, error))?;
        let resolution_root = cache_context
            .materialization
            .active_project_root()
            .map(std::path::Path::to_path_buf);
        let closure = ryeos_app::resolution_cache::ResolvedClosure::new_with_probes(
            output,
            cache_context.materialization.subject_authority().clone(),
            resolution_root,
            cache_context
                .materialization
                .materialization_lifeline()
                .cloned(),
            probed_absent,
        )
        .map(std::sync::Arc::new)
        .map_err(|error| {
            preparation_error_with_binding(
                "ref_binding_resolution_invalid",
                format!("retain ref binding `{name}`: {error:#}"),
                LaunchPrepareErrorClass::Internal,
                Some(name.to_owned()),
            )
        })?;
        Ok::<_, DispatchError>(closure)
    };
    let mut mutable_authority_races = 0_usize;
    loop {
        match cache_context
            .cache
            .begin_admitted(&cache_key, cache_context.materialization)
            .map_err(|error| {
                preparation_error_with_binding(
                    "ref_binding_resolution_authority_invalid",
                    format!("resolve ref binding `{name}` cache authority: {error:#}"),
                    LaunchPrepareErrorClass::Internal,
                    Some(name.to_owned()),
                )
            })? {
            ryeos_app::resolution_cache::ResolutionLookup::Hit(cached) => {
                ryeos_app::resolution_cache::emit_resolution_cache_metric(
                    ryeos_app::resolution_cache::ResolutionCacheMetric::LaunchBinding,
                    ryeos_app::resolution_cache::ResolutionCachePhase::Binding,
                    ryeos_app::resolution_cache::ResolutionCacheOutcome::Hit,
                    Some(ryeos_app::resolution_cache::ResolutionCacheReason::Ready),
                    0,
                );
                return Ok(cached.output_arc());
            }
            ryeos_app::resolution_cache::ResolutionLookup::Wait(wait) => {
                ryeos_app::resolution_cache::emit_resolution_cache_metric(
                    ryeos_app::resolution_cache::ResolutionCacheMetric::LaunchBinding,
                    ryeos_app::resolution_cache::ResolutionCachePhase::Binding,
                    ryeos_app::resolution_cache::ResolutionCacheOutcome::SingleFlightWait,
                    None,
                    0,
                );
                match wait.wait_blocking().map_err(|error| {
                    error
                        .downcast::<DispatchError>()
                        .map(DispatchError::Shared)
                        .unwrap_or_else(|| {
                            preparation_error_with_binding(
                                "ref_binding_resolution_authority_invalid",
                                format!("ref binding `{name}` resolution failed: {error}"),
                                LaunchPrepareErrorClass::Internal,
                                Some(name.to_owned()),
                            )
                        })
                })? {
                    Some(published) => return Ok(published.output_arc()),
                    None => {
                        mutable_authority_races = mutable_authority_races.saturating_add(1);
                    }
                }
            }
            ryeos_app::resolution_cache::ResolutionLookup::Build(fill) => {
                ryeos_app::resolution_cache::emit_resolution_cache_metric(
                    ryeos_app::resolution_cache::ResolutionCacheMetric::LaunchBinding,
                    ryeos_app::resolution_cache::ResolutionCachePhase::Binding,
                    ryeos_app::resolution_cache::ResolutionCacheOutcome::Miss,
                    None,
                    0,
                );
                let closure = match build_closure() {
                    Ok(closure) => closure,
                    Err(error) => {
                        return Err(DispatchError::Shared(fill.fail_typed_error(error)));
                    }
                };
                let published = fill
                    .complete(closure.clone(), closure.probed_absent().to_vec())
                    .map_err(|error| {
                        preparation_error_with_binding(
                            "ref_binding_resolution_authority_invalid",
                            format!("ref binding `{name}` resolution failed: {error}"),
                            LaunchPrepareErrorClass::Internal,
                            Some(name.to_owned()),
                        )
                    })?;
                if let Some(published) = published {
                    return Ok(published.output_arc());
                }
                mutable_authority_races = mutable_authority_races.saturating_add(1);
            }
            ryeos_app::resolution_cache::ResolutionLookup::Bypass => {
                ryeos_app::resolution_cache::emit_resolution_cache_metric(
                    ryeos_app::resolution_cache::ResolutionCacheMetric::LaunchBinding,
                    ryeos_app::resolution_cache::ResolutionCachePhase::Binding,
                    ryeos_app::resolution_cache::ResolutionCacheOutcome::Bypass,
                    Some(ryeos_app::resolution_cache::ResolutionCacheReason::PendingCapacity),
                    0,
                );
                let closure = build_closure()?;
                if !cache_context
                    .materialization
                    .validates_closure(&closure)
                    .map_err(|error| {
                        preparation_error_with_binding(
                            "ref_binding_resolution_authority_invalid",
                            format!("validate uncached ref binding `{name}` authority: {error:#}"),
                            LaunchPrepareErrorClass::Internal,
                            Some(name.to_owned()),
                        )
                    })?
                {
                    return Err(preparation_error_with_binding(
                        "ref_binding_resolution_authority_changed",
                        format!("uncached ref binding `{name}` changed before launch preparation"),
                        LaunchPrepareErrorClass::Internal,
                        Some(name.to_owned()),
                    ));
                }
                return Ok(closure.output_arc());
            }
        }
        if mutable_authority_races > ryeos_app::resolution_cache::MAX_MUTABLE_AUTHORITY_RACE_RETRIES
        {
            return Err(preparation_error_with_binding(
                "ref_binding_resolution_authority_unstable",
                format!("ref binding `{name}` changed repeatedly under live project authority"),
                LaunchPrepareErrorClass::Internal,
                Some(name.to_owned()),
            ));
        }
    }
}

fn finish_runtime_launch_preparation(
    request: &PrepareRuntimeLaunchRequest<'_>,
    inputs: &PreparedRuntimeLaunchInputs,
) -> Result<PreparedRuntimeLaunch, DispatchError> {
    finish_runtime_launch_preparation_parts(
        request.engine,
        request.runtime,
        request.ref_bindings,
        inputs,
    )
}

fn finish_runtime_launch_preparation_parts(
    engine: &ryeos_engine::engine::Engine,
    runtime: &VerifiedRuntime,
    ref_bindings: &BTreeMap<String, String>,
    inputs: &PreparedRuntimeLaunchInputs,
) -> Result<PreparedRuntimeLaunch, DispatchError> {
    let contract = &runtime.yaml.launch_contract;
    let mut result = match &contract.preparation {
        LaunchPreparationDecl::None => ryeos_handler_protocol::LaunchPrepareSuccess {
            runtime_data: BTreeMap::new(),
            required_secrets: Vec::new(),
            runtime_facts: BTreeMap::new(),
            financial_authority: ryeos_handler_protocol::FinancialAuthorityResultWire::None,
        },
        LaunchPreparationDecl::Handler { config, .. } => {
            let handler_request = LaunchPrepareRequest {
                handler_config: config.clone(),
                primary: inputs.primary.clone(),
                ref_bindings: inputs.binding_wires.clone(),
                config_inputs: inputs.config_inputs.clone(),
            };
            match engine
                .launch_preparers
                .prepare(&runtime.canonical_ref, handler_request)
                .map_err(map_launch_preparer_host_error)?
            {
                LaunchPrepareResponse::Success { result } => result,
                LaunchPrepareResponse::Error { error } => {
                    return Err(handler_preparation_error(error, ref_bindings));
                }
            }
        }
    };

    validate_result(contract, ref_bindings, &inputs.config_inputs, &mut result)?;
    let config_contributors = collect_config_contributors(&inputs.config_inputs);
    let financial_authority = validate_financial_authority(contract, result.financial_authority)?;
    Ok(PreparedRuntimeLaunch {
        runtime_data: result.runtime_data,
        required_secrets: result
            .required_secrets
            .into_iter()
            .map(|requirement| PreparedSecret {
                name: requirement.name,
                origin: requirement.origin,
            })
            .collect(),
        runtime_facts: result.runtime_facts,
        binding_records: inputs.binding_records.clone(),
        config_contributors,
        financial_authority,
    })
}

fn prepared_launch_skeleton_key(
    request: &PrepareRuntimeLaunchRequest<'_>,
    inputs: &PreparedRuntimeLaunchInputs,
    authority: &OwnedPreparedLaunchSkeletonAuthority,
) -> Result<String, DispatchError> {
    let config_dependency_digest = inputs
        .config_dependency_proof
        .identity_digest()
        .map_err(map_launch_config_error)?;
    let execution_project_authority_identity = authority
        .execution_project_authority
        .stable_cache_identity()
        .map_err(|error| {
            preparation_error(
                "prepared_launch_project_authority_invalid",
                format!("derive stable execution project authority identity: {error:#}"),
                LaunchPrepareErrorClass::Internal,
            )
        })?;
    let value = serde_json::json!({
        "schema_version": 1,
        "request_engine_generation_identity": authority.request_engine_generation_identity,
        "effective_trust_identity": authority.effective_trust_identity,
        "subject_resolution_authority": authority.subject_resolution_authority,
        "execution_project_authority": execution_project_authority_identity,
        "lifecycle_authority": authority.lifecycle_authority,
        "runtime": {
            "canonical_ref": request.runtime.canonical_ref.to_string(),
            "content_hash": &request.runtime.raw_content_digest,
            "signer_fingerprint": &request.runtime.signer_fingerprint,
            "launch_contract": &request.runtime.yaml.launch_contract,
        },
        "protocol": {
            "canonical_ref": authority.protocol.canonical_ref.to_string(),
            "content_hash": &authority.protocol.raw_content_digest,
            "signer_fingerprint": &authority.protocol.signer_fingerprint,
        },
        "executor_chain_identity": authority.executor_chain_identity,
        "primary_post_augmentation": &inputs.primary,
        "ref_bindings": request.ref_bindings,
        "resolved_binding_records": &inputs.binding_records,
        "config_inputs": &inputs.config_inputs,
        "config_dependency_digest": config_dependency_digest,
    });
    let canonical = lillux::canonical_json(&value).map_err(|error| {
        preparation_error(
            "prepared_launch_skeleton_key_invalid",
            format!("canonicalize prepared launch skeleton key: {error}"),
            LaunchPrepareErrorClass::Internal,
        )
    })?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

fn collect_config_contributors(
    inputs: &BTreeMap<String, LaunchConfigSnapshotWire>,
) -> Vec<LaunchConfigContributorWire> {
    let mut contributors = inputs
        .values()
        .flat_map(|snapshot| match snapshot {
            LaunchConfigSnapshotWire::Item { contributors, .. } => {
                contributors.iter().collect::<Vec<_>>()
            }
            LaunchConfigSnapshotWire::Catalog { entries } => entries
                .values()
                .flat_map(|entry| entry.contributors.iter())
                .collect(),
        })
        .cloned()
        .collect::<Vec<_>>();
    contributors.sort_by(|left, right| {
        (
            &left.root_label,
            &left.canonical_id,
            &left.content_digest,
            &left.signer_fingerprint,
        )
            .cmp(&(
                &right.root_label,
                &right.canonical_id,
                &right.content_digest,
                &right.signer_fingerprint,
            ))
    });
    contributors.dedup_by(|left, right| {
        left.root_label == right.root_label
            && left.canonical_id == right.canonical_id
            && left.content_digest == right.content_digest
            && left.signer_fingerprint == right.signer_fingerprint
            && left.space == right.space
            && left.trust_class == right.trust_class
    });
    contributors
}

/// Validate the declared financial-authority result and seal it: strict
/// bounded shape, strict typed decode with an internal digest check,
/// canonicalization, and hashing. No item-kind or provider-name branch —
/// only the contract-declared kind selects the decoder.
fn validate_financial_authority(
    contract: &ryeos_engine::runtime_registry::LaunchContractDecl,
    result: ryeos_handler_protocol::FinancialAuthorityResultWire,
) -> Result<Option<PreparedFinancialAuthority>, DispatchError> {
    use ryeos_engine::runtime_registry::FinancialAuthorityDecl;
    use ryeos_handler_protocol::FinancialAuthorityResultWire;

    match (contract.financial_authority, result) {
        (FinancialAuthorityDecl::None, FinancialAuthorityResultWire::None) => Ok(None),
        (
            FinancialAuthorityDecl::ProviderAccountingAuthorityV1,
            FinancialAuthorityResultWire::ProviderAccountingAuthorityV1 { authority },
        ) => {
            validate_json_value(
                "financial_authority",
                &authority,
                MAX_FINANCIAL_AUTHORITY_BYTES,
            )?;
            let decoded: ryeos_accounting::ProviderAccountingAuthority =
                serde_json::from_value(authority.clone()).map_err(|error| {
                    preparation_error(
                        "financial_authority_invalid",
                        format!("financial authority does not decode strictly: {error}"),
                        LaunchPrepareErrorClass::Internal,
                    )
                })?;
            decoded.validate().map_err(|error| {
                preparation_error(
                    "financial_authority_invalid",
                    format!("financial authority failed validation: {error}"),
                    LaunchPrepareErrorClass::Internal,
                )
            })?;
            let spend_bound = match &decoded.spend_bound {
                ryeos_accounting::SpendBoundAuthority::Paid { .. } => SealedSpendBound::Paid,
                ryeos_accounting::SpendBoundAuthority::ExplicitlyFree { .. } => {
                    SealedSpendBound::ExplicitlyFree
                }
                ryeos_accounting::SpendBoundAuthority::AdvisoryOnly => {
                    SealedSpendBound::AdvisoryOnly
                }
            };
            // Re-encode the typed value so the sealed canonical form cannot
            // carry byte-level variance the strict decode ignored.
            let canonical_value = serde_json::to_value(&decoded)
                .map_err(|error| DispatchError::Internal(error.into()))?;
            let canonical = lillux::canonical_json(&canonical_value).map_err(|error| {
                preparation_error(
                    "financial_authority_invalid",
                    format!("financial authority is not canonical JSON: {error}"),
                    LaunchPrepareErrorClass::Internal,
                )
            })?;
            Ok(Some(PreparedFinancialAuthority {
                kind: FINANCIAL_AUTHORITY_KIND_PROVIDER_ACCOUNTING_V1.to_owned(),
                authority: canonical_value,
                authority_digest: lillux::sha256_hex(canonical.as_bytes()),
                spend_bound,
            }))
        }
        (declared, produced) => Err(preparation_error(
            "financial_authority_mismatch",
            format!(
                "launch contract declares financial authority {declared:?} but preparation \
                 produced {}",
                match produced {
                    FinancialAuthorityResultWire::None => "none",
                    FinancialAuthorityResultWire::ProviderAccountingAuthorityV1 { .. } =>
                        "provider_accounting_authority_v1",
                }
            ),
            LaunchPrepareErrorClass::Internal,
        )),
    }
}

/// Validate daemon-wide syntax and size caps for a serialized secondary
/// execution identity before authorization, forwarding, or preparation.
pub fn validate_ref_bindings(ref_bindings: &BTreeMap<String, String>) -> Result<(), DispatchError> {
    if ref_bindings.len() > MAX_REF_BINDINGS {
        return Err(preparation_error(
            "invalid_ref_binding",
            format!("ref_bindings exceeds the daemon limit of {MAX_REF_BINDINGS}"),
            LaunchPrepareErrorClass::Caller,
        ));
    }
    for (name, raw_ref) in ref_bindings {
        if !valid_ref_binding_name(name) {
            return Err(preparation_error_with_binding(
                "invalid_ref_binding",
                format!(
                    "ref binding names must be lower snake case and at most \
                     {MAX_REF_BINDING_NAME_BYTES} bytes"
                ),
                LaunchPrepareErrorClass::Caller,
                None,
            ));
        }
        if raw_ref.len() > MAX_REF_BINDING_VALUE_BYTES {
            return Err(preparation_error_with_binding(
                "invalid_ref_binding",
                format!("ref binding `{name}` exceeds {MAX_REF_BINDING_VALUE_BYTES} UTF-8 bytes"),
                LaunchPrepareErrorClass::Caller,
                Some(name.clone()),
            ));
        }
        let canonical =
            ryeos_engine::canonical_ref::CanonicalRef::parse(raw_ref).map_err(|_| {
                preparation_error_with_binding(
                    "invalid_ref_binding",
                    format!("ref binding `{name}` is not a canonical ref"),
                    LaunchPrepareErrorClass::Caller,
                    Some(name.clone()),
                )
            })?;
        if canonical.to_string() != *raw_ref {
            return Err(preparation_error_with_binding(
                "invalid_ref_binding",
                format!("ref binding `{name}` is not in canonical form"),
                LaunchPrepareErrorClass::Caller,
                Some(name.clone()),
            ));
        }
    }
    Ok(())
}

fn valid_ref_binding_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_REF_BINDING_NAME_BYTES {
        return false;
    }
    let mut segments = name.split('_');
    let Some(first) = segments.next() else {
        return false;
    };
    first.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && first
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && segments.all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

#[derive(Clone, Copy)]
enum PreparedItemRole<'a> {
    Primary,
    Binding(&'a str),
}

fn validate_prepared_item(
    role: PreparedItemRole<'_>,
    canonical_ref: &str,
    source_space: ItemSpace,
    trust: TrustClass,
    allowed_kinds: &[String],
    allowed_spaces: &[LaunchItemSpace],
    allowed_trust: &[TrustClass],
) -> Result<(), DispatchError> {
    let display_name = match role {
        PreparedItemRole::Primary => "primary",
        PreparedItemRole::Binding(name) => name,
    };
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(canonical_ref)
        .map_err(|error| DispatchError::InvalidRef(canonical_ref.to_owned(), error.to_string()))?;
    if !allowed_kinds.contains(&canonical.kind) {
        let (code, binding) = match role {
            PreparedItemRole::Primary => ("invalid_primary_kind", None),
            PreparedItemRole::Binding(name) => {
                ("ref_binding_kind_not_allowed", Some(name.to_owned()))
            }
        };
        return Err(preparation_error_with_binding(
            code,
            format!("{display_name} kind `{}` is not allowed", canonical.kind),
            LaunchPrepareErrorClass::Caller,
            binding,
        ));
    }
    let space = match source_space {
        ItemSpace::Bundle => LaunchItemSpace::Bundle,
        ItemSpace::Project => LaunchItemSpace::Project,
        ItemSpace::Node => LaunchItemSpace::Node,
    };
    if !allowed_spaces.contains(&space) {
        let (code, binding) = match role {
            PreparedItemRole::Primary => ("primary_space_not_allowed", None),
            PreparedItemRole::Binding(name) => {
                ("ref_binding_space_not_allowed", Some(name.to_owned()))
            }
        };
        return Err(launch_policy_forbidden(
            code,
            format!("{display_name} source space is not allowed"),
            binding,
        ));
    }
    if !allowed_trust.contains(&trust) {
        let (code, binding) = match role {
            PreparedItemRole::Primary => ("primary_untrusted", None),
            PreparedItemRole::Binding(name) => ("ref_binding_untrusted", Some(name.to_owned())),
        };
        return Err(launch_policy_forbidden(
            code,
            format!("{display_name} trust class is not allowed"),
            binding,
        ));
    }
    Ok(())
}

fn prepared_item_wire(
    resolution: &ResolutionOutput,
) -> Result<LaunchPreparedItemWire, DispatchError> {
    Ok(LaunchPreparedItemWire {
        canonical_ref: resolution.root.resolved_ref.clone(),
        source_space: item_space_wire(resolution.root.source_space),
        effective_trust_class: trust_wire(resolution.effective_trust_class),
        composed: LaunchComposedViewWire {
            composed: resolution.composed.composed.clone(),
            derived: resolution
                .composed
                .derived
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            policy_facts: resolution
                .composed
                .policy_facts
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        },
        resolution_digest: serde_json::to_value(resolution.as_launched_digest())
            .map_err(|error| DispatchError::Internal(error.into()))?,
    })
}

fn validate_result(
    contract: &ryeos_engine::runtime_registry::LaunchContractDecl,
    ref_bindings: &BTreeMap<String, String>,
    config_inputs: &BTreeMap<String, LaunchConfigSnapshotWire>,
    result: &mut ryeos_handler_protocol::LaunchPrepareSuccess,
) -> Result<(), DispatchError> {
    let expected: BTreeSet<_> = contract.required_runtime_data.iter().collect();
    let actual: BTreeSet<_> = result.runtime_data.keys().collect();
    if actual != expected {
        return Err(preparation_error(
            "launch_preparer_runtime_data_mismatch",
            format!(
                "runtime_data keys do not match signed contract: expected {expected:?}, got {actual:?}"
            ),
            LaunchPrepareErrorClass::Internal,
        ));
    }
    let mut aggregate = 0usize;
    for (name, value) in &result.runtime_data {
        aggregate = aggregate.saturating_add(validate_json_value(
            name,
            value,
            MAX_RUNTIME_DATA_VALUE_BYTES,
        )?);
    }
    if aggregate > MAX_RUNTIME_DATA_BYTES {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            "aggregate runtime_data exceeds daemon limit",
            LaunchPrepareErrorClass::Internal,
        ));
    }

    if result.required_secrets.len() > MAX_SECRET_ORIGINS {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            "too many symbolic secret origins",
            LaunchPrepareErrorClass::Internal,
        ));
    }
    let allowed_secrets: BTreeSet<_> = contract.secret_policy.allowed_names.iter().collect();
    let mut unique_names = BTreeSet::new();
    let mut unique_origins = BTreeMap::new();
    for requirement in &result.required_secrets {
        if !allowed_secrets.contains(&requirement.name) {
            return Err(preparation_error(
                "launch_secret_not_allowed",
                format!(
                    "secret `{}` is not allowed by the signed contract",
                    requirement.name
                ),
                LaunchPrepareErrorClass::Internal,
            ));
        }
        validate_secret_origin(&requirement.origin, ref_bindings, config_inputs)?;
        let origin_value = serde_json::to_value(&requirement.origin)
            .map_err(|error| DispatchError::Internal(error.into()))?;
        let origin = lillux::canonical_json(&origin_value).map_err(|error| {
            preparation_error(
                "launch_secret_origin_invalid",
                format!("secret origin cannot be represented as canonical JSON: {error}"),
                LaunchPrepareErrorClass::Internal,
            )
        })?;
        unique_origins
            .entry((requirement.name.clone(), origin))
            .or_insert_with(|| requirement.clone());
        unique_names.insert(requirement.name.clone());
    }
    result.required_secrets = unique_origins.into_values().collect();
    if result.required_secrets.len() > MAX_SECRET_ORIGINS {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            "too many deduplicated symbolic secret origins",
            LaunchPrepareErrorClass::Internal,
        ));
    }
    if unique_names.len() > MAX_SECRET_NAMES
        || unique_names.len() > usize::from(contract.secret_policy.max_requirements)
    {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            "symbolic secret requirement limit exceeded",
            LaunchPrepareErrorClass::Internal,
        ));
    }

    let mut facts_bytes = 0usize;
    for (name, declaration) in &contract.runtime_facts {
        if declaration.required && !result.runtime_facts.contains_key(name) {
            return Err(preparation_error(
                "runtime_fact_missing",
                format!("required runtime fact `{name}` is missing"),
                LaunchPrepareErrorClass::Internal,
            ));
        }
    }
    for (name, value) in &result.runtime_facts {
        let declaration = contract.runtime_facts.get(name).ok_or_else(|| {
            preparation_error(
                "runtime_fact_undeclared",
                format!("runtime fact `{name}` is undeclared"),
                LaunchPrepareErrorClass::Internal,
            )
        })?;
        let kind_ok = match declaration.kind {
            RuntimeFactKind::Bool => value.is_boolean(),
            RuntimeFactKind::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
            RuntimeFactKind::String => value.is_string(),
            RuntimeFactKind::Json => true,
        };
        if !kind_ok {
            return Err(preparation_error(
                "runtime_fact_type_invalid",
                format!("runtime fact `{name}` has the wrong type"),
                LaunchPrepareErrorClass::Internal,
            ));
        }
        let bytes = canonical_json_len(name, value)?;
        if bytes > declaration.max_bytes as usize {
            return Err(preparation_error(
                "runtime_fact_too_large",
                format!("runtime fact `{name}` exceeds its signed size"),
                LaunchPrepareErrorClass::Internal,
            ));
        }
        facts_bytes = facts_bytes.saturating_add(bytes);
    }
    if facts_bytes > MAX_RUNTIME_FACT_BYTES {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            "aggregate runtime facts exceed daemon limit",
            LaunchPrepareErrorClass::Internal,
        ));
    }
    Ok(())
}

fn validate_secret_origin(
    origin: &LaunchSecretOriginWire,
    ref_bindings: &BTreeMap<String, String>,
    config_inputs: &BTreeMap<String, LaunchConfigSnapshotWire>,
) -> Result<(), DispatchError> {
    match origin {
        LaunchSecretOriginWire::Binding { name } if ref_bindings.contains_key(name) => Ok(()),
        LaunchSecretOriginWire::Binding { name } => Err(preparation_error(
            "launch_secret_origin_invalid",
            format!("unknown binding origin `{name}`"),
            LaunchPrepareErrorClass::Internal,
        )),
        LaunchSecretOriginWire::ConfigInput {
            name,
            canonical_id,
            value_digest,
        } => {
            let valid = match config_inputs.get(name) {
                Some(LaunchConfigSnapshotWire::Item {
                    present: true,
                    value_digest: Some(actual),
                    contributors,
                    ..
                }) => {
                    actual == value_digest
                        && contributors
                            .iter()
                            .any(|source| source.canonical_id == *canonical_id)
                }
                Some(LaunchConfigSnapshotWire::Catalog { entries }) => entries
                    .get(canonical_id)
                    .is_some_and(|entry| entry.value_digest == *value_digest),
                _ => false,
            };
            if valid {
                Ok(())
            } else {
                Err(preparation_error(
                    "launch_secret_origin_invalid",
                    format!(
                        "config origin `{name}/{canonical_id}` does not match its verified snapshot"
                    ),
                    LaunchPrepareErrorClass::Internal,
                ))
            }
        }
    }
}

fn validate_json_value(
    name: &str,
    value: &Value,
    max_bytes: usize,
) -> Result<usize, DispatchError> {
    if json_depth(value) > MAX_JSON_DEPTH {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            format!("`{name}` exceeds JSON depth limit"),
            LaunchPrepareErrorClass::Internal,
        ));
    }
    let bytes = canonical_json_len(name, value)?;
    if bytes > max_bytes {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            format!("`{name}` exceeds byte limit"),
            LaunchPrepareErrorClass::Internal,
        ));
    }
    Ok(bytes)
}

fn canonical_json_len(name: &str, value: &Value) -> Result<usize, DispatchError> {
    lillux::canonical_json(value)
        .map(|canonical| canonical.len())
        .map_err(|error| {
            preparation_error(
                "launch_preparer_value_not_canonical",
                format!("`{name}` cannot be represented as canonical JSON: {error}"),
                LaunchPrepareErrorClass::Internal,
            )
        })
}

fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

fn principal_scopes(principal: &EffectivePrincipal) -> Vec<String> {
    match principal {
        EffectivePrincipal::Local(principal) => principal.scopes.clone(),
        EffectivePrincipal::Delegated(principal) => principal.delegated_scopes.clone(),
    }
}

fn item_space_wire(space: ItemSpace) -> ItemSpaceWire {
    match space {
        ItemSpace::Bundle => ItemSpaceWire::Bundle,
        ItemSpace::Project => ItemSpaceWire::Project,
        ItemSpace::Node => ItemSpaceWire::Node,
    }
}

fn trust_wire(trust: TrustClass) -> TrustClassWire {
    match trust {
        TrustClass::TrustedBundle => TrustClassWire::TrustedBundle,
        TrustClass::TrustedProject => TrustClassWire::TrustedProject,
        TrustClass::TrustedNode => TrustClassWire::TrustedNode,
        TrustClass::UntrustedProject => TrustClassWire::UntrustedProject,
        TrustClass::Unsigned => TrustClassWire::Unsigned,
    }
}

fn map_binding_resolution_error(
    binding: &str,
    error: ryeos_engine::resolution::ResolutionError,
) -> DispatchError {
    use ryeos_engine::resolution::ResolutionError;

    let detail = error.to_string();
    match error {
        ResolutionError::MissingItem { .. } => DispatchError::LaunchResourceNotFound {
            code: "ref_binding_not_found".to_owned(),
            message: format!("ref binding `{binding}` was not found"),
            binding: Some(binding.to_owned()),
        },
        ResolutionError::CycleDetected { .. }
        | ResolutionError::MaxDepthExceeded { .. }
        | ResolutionError::AliasMaxDepthExceeded { .. }
        | ResolutionError::AliasCycle { .. }
        | ResolutionError::UnknownAlias { .. }
        | ResolutionError::IntegrityFailure { .. }
        | ResolutionError::MetadataAnchoringFailed { .. }
        | ResolutionError::KindNotExecutable { .. }
        | ResolutionError::ComposedValueContractViolation { .. } => preparation_error_with_binding(
            "ref_binding_resolution_failed",
            format!("binding `{binding}` has an invalid definition: {detail}"),
            LaunchPrepareErrorClass::Configuration,
            Some(binding.to_owned()),
        ),
        ResolutionError::StepFailed { class, .. } => {
            use ryeos_engine::resolution::ResolutionFailureClass;

            let classification = match class {
                ResolutionFailureClass::InvalidDefinition => LaunchPrepareErrorClass::Configuration,
                ResolutionFailureClass::DependencyUnavailable => {
                    return host_preparation_error_with_binding(
                        "ref_binding_resolution_failed",
                        format!(
                            "binding `{binding}` resolution dependency is unavailable: {detail}"
                        ),
                        "unavailable",
                        Some(binding.to_owned()),
                    );
                }
                ResolutionFailureClass::InternalInvariant => LaunchPrepareErrorClass::Internal,
            };
            preparation_error_with_binding(
                "ref_binding_resolution_failed",
                format!("binding `{binding}` resolution failed: {detail}"),
                classification,
                Some(binding.to_owned()),
            )
        }
    }
}

fn map_launch_preparer_host_error(error: EngineError) -> DispatchError {
    match error {
        EngineError::LaunchPreparerUnavailable { detail, .. } => {
            host_preparation_error("launch_preparer_unavailable", detail, "unavailable")
        }
        EngineError::LaunchPreparerLimitExceeded { detail, .. } => {
            host_preparation_error("launch_preparer_limit_exceeded", detail, "internal")
        }
        EngineError::LaunchPreparerProtocolInvalid { detail, .. } => {
            host_preparation_error("launch_preparer_protocol_invalid", detail, "internal")
        }
        other => host_preparation_error(
            "launch_preparer_protocol_invalid",
            other.to_string(),
            "internal",
        ),
    }
}

fn map_launch_config_error(error: EngineError) -> DispatchError {
    match error {
        EngineError::LaunchConfigMissing { input, detail } => preparation_error(
            "launch_config_missing",
            format!("launch config input `{input}` is missing: {detail}"),
            LaunchPrepareErrorClass::Configuration,
        ),
        EngineError::LaunchConfigPolicyDenied {
            code,
            input,
            detail,
        } => launch_policy_forbidden(
            code,
            format!("launch config input `{input}` is forbidden: {detail}"),
            None,
        ),
        other => preparation_error(
            "launch_config_invalid",
            other.to_string(),
            LaunchPrepareErrorClass::Configuration,
        ),
    }
}

fn handler_preparation_error(
    error: LaunchPrepareError,
    ref_bindings: &BTreeMap<String, String>,
) -> DispatchError {
    if let Err(reason) = validate_handler_error(&error, ref_bindings) {
        return host_preparation_error("launch_preparer_protocol_invalid", reason, "internal");
    }
    let classification = match error.classification {
        LaunchPrepareErrorClass::Caller => "caller",
        LaunchPrepareErrorClass::Configuration => "configuration",
        LaunchPrepareErrorClass::Internal => "internal",
    };
    DispatchError::LaunchPreparationFailed {
        code: error.code,
        message: error.message,
        classification: classification.to_owned(),
        binding: error.binding,
        details: Box::new(error.details),
    }
}

fn validate_handler_error(
    error: &LaunchPrepareError,
    ref_bindings: &BTreeMap<String, String>,
) -> Result<(), String> {
    if !valid_launch_name(&error.code, MAX_HANDLER_ERROR_CODE_BYTES) {
        return Err("launch-preparer error code is not a bounded lower-snake-case name".to_owned());
    }
    if error.message.len() > MAX_HANDLER_ERROR_MESSAGE_BYTES
        || error.message.contains('\n')
        || error.message.contains('\r')
    {
        return Err("launch-preparer error message is not a bounded single line".to_owned());
    }
    if let Some(binding) = &error.binding
        && !ref_bindings.contains_key(binding)
    {
        return Err(format!(
            "launch-preparer error names unknown binding `{binding}`"
        ));
    }
    if error.details.len() > MAX_HANDLER_ERROR_DETAILS {
        return Err("launch-preparer error details exceed the key limit".to_owned());
    }
    for (key, value) in &error.details {
        if !valid_launch_name(key, MAX_HANDLER_ERROR_CODE_BYTES) {
            return Err(format!(
                "launch-preparer error detail key `{key}` is invalid"
            ));
        }
        if let LaunchDiagnosticScalarWire::String(value) = value
            && value.len() > MAX_HANDLER_ERROR_DETAIL_STRING_BYTES
        {
            return Err(format!(
                "launch-preparer error detail `{key}` exceeds the string limit"
            ));
        }
    }
    let value = serde_json::to_value(&error.details)
        .map_err(|encode| format!("encode launch-preparer error details: {encode}"))?;
    let canonical = lillux::canonical_json(&value)
        .map_err(|encode| format!("canonicalize launch-preparer error details: {encode}"))?;
    if canonical.len() > MAX_HANDLER_ERROR_DETAILS_BYTES {
        return Err("launch-preparer error details exceed the aggregate byte limit".to_owned());
    }
    Ok(())
}

fn valid_launch_name(name: &str, max_bytes: usize) -> bool {
    !name.is_empty()
        && name.len() <= max_bytes
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !name.ends_with('_')
        && !name.contains("__")
}

fn preparation_error(
    code: impl Into<String>,
    message: impl Into<String>,
    classification: LaunchPrepareErrorClass,
) -> DispatchError {
    preparation_error_with_binding(code, message, classification, None)
}

fn preparation_error_with_binding(
    code: impl Into<String>,
    message: impl Into<String>,
    classification: LaunchPrepareErrorClass,
    binding: Option<String>,
) -> DispatchError {
    let classification = match classification {
        LaunchPrepareErrorClass::Caller => "caller",
        LaunchPrepareErrorClass::Configuration => "configuration",
        LaunchPrepareErrorClass::Internal => "internal",
    };
    DispatchError::LaunchPreparationFailed {
        code: code.into(),
        message: message.into(),
        classification: classification.to_owned(),
        binding,
        details: Box::new(BTreeMap::new()),
    }
}

fn host_preparation_error(
    code: impl Into<String>,
    message: impl Into<String>,
    classification: &'static str,
) -> DispatchError {
    host_preparation_error_with_binding(code, message, classification, None)
}

fn host_preparation_error_with_binding(
    code: impl Into<String>,
    message: impl Into<String>,
    classification: &'static str,
    binding: Option<String>,
) -> DispatchError {
    DispatchError::LaunchPreparationFailed {
        code: code.into(),
        message: message.into(),
        classification: classification.to_owned(),
        binding,
        details: Box::new(BTreeMap::new()),
    }
}

fn launch_policy_forbidden(
    code: impl Into<String>,
    message: impl Into<String>,
    binding: Option<String>,
) -> DispatchError {
    DispatchError::LaunchPolicyForbidden {
        code: code.into(),
        message: message.into(),
        binding,
    }
}
