//! Admission, recovery, and execution of kind-neutral persistent sessions.
//!
//! A kind owns the executable definition and declares the mechanical session
//! contract.  This module does not interpret request or response bodies.  It
//! captures that already-resolved definition into an immutable capsule before
//! the outer runtime capsule is minted, then reopens only retained content.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ryeos_app::persistent_session::StartedPersistentSession;
use ryeos_app::runtime_db::{WorkerProcessRecord, WorkerProcessState, daemon_generation_id};
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::{ResolvedExecutionRequest, prepare_captured_item_plan};
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
    SubjectResolutionAuthority,
};
use ryeos_engine::kind_registry::{PersistentSessionDecl, TerminatorDecl};
use ryeos_engine::protocols::descriptor::PersistentSessionProcessMode;
use ryeos_engine::protocols::{VerifiedProtocol, validate_persistent_session_protocol};
use ryeos_state::objects::{
    AdmittedPersistentSessionCapsule, ExecutableSearchPathEntry, PERSISTENT_SESSION_CAPSULE_KIND,
    PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION, PersistentSessionAuthority,
    PersistentSessionLifecycleContract, PersistentSessionWireContract,
};

use super::launch_preparation::{
    PreparedContentDependency, PreparedExecutionDependency, PreparedRuntimeLaunch,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistentSessionExactProgram {
    pub(crate) effective_definition_digest: String,
    pub(crate) resolution_output: ryeos_engine::resolution::RetainedResolutionOutput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedPersistentSessionIdentity {
    pub canonical_ref: String,
    pub effective_definition_digest: String,
    pub capsule_hash: String,
    pub execution_realization_hash: String,
}

#[derive(Debug, Clone)]
pub struct ExclusivePersistentSessionIdentity {
    pub placement_thread_id: String,
    pub worker_instance_id: String,
    pub boot_identity_hash: String,
    pub boot_epoch: u64,
    pub lifecycle_generation: u64,
    pub control_channel_identity: String,
}

/// Typed evidence for the caller that a start failure crossed process
/// creation and RyeOS could not prove cleanup. The caller must preserve the
/// durable worker/profile fence instead of terminalizing the session.
#[derive(Debug)]
pub struct ExclusiveWorkerCleanupUnproved;

impl std::fmt::Display for ExclusiveWorkerCleanupUnproved {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("exclusive worker cleanup is unproved")
    }
}

impl std::error::Error for ExclusiveWorkerCleanupUnproved {}

struct HeldPersistentSession {
    process: ryeos_app::thread_lifecycle::SpawnedPersistentSessionAwaitingAttachment,
    socket: std::os::unix::net::UnixStream,
    lifelines: Vec<Box<dyn Send + Sync>>,
}

pub(crate) struct AdmittedSessionPublications {
    publications: Vec<ryeos_state::PendingCasPublication>,
}

impl AdmittedSessionPublications {
    pub(crate) fn publish(self) -> Result<()> {
        for publication in self.publications {
            publication.publish()?;
        }
        Ok(())
    }
}

/// Remove only node-local admission output before independently admitting a
/// transferred portable program on another node. Authored/composed values and
/// the captured dependency remain exact; source closure, external realization,
/// and session capsule coordinates are recomputed from target-local authority.
pub(crate) fn reset_for_cross_site_admission(
    engine: &ryeos_engine::engine::Engine,
    principal: &EffectivePrincipal,
    prepared: &mut PreparedRuntimeLaunch,
) -> Result<()> {
    prepared.admitted_sessions.clear();
    for dependency in prepared.execution_dependencies.values_mut() {
        dependency.validate()?;
        let mut source_resolution = dependency.resolution.clone();
        clear_node_local_admission_projections(&mut source_resolution);

        let canonical =
            ryeos_engine::canonical_ref::CanonicalRef::parse(&dependency.canonical_ref)?;
        let plan_context = PlanContext {
            requested_by: principal.clone(),
            project_context: ProjectContext::None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            current_site_id: "cross-site-admission".to_owned(),
            origin_site_id: "cross-site-admission".to_owned(),
            execution_hints: ExecutionHints::default(),
            validate_only: true,
        };
        let target_resolved = engine.resolve(&plan_context, &canonical)?;
        let target_verified = engine.verify(&plan_context, target_resolved)?;
        let target_resolution =
            engine.effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
                item_ref: canonical,
                expected_kind: None,
                project_root: None,
                subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            })?;
        let source_portable =
            ryeos_engine::resolution::RetainedResolutionOutput::capture(&source_resolution);
        let target_portable =
            ryeos_engine::resolution::RetainedResolutionOutput::capture(&target_resolution);
        if serde_json::to_value(&source_portable)? != serde_json::to_value(&target_portable)? {
            bail!(
                "target dependency `{}` differs from the transferred portable program",
                dependency.canonical_ref
            );
        }

        let target_subject = super::launch_preparation::PreparedExecutionDependencySubject {
            source_path: target_verified.resolved.source_path.clone(),
            source_space: target_verified.resolved.source_space,
            source_root: target_verified.resolved.source_root.clone(),
            resolved_from: target_verified.resolved.resolved_from.clone(),
            materialized_project_root: target_verified.resolved.materialized_project_root.clone(),
            subject_resolution_authority: target_verified
                .resolved
                .subject_resolution_authority
                .clone(),
            raw_content_digest: target_verified.resolved.raw_content_digest.clone(),
            content_hash: target_verified.resolved.content_hash.clone(),
            signature_header: target_verified.resolved.signature_header.clone(),
            source_format: target_verified.resolved.source_format.clone(),
            metadata: target_verified.resolved.metadata.clone(),
            signer: target_verified.signer.clone(),
            trust_class: target_verified.trust_class,
        };
        if portable_subject_value(&dependency.subject)? != portable_subject_value(&target_subject)?
        {
            bail!(
                "target dependency `{}` verification authority differs from the transferred program",
                dependency.canonical_ref
            );
        }
        dependency.resolution = target_resolution;
        dependency.subject = target_subject;
        dependency.validate()?;
    }
    for dependency in prepared.content_dependencies.values_mut() {
        dependency.validate()?;
        let mut source_resolution = dependency.resolution.restore();
        clear_node_local_admission_projections(&mut source_resolution);
        let canonical =
            ryeos_engine::canonical_ref::CanonicalRef::parse(&dependency.canonical_ref)?;
        let target_resolution =
            engine.effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
                item_ref: canonical,
                expected_kind: None,
                project_root: None,
                subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            })?;
        let source_portable =
            ryeos_engine::resolution::RetainedResolutionOutput::capture(&source_resolution);
        let target_portable =
            ryeos_engine::resolution::RetainedResolutionOutput::capture(&target_resolution);
        if serde_json::to_value(&source_portable)? != serde_json::to_value(&target_portable)? {
            bail!(
                "target content dependency `{}` differs from the transferred portable program",
                dependency.canonical_ref
            );
        }
        dependency.resolution = target_portable;
        dependency.validate()?;
    }
    Ok(())
}

fn clear_node_local_admission_projections(
    resolution: &mut ryeos_engine::resolution::ResolutionOutput,
) {
    resolution
        .composed
        .derived
        .remove(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY);
    resolution
        .composed
        .derived
        .remove(ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY);
}

fn portable_subject_value(
    subject: &super::launch_preparation::PreparedExecutionDependencySubject,
) -> Result<Value> {
    let mut value = serde_json::to_value(subject)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("prepared dependency subject must be an object"))?;
    object.remove("source_path");
    object.remove("materialized_project_root");
    Ok(value)
}

/// Admit fresh session dependencies or verify recovered capsule references.
/// No mutable item/config lookup occurs on recovery.
pub(crate) fn admit_or_verify_prepared_sessions(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    prepared: &mut PreparedRuntimeLaunch,
    recovered: bool,
) -> Result<AdmittedSessionPublications> {
    let mut expected_names = BTreeSet::new();
    let (content_by_target, search_by_target, mut publications) =
        admit_or_verify_content_dependencies(state, prepared, recovered)?;
    let (dependencies, admitted_sessions) = (
        &mut prepared.execution_dependencies,
        &mut prepared.admitted_sessions,
    );
    for (name, dependency) in dependencies {
        dependency
            .validate()
            .with_context(|| format!("validate persistent-session dependency `{name}`"))?;
        if recovered {
            let Some(hash) = admitted_sessions.get(name) else {
                continue;
            };
            expected_names.insert(name.clone());
            verify_session_capsule(
                state,
                engine,
                dependency,
                hash,
                search_by_target.get(name).map(Vec::as_slice).unwrap_or(&[]),
            )
            .with_context(|| format!("verify recovered session dependency `{name}`"))?;
        } else {
            let Some((declaration, protocol)) = session_contract(engine, dependency)? else {
                continue;
            };
            expected_names.insert(name.clone());
            if admitted_sessions.contains_key(name) {
                bail!("fresh session dependency `{name}` already carries a capsule hash");
            }
            let (hash, admitted_publications) = admit_session_capsule(
                state,
                engine,
                dependency,
                &declaration,
                &protocol,
                content_by_target.get(name),
                search_by_target.get(name).map(Vec::as_slice).unwrap_or(&[]),
            )
            .inspect_err(|error| {
                tracing::warn!(
                    dependency = %name,
                    error = %error,
                    "persistent-session dependency admission failed"
                );
            })
            .with_context(|| format!("admit persistent-session dependency `{name}`"))?;
            admitted_sessions.insert(name.clone(), hash);
            publications.extend(admitted_publications);
        }
    }
    let actual_names = admitted_sessions.keys().cloned().collect::<BTreeSet<_>>();
    if actual_names != expected_names {
        bail!(
            "admitted persistent-session names contradict execution dependencies: expected={expected_names:?}, actual={actual_names:?}"
        );
    }
    Ok(AdmittedSessionPublications { publications })
}

type TargetContentSets =
    BTreeMap<String, ryeos_engine::external_realization::RealizedExternalContentSet>;
type TargetExecutableSearch = BTreeMap<String, Vec<ExecutableSearchPathEntry>>;

fn admit_or_verify_content_dependencies(
    state: &AppState,
    prepared: &mut PreparedRuntimeLaunch,
    recovered: bool,
) -> Result<(
    TargetContentSets,
    TargetExecutableSearch,
    Vec<ryeos_state::PendingCasPublication>,
)> {
    let mut entries_by_target: BTreeMap<
        String,
        Vec<ryeos_engine::external_realization::RealizedExternalContent>,
    > = BTreeMap::new();
    let mut search_by_target: TargetExecutableSearch = BTreeMap::new();
    let mut publications = Vec::new();
    for (name, dependency) in &mut prepared.content_dependencies {
        dependency
            .validate()
            .with_context(|| format!("validate content dependency `{name}`"))?;
        let mut resolution = dependency.resolution.restore();
        let realized = if recovered {
            ryeos_app::external_content_admission::recover_external_realizations(
                state,
                &resolution,
            )?
            .ok_or_else(|| anyhow!("recovered content dependency `{name}` has no realization"))?;
            realization_set(&resolution)?
        } else {
            let mut publication = None;
            ryeos_app::external_content_admission::admit_portable_content_dependency_in_publication(
                state,
                &mut resolution,
                &dependency.external_content_policy,
                None,
                &mut publication,
            )?;
            dependency.resolution =
                ryeos_engine::resolution::RetainedResolutionOutput::capture(&resolution);
            if let Some(publication) = publication {
                publications.push(publication);
            }
            realization_set(&resolution)?
        };
        validate_dependency_search(name, dependency, &realized)?;
        for target in &dependency.targets {
            entries_by_target
                .entry(target.clone())
                .or_default()
                .extend(realized.iter().cloned());
            search_by_target.entry(target.clone()).or_default().extend(
                dependency
                    .executable_search
                    .iter()
                    .map(|entry| ExecutableSearchPathEntry {
                        realization_id: entry.realization_id.clone(),
                        relative_directory: entry.relative_directory.clone(),
                    }),
            );
        }
    }
    let content_by_target = entries_by_target
        .into_iter()
        .map(|(target, entries)| {
            Ok((
                target,
                ryeos_engine::external_realization::RealizedExternalContentSet::new(entries)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    for search in search_by_target.values() {
        if search.len() > ryeos_state::objects::MAX_EXECUTABLE_SEARCH_PATH_ENTRIES {
            bail!("combined executable search exceeds the session capsule bound");
        }
        let mut seen = BTreeSet::new();
        if search.iter().any(|entry| {
            !seen.insert((
                entry.realization_id.as_str(),
                entry.relative_directory.as_str(),
            ))
        }) {
            bail!("combined executable search contains duplicate entries");
        }
    }
    Ok((content_by_target, search_by_target, publications))
}

fn realization_set(
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> Result<ryeos_engine::external_realization::RealizedExternalContentSet> {
    let value = resolution
        .composed
        .derived
        .get(ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY)
        .ok_or_else(|| anyhow!("content dependency has no retained external realization"))?;
    Ok(ryeos_engine::external_realization::RealizedExternalContentSet::from_value(value)?)
}

fn validate_dependency_search(
    name: &str,
    dependency: &PreparedContentDependency,
    realized: &ryeos_engine::external_realization::RealizedExternalContentSet,
) -> Result<()> {
    for search in &dependency.executable_search {
        let entry = realized
            .iter()
            .find(|entry| entry.id == search.realization_id)
            .ok_or_else(|| {
                anyhow!(
                    "content dependency `{name}` executable search names absent realization `{}`",
                    search.realization_id
                )
            })?;
        if entry.kind != ryeos_state::objects::ExternalContentKind::Tree {
            bail!(
                "content dependency `{name}` executable search names non-tree realization `{}`",
                search.realization_id
            );
        }
    }
    Ok(())
}

fn session_contract(
    engine: &ryeos_engine::engine::Engine,
    dependency: &PreparedExecutionDependency,
) -> Result<Option<(PersistentSessionDecl, VerifiedProtocol)>> {
    let kind = &dependency.captured_verified_subject()?.resolved.kind;
    let execution = engine
        .kinds
        .get(kind)
        .and_then(|schema| schema.execution.as_ref())
        .ok_or_else(|| anyhow!("execution dependency kind `{kind}` is not executable"))?;
    let Some(mut declaration) = execution.persistent_session.clone() else {
        return Ok(None);
    };
    apply_resource_overrides(&mut declaration, &dependency.resolution.composed.composed)?;
    let TerminatorDecl::Subprocess {
        protocol: protocol_selection,
    } = execution
        .terminator
        .as_ref()
        .ok_or_else(|| anyhow!("persistent-session kind `{kind}` has no terminator"))?
    else {
        bail!("persistent-session kind `{kind}` is not subprocess-terminated");
    };
    let protocol_ref = protocol_selection
        .resolve(&dependency.resolution.composed.composed)
        .map_err(|reason| anyhow!("persistent-session kind `{kind}`: {reason}"))?;
    let protocol = engine
        .protocols
        .get(&protocol_ref)
        .cloned()
        .ok_or_else(|| anyhow!("persistent-session protocol `{protocol_ref}` is not installed"))?;
    validate_persistent_session_protocol(&protocol.descriptor)
        .map_err(|error| anyhow!("persistent-session protocol `{protocol_ref}`: {error}"))?;
    validate_session_target(
        &dependency.resolution.composed.composed,
        &declaration.target_path,
    )?;
    Ok(Some((declaration, protocol)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistentSessionResourceOverrides {
    real_uid_process_limit: Option<u64>,
}

fn apply_resource_overrides(
    declaration: &mut PersistentSessionDecl,
    composed: &Value,
) -> Result<()> {
    let Some(path) = declaration.resource_overrides_path.as_ref() else {
        return Ok(());
    };
    let mut value = composed;
    for segment in path {
        let Some(next) = value.get(segment) else {
            return Ok(());
        };
        value = next;
    }
    let overrides: PersistentSessionResourceOverrides = serde_json::from_value(value.clone())
        .context("decode signed persistent-session resource overrides")?;
    if let Some(limit) = overrides.real_uid_process_limit {
        if limit == 0 || limit > declaration.max_real_uid_process_limit {
            bail!(
                "persistent-session real-UID process limit {limit} exceeds its signed kind ceiling {}",
                declaration.max_real_uid_process_limit
            );
        }
        declaration.real_uid_process_limit = limit;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistentSessionTarget {
    os: String,
    arch: String,
}

fn validate_session_target(composed: &Value, path: &[String]) -> Result<()> {
    let mut value = composed;
    for segment in path {
        value = value.get(segment).ok_or_else(|| {
            anyhow!(
                "persistent-session subject has no target constraint at `{}`",
                path.join(".")
            )
        })?;
    }
    let target: PersistentSessionTarget = serde_json::from_value(value.clone())
        .context("decode persistent-session target constraint")?;
    if target.os != std::env::consts::OS || target.arch != std::env::consts::ARCH {
        bail!(
            "persistent-session target {}-{} does not admit this {}-{} node",
            target.arch,
            target.os,
            std::env::consts::ARCH,
            std::env::consts::OS
        );
    }
    Ok(())
}

fn admit_session_capsule(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    dependency: &mut PreparedExecutionDependency,
    declaration: &PersistentSessionDecl,
    protocol: &VerifiedProtocol,
    inherited_content: Option<&ryeos_engine::external_realization::RealizedExternalContentSet>,
    executable_search: &[ExecutableSearchPathEntry],
) -> Result<(String, Vec<ryeos_state::PendingCasPublication>)> {
    let roots = engine.resolution_roots(None);
    let mut resolution = dependency.resolution.clone();
    let mut publication = None;
    let captured_source = ryeos_app::source_closure_admission::admit_source_closure_in_publication(
        state,
        engine,
        &dependency.captured_verified_subject()?.resolved.kind,
        &mut resolution,
        &roots,
        None,
        None,
        &mut publication,
        None,
    )?;
    let captured_external =
        ryeos_app::external_content_admission::admit_external_realizations_in_publication(
            state,
            engine,
            &dependency.captured_verified_subject()?.resolved.kind,
            &mut resolution,
            &roots,
            inherited_content,
            &mut publication,
        )?;
    let validation = engine.effective_validators.validate(
        &dependency.captured_verified_subject()?.resolved.kind,
        &resolution,
    )?;
    let candidate =
        ryeos_engine::effective_program::lock_validated_effective_program(resolution, validation)?;
    let proof = ryeos_engine::effective_program::prove_finalization_authority(
        &candidate,
        &[],
        &roots,
        None,
        captured_external
            .as_ref()
            .map(|captured| captured.finalization_evidence()),
        captured_source
            .as_ref()
            .map(|captured| captured.finalization_evidence()),
    )?;
    let finalized = ryeos_engine::effective_program::finalize_effective_program(candidate, proof)?;
    // The outer runtime capsule must retain the exact augmented dependency the
    // session capsule executes, including its admitted external-realization
    // projection. Keeping only the pre-admission resolution would make outer
    // recovery compare two different programs.
    dependency.resolution = finalized.resolution().clone();
    let exact_program = PersistentSessionExactProgram {
        effective_definition_digest: finalized.effective_definition_digest().as_str().to_owned(),
        resolution_output: ryeos_engine::resolution::RetainedResolutionOutput::capture(
            finalized.resolution(),
        ),
    };
    let exact_program_value = serde_json::to_value(&exact_program)?;
    let exact_program_hash = canonical_hash(&exact_program_value)?;
    let workspace = logical_admission_workspace();
    let lifecycle = lifecycle_contract(declaration)?;
    let wire = wire_contract(protocol)?;
    let verified = dependency.captured_verified_subject()?;
    let mut request = direct_request(state, dependency, &verified, String::new())?;
    let mut plan = prepare_captured_item_plan(
        engine,
        &request,
        &verified,
        &dependency.resolution.root.raw_content,
        &state.isolation,
        None,
    )?;
    let executor_ref = plan
        .execution_plan()
        .executor_chain
        .get(1)
        .cloned()
        .ok_or_else(|| anyhow!("persistent-session plan has no executor-chain hop"))?;
    request.executor_ref = executor_ref.clone();
    plan.bind_persistent_session_workspace(&workspace)?;
    let artifact_identity = plan.admitted_artifact_identity(&request, protocol)?;

    let authority = publication
        .as_ref()
        .map(|publication| publication.authority().try_clone())
        .transpose()?
        .unwrap_or(state.state_store.pinned_state_authority()?);
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let structured_session_profile = if wire.wire_protocol == "ryeos.structured-session" {
        let captured = captured_source
            .as_ref()
            .ok_or_else(|| anyhow!("structured-session worker has no admitted source closure"))?;
        let entry = match &captured.binding().logical_binding {
            ryeos_state::objects::SourceLogicalBinding::Worker { entry, .. } => entry,
            _ => bail!("structured-session worker has a non-worker source binding"),
        };
        let mut source_files = BTreeMap::new();
        for file in &captured.manifest().entries {
            let bytes = cas
                .get_blob(&file.blob_hash)?
                .ok_or_else(|| anyhow!("captured structured-session source blob is absent"))?;
            source_files.insert(file.path.clone(), bytes);
        }
        let profile_bytes = source_files.get(entry).ok_or_else(|| {
            anyhow!("structured-session entry is absent from its captured source closure")
        })?;
        Some(ryeos_engine::structured_session_profile::compile(
            profile_bytes,
            &source_files,
        )?)
    } else {
        None
    };
    let execution_closure = {
        let _permit = state
            .write_barrier
            .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
            .map_err(|error| anyhow!("cannot acquire persistent-session write permit: {error}"))?;
        plan.admit_execution_closure(
            &cas,
            &state.isolation,
            protocol,
            &engine.node_trust_store,
            Some(&workspace),
        )?
    };
    authority.ensure_guard(&guard)?;
    let session_authority = PersistentSessionAuthority {
        exact_program_hash: exact_program_hash.clone(),
        lifecycle: lifecycle.clone(),
        wire: wire.clone(),
        artifact_identity: artifact_identity.clone(),
        execution_closure: execution_closure.clone(),
        runtime_ref: plan.runtime_ref()?.to_owned(),
        executor_ref: executor_ref.clone(),
    };
    let realization = super::execution_realization::admit_persistent_session(
        state,
        &session_authority,
        finalized.resolution(),
        finalized.effective_definition_digest().as_str(),
        &protocol.canonical_ref,
        &protocol.raw_content_digest,
        publication.as_mut(),
    )?;
    if publication.is_none() {
        publication = realization.publication;
    }
    let mut publication = publication.ok_or_else(|| {
        anyhow!("persistent-session admission produced no durable CAS publication")
    })?;
    let capsule = AdmittedPersistentSessionCapsule {
        schema: PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
        kind: PERSISTENT_SESSION_CAPSULE_KIND.to_owned(),
        exact_program: exact_program_value,
        exact_program_hash,
        lifecycle,
        wire,
        artifact_identity,
        execution_closure,
        execution_realization_hash: realization.hash,
        source_binding_hash: captured_source
            .as_ref()
            .map(|captured| captured.binding().digest())
            .transpose()?,
        structured_session_profile,
        executable_search: executable_search.to_vec(),
        runtime_ref: session_authority.runtime_ref,
        executor_ref,
    };
    let expected_hash = capsule.content_hash()?;
    let guard = publication.authority().acquire_shared_guard()?;
    publication.authority().ensure_guard(&guard)?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow!("cannot acquire session-capsule write permit: {error}"))?;
    let cas = publication.authority().cas_store()?;
    let stored =
        publication
            .staged_roots_mut()
            .store_object_admitted(&guard, &cas, &capsule.to_value()?)?;
    if stored != expected_hash {
        bail!(
            "persistent-session capsule hash mismatch: expected {expected_hash}, stored {stored}"
        );
    }
    Ok((stored, vec![publication]))
}

fn verify_session_capsule(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    dependency: &PreparedExecutionDependency,
    capsule_hash: &str,
    executable_search: &[ExecutableSearchPathEntry],
) -> Result<AdmittedPersistentSessionCapsule> {
    let capsule = load_capsule(state, capsule_hash)?;
    let exact: PersistentSessionExactProgram =
        serde_json::from_value(capsule.exact_program.clone())?;
    let retained_dependency =
        ryeos_engine::resolution::RetainedResolutionOutput::capture(&dependency.resolution);
    if exact.resolution_output.root_ref() != dependency.canonical_ref
        || canonical_hash(&serde_json::to_value(&exact.resolution_output)?)?
            != canonical_hash(&serde_json::to_value(&retained_dependency)?)?
    {
        bail!("persistent-session capsule contradicts its captured dependency");
    }
    if capsule.executable_search != executable_search {
        bail!("persistent-session capsule contradicts its executable-search dependency");
    }
    let observed_digest = exact.resolution_output.effective_definition_digest()?;
    if observed_digest.as_str() != exact.effective_definition_digest {
        bail!("persistent-session exact effective-definition digest changed");
    }
    validate_capsule_current_trust(engine, &capsule)?;
    let (protocol_ref, protocol_digest) = capsule_protocol_identity(&capsule)?;
    let resolution = exact.resolution_output.restore();
    ryeos_app::source_closure_admission::recover_source_closure(state, &state.engine, &resolution)?;
    ryeos_app::external_content_admission::recover_external_realizations(state, &resolution)?;
    super::execution_realization::verify_persistent_session(
        state,
        &capsule,
        &resolution,
        &exact.effective_definition_digest,
        protocol_ref,
        protocol_digest,
    )?;
    Ok(capsule)
}

pub fn inspect_capsule(
    state: &AppState,
    capsule_hash: &str,
) -> Result<AdmittedPersistentSessionIdentity> {
    let capsule = load_capsule(state, capsule_hash)?;
    validate_capsule_current_trust(&state.engine, &capsule)?;
    let exact: PersistentSessionExactProgram =
        serde_json::from_value(capsule.exact_program.clone())?;
    let current_digest = exact.resolution_output.effective_definition_digest()?;
    if current_digest.as_str() != exact.effective_definition_digest {
        bail!("persistent-session exact program digest does not reproduce");
    }
    let resolution = exact.resolution_output.restore();
    ryeos_app::source_closure_admission::recover_source_closure(state, &state.engine, &resolution)?;
    super::execution_realization::verify_persistent_session(
        state,
        &capsule,
        &resolution,
        &exact.effective_definition_digest,
        capsule_protocol_identity(&capsule)?.0,
        capsule_protocol_identity(&capsule)?.1,
    )?;
    Ok(AdmittedPersistentSessionIdentity {
        canonical_ref: exact.resolution_output.root_ref().to_owned(),
        effective_definition_digest: exact.effective_definition_digest,
        capsule_hash: capsule_hash.to_owned(),
        execution_realization_hash: capsule.execution_realization_hash,
    })
}

pub fn execute_capsule<C, D>(
    state: &AppState,
    capsule_hash: &str,
    request_body: Value,
    cancelled: C,
    on_delta: D,
) -> Result<Value>
where
    C: Fn() -> bool,
    D: FnMut(Value) -> Result<()>,
{
    let capsule = load_capsule(state, capsule_hash)?;
    validate_capsule_current_trust(&state.engine, &capsule)?;
    let protocol_ref = capsule_protocol_identity(&capsule)?.0;
    if installed_session_protocol(state, protocol_ref)?.process_mode
        != PersistentSessionProcessMode::PooledRequests
    {
        bail!(
            "exclusive persistent-session protocol `{protocol_ref}` cannot enter the request pool"
        );
    }
    let exact: PersistentSessionExactProgram =
        serde_json::from_value(capsule.exact_program.clone())?;
    let current_digest = exact.resolution_output.effective_definition_digest()?;
    if current_digest.as_str() != exact.effective_definition_digest {
        bail!("persistent-session exact program digest does not reproduce");
    }
    let resolution = exact.resolution_output.restore();
    super::execution_realization::verify_persistent_session(
        state,
        &capsule,
        &resolution,
        &exact.effective_definition_digest,
        capsule_protocol_identity(&capsule)?.0,
        capsule_protocol_identity(&capsule)?.1,
    )?;
    let pool_key = canonical_hash(&json!({
        "capsule_hash": capsule_hash,
        "execution_realization_hash": capsule.execution_realization_hash,
        "authority": capsule.authority().digest()?,
    }))?;
    let lifecycle = capsule.lifecycle.clone();
    let wire = capsule.wire.clone();
    state.persistent_sessions.execute(
        &pool_key,
        &lifecycle,
        &wire,
        request_body,
        || start_capsule_process(state, capsule_hash, &capsule, &exact),
        cancelled,
        on_delta,
    )
}

fn installed_session_protocol<'a>(
    state: &'a AppState,
    protocol_ref: &str,
) -> Result<&'a ryeos_engine::protocols::descriptor::PersistentSessionProtocol> {
    let protocol =
        state.engine.protocols.get(protocol_ref).ok_or_else(|| {
            anyhow!("persistent-session protocol `{protocol_ref}` is not installed")
        })?;
    validate_persistent_session_protocol(&protocol.descriptor)
        .map_err(|error| anyhow!("persistent-session protocol `{protocol_ref}`: {error}"))
}

fn start_capsule_process(
    state: &AppState,
    capsule_hash: &str,
    capsule: &AdmittedPersistentSessionCapsule,
    exact: &PersistentSessionExactProgram,
) -> Result<StartedPersistentSession> {
    let protocol_ref = capsule_protocol_identity(capsule)?.0;
    let session_protocol = installed_session_protocol(state, protocol_ref)?;
    if session_protocol.process_mode != PersistentSessionProcessMode::PooledRequests {
        bail!("exclusive persistent-session protocol cannot use the pooled launcher");
    }
    let workspace_name = format!(
        "persistent-session-{}-{:08x}",
        &capsule_hash[..16],
        rand::random::<u32>()
    );
    let (workspace, workspace_lifeline) = ryeos_app::temp_dir_guard::create_projectless_workspace(
        &state.config.runtime_root().cache(),
        &workspace_name,
    )?;
    let mut held = spawn_capsule_process_held(
        state,
        capsule_hash,
        capsule,
        exact,
        &workspace,
        session_protocol,
        None,
        &BTreeMap::new(),
    )?;
    held.lifelines.push(Box::new(workspace_lifeline));
    // The fixed pool becomes the process owner as soon as this constructor
    // succeeds. It has no durable cross-restart attachment: restart recovery
    // deliberately reconstructs an equivalent pooled process from the
    // immutable capsule.
    let running = held.process.release_after_attachment()?;
    Ok(StartedPersistentSession {
        running,
        socket: held.socket,
        lifelines: held.lifelines,
        expected_boot_identity: None,
    })
}

/// Create a daemon-owned execution view under RyeOS's code-enforced
/// `.ai/cache` snapshot exclusion. Disabled-isolation launches can materialize
/// immutable runtime inputs here without adding them to the project candidate.
fn create_node_owned_runtime_view(workspace: &Path) -> Result<lillux::PinnedDirectory> {
    let mut current = lillux::PinnedDirectory::open(workspace)?
        .ok_or_else(|| anyhow!("persistent-session workspace is missing"))?;
    for component in [".ai", "cache", "ryeos-runtime"] {
        current = current
            .open_or_create_child(std::ffi::OsStr::new(component), 0o700)
            .with_context(|| format!("open node-owned runtime-view component `{component}`"))?;
    }
    current.tighten_owner_private_directory()?;
    Ok(current)
}

fn prepare_structured_session_baseline(
    profile: &ryeos_state::objects::AdmittedStructuredSessionProfile,
    source_entry: &Path,
    state_root: &Path,
    enforced: bool,
) -> Result<Option<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>> {
    let source_parent = source_entry
        .parent()
        .ok_or_else(|| anyhow!("structured-session entry has no source parent"))?;
    let source_directory = lillux::PinnedDirectory::open(source_parent)?
        .ok_or_else(|| anyhow!("structured-session source parent is missing"))?;
    let source_file = source_directory
        .open_pinned_regular(std::ffi::OsStr::new(&profile.baseline_source), false)?
        .ok_or_else(|| anyhow!("admitted structured-session baseline is missing"))?;
    let bytes = source_file.read_bounded(64 * 1024)?;
    if bytes.is_empty() {
        bail!("admitted structured-session baseline is empty");
    }

    let state_directory = lillux::PinnedDirectory::open(state_root)?
        .ok_or_else(|| anyhow!("structured-session state root is missing"))?;
    let destination_name = std::ffi::OsStr::new(&profile.baseline_destination);
    let incumbent = state_directory
        .open_pinned_regular(destination_name, false)
        .context("open workload compatibility seed through Lillux")?;
    let current_matches = incumbent
        .as_ref()
        .map(|entry| {
            Ok(entry.permission_mode()? == 0o400 && entry.read_bounded(64 * 1024)? == bytes)
        })
        .transpose()?
        .unwrap_or(false);
    if !current_matches {
        state_directory
            .atomic_write_pinned_if_same(destination_name, incumbent.as_ref(), &bytes, 0o400)
            .context("publish workload compatibility seed through Lillux")?;
    }
    if !enforced {
        return Ok(None);
    }
    let source_path = source_file.path().to_path_buf();
    let destination = state_root.join(&profile.baseline_destination);
    let source_descriptor = source_file.try_clone_descriptor()?;
    Ok(Some(
        ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new_state_overlay(
            source_path,
            destination,
            source_descriptor,
        ),
    ))
}

fn spawn_capsule_process_held(
    state: &AppState,
    capsule_hash: &str,
    capsule: &AdmittedPersistentSessionCapsule,
    exact: &PersistentSessionExactProgram,
    workspace: &Path,
    session_protocol: &ryeos_engine::protocols::descriptor::PersistentSessionProtocol,
    state_root: Option<&Path>,
    runtime_environment: &BTreeMap<String, String>,
) -> Result<HeldPersistentSession> {
    let resolution = exact.resolution_output.restore();
    super::source_closure::validate_external_mount_separation(state, &resolution)?;
    let private_budget = (!state.isolation.is_enforced())
        .then(super::external_content::private_materialization_budget)
        .transpose()?;
    let private_runtime_view;
    let realization_workspace = if state.isolation.is_enforced()
        || session_protocol.process_mode == PersistentSessionProcessMode::PooledRequests
    {
        workspace
    } else {
        private_runtime_view = create_node_owned_runtime_view(workspace)?;
        private_runtime_view.path()
    };
    let bound = if state.isolation.is_enforced() {
        super::external_content::bind_external_realizations(state, &resolution, &workspace)?
    } else {
        super::external_content::bind_external_realizations_in_private_workspace_with_budget(
            state,
            &resolution,
            realization_workspace,
            private_budget
                .as_ref()
                .expect("disabled isolation has a private copy budget"),
        )?
    };
    let (mounts, external_env, leases) = match bound {
        Some(bound) => {
            let (mounts, env, leases) = bound.into_spawn_parts();
            (mounts, Some(env), leases)
        }
        None => (Vec::new(), None, Vec::new()),
    };
    let source = if state.isolation.is_enforced() {
        super::source_closure::bind_source(state, &resolution, &workspace)?
    } else {
        super::source_closure::bind_source_in_private_workspace_with_budget(
            state,
            &resolution,
            realization_workspace,
            private_budget
                .as_ref()
                .expect("disabled isolation has a private copy budget"),
        )?
    };
    let mut mounts = mounts;
    let (source_env, source_entry) = match source.as_ref() {
        Some(source) => {
            mounts.extend_from_slice(source.mounts());
            (
                Some(source.sealed_identity_env()),
                Some(source.entry_path()),
            )
        }
        None => (None, None),
    };
    if let Some(profile) = capsule.structured_session_profile.as_ref() {
        let source_entry = source_entry
            .ok_or_else(|| anyhow!("structured-session capsule has no bound source entry"))?;
        let state_root = state_root
            .ok_or_else(|| anyhow!("structured-session capsule has no exact state root"))?;
        // The admission-compiled immutable argv is the structured workload's
        // configuration authority. An enforced generic isolation backend adds
        // a read-only overlay for the compatibility baseline, but the
        // structured-session substrate does not require one.
        if let Some(overlay) = prepare_structured_session_baseline(
            profile,
            source_entry,
            state_root,
            state.isolation.is_enforced(),
        )? {
            mounts.push(overlay);
        }
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let mut plan =
        ryeos_app::thread_lifecycle::PreparedItemPlan::recover_from_persistent_session_capsule(
            capsule,
            &cas,
            &state.isolation,
            &workspace,
        )?;
    let (daemon_socket, worker_socket) = std::os::unix::net::UnixStream::pair()
        .context("create daemon-owned persistent-session channel")?;
    let target_channel = ryeos_engine::isolation::IsolationTargetChannelAuthority::new(
        worker_socket,
        capsule.wire.channel_env.clone(),
    )?;
    let executable_search_env = (!capsule.executable_search.is_empty())
        .then(|| {
            lillux::canonical_json(&serde_json::to_value(&capsule.executable_search)?)
                .map_err(anyhow::Error::from)
        })
        .transpose()?;
    plan.bind_persistent_session_spawn_environment(
        external_env.as_deref(),
        external_env.as_ref().map(|_| realization_workspace),
        source_env,
        source_entry,
        executable_search_env.as_deref(),
    )?;
    let mut runtime_env_allowlist = session_protocol.runtime_env_allowlist.clone();
    if let Some(name) = session_protocol.readiness_identity_env.as_ref() {
        runtime_env_allowlist.push(name.clone());
    }
    plan.bind_persistent_session_runtime_environment(runtime_environment, &runtime_env_allowlist)?;
    // `realization_workspace` is only the daemon-owned location for sealed
    // runtime inputs when outer isolation is disabled.  The process authority
    // remains the canonical runtime-workspace `project` child; substituting
    // the nested realization view here would change the admitted workspace
    // identity and fail the runtime-workspace layout check.
    let process = plan.spawn_persistent_session_held(
        state,
        workspace,
        mounts,
        target_channel,
        &capsule.lifecycle,
        session_protocol.workspace_authority,
        session_protocol.network_authority,
        state_root,
        &format!("session-{}", &capsule_hash[..24]),
    )?;
    let mut lifelines: Vec<Box<dyn Send + Sync>> = Vec::with_capacity(leases.len());
    lifelines.extend(
        leases
            .into_iter()
            .map(|lease| Box::new(lease) as Box<dyn Send + Sync>),
    );
    if let Some(source) = source {
        lifelines.push(Box::new(source));
    }
    Ok(HeldPersistentSession {
        process,
        socket: daemon_socket,
        lifelines,
    })
}

/// Start one session-owned process from an already-admitted capsule and
/// already-ready durable workspace. The exact held identity is committed
/// before Lillux authorizes child execution.
pub fn start_exclusive_capsule(
    state: &AppState,
    capsule_hash: &str,
    workspace: &Path,
    state_root: Option<&Path>,
    runtime_environment: &BTreeMap<String, String>,
    identity: &ExclusivePersistentSessionIdentity,
) -> Result<()> {
    let capsule = load_capsule(state, capsule_hash)?;
    validate_capsule_current_trust(&state.engine, &capsule)?;
    let exact: PersistentSessionExactProgram =
        serde_json::from_value(capsule.exact_program.clone())?;
    let protocol_ref = capsule_protocol_identity(&capsule)?.0;
    let session_protocol = installed_session_protocol(state, protocol_ref)?;
    use ryeos_engine::protocols::descriptor::PersistentSessionWorkspaceAuthority;
    if session_protocol.process_mode != PersistentSessionProcessMode::ExclusiveSession
        || session_protocol.workspace_authority
            != PersistentSessionWorkspaceAuthority::RuntimeWorkspace
    {
        bail!("persistent-session protocol does not authorize an exclusive runtime workspace");
    }
    let reservation = state.persistent_sessions.reserve_exclusive(
        &identity.placement_thread_id,
        &capsule.lifecycle,
        &capsule.wire,
    )?;
    let mut runtime_environment = runtime_environment.clone();
    if let Some(profile) = capsule.structured_session_profile.as_ref()
        && runtime_environment
            .insert(
                "RYEOS_STRUCTURED_SESSION_PROFILE_HASH".to_owned(),
                profile.profile_hash.clone(),
            )
            .is_some()
    {
        bail!("structured-session profile identity collides with runtime authority");
    }
    if let Some(name) = session_protocol.readiness_identity_env.as_ref() {
        if runtime_environment
            .insert(name.clone(), identity.boot_identity_hash.clone())
            .is_some()
        {
            bail!("readiness identity environment collides with runtime authority");
        }
    } else {
        bail!("exclusive persistent-session protocol requires a readiness identity slot");
    }
    let held = spawn_capsule_process_held(
        state,
        capsule_hash,
        &capsule,
        &exact,
        workspace,
        session_protocol,
        state_root,
        &runtime_environment,
    )?;
    let now = lillux::time::timestamp_millis() as i64;
    let record = WorkerProcessRecord {
        worker_instance_id: identity.worker_instance_id.clone(),
        boot_identity_hash: identity.boot_identity_hash.clone(),
        session_capsule_hash: capsule_hash.to_owned(),
        boot_epoch: identity.boot_epoch,
        lifecycle_generation: identity.lifecycle_generation,
        process_identity: held.process.process_identity.clone(),
        control_channel_identity: identity.control_channel_identity.clone(),
        state: WorkerProcessState::Attached,
        daemon_generation_id: daemon_generation_id().to_owned(),
        placement_thread_id: identity.placement_thread_id.clone(),
        cleanup_state: "owned".to_owned(),
        created_at_ms: now,
        updated_at_ms: now,
    };
    if let Err(error) = state.state_store.attach_worker_process(&record) {
        let cleanup = held.process.abort_and_reap().err();
        return Err(match cleanup {
            Some(cleanup) => {
                let reason = format!("exclusive held-process attachment cleanup failed: {cleanup}");
                let evidence = state
                    .state_store
                    .fence_unproved_worker_start(&record, &reason);
                let error = error.context(reason);
                match evidence {
                    Ok(()) => error.context(ExclusiveWorkerCleanupUnproved),
                    Err(evidence) => error
                        .context(format!(
                            "persist exact unproved worker evidence failed: {evidence}"
                        ))
                        .context(ExclusiveWorkerCleanupUnproved),
                }
            }
            None => error,
        });
    }
    let running = match held.process.release_after_attachment() {
        Ok(running) => running,
        Err(error) => {
            let mut error = error;
            if let Err(settlement) = state.state_store.settle_worker_process(
                &identity.worker_instance_id,
                &identity.placement_thread_id,
                identity.boot_epoch,
                "unproved",
                "held process release failed",
            ) {
                error = error.context(format!(
                    "persist held-process release failure also failed: {settlement:#}"
                ));
            }
            return Err(error.context(ExclusiveWorkerCleanupUnproved));
        }
    };
    let started = StartedPersistentSession {
        running,
        socket: held.socket,
        lifelines: held.lifelines,
        expected_boot_identity: Some(identity.boot_identity_hash.clone()),
    };
    if let Err(error) = reservation.bind(started) {
        let cleanup_unproved = error
            .downcast_ref::<ryeos_app::persistent_session::PersistentSessionCleanupUnproved>()
            .is_some();
        let cleanup_state = if cleanup_unproved {
            "unproved"
        } else {
            "reaped"
        };
        let mut error = error;
        if let Err(settlement) = state.state_store.settle_worker_process(
            &identity.worker_instance_id,
            &identity.placement_thread_id,
            identity.boot_epoch,
            cleanup_state,
            "exclusive worker readiness failed",
        ) {
            error = error.context(format!(
                "persist exclusive readiness cleanup also failed: {settlement:#}"
            ));
        }
        return Err(if cleanup_unproved {
            error.context(ExclusiveWorkerCleanupUnproved)
        } else {
            error
        });
    }
    if let Err(error) = state.state_store.complete_worker_binding(
        &identity.worker_instance_id,
        &identity.placement_thread_id,
        identity.boot_epoch,
    ) {
        let mut error = error;
        let cleanup_state = match ryeos_app::dedicated_session_service::retire_worker_process(
            state,
            &identity.placement_thread_id,
            &record,
        ) {
            Ok(cleanup_state) => cleanup_state,
            Err(cleanup) => {
                error = error.context(format!(
                    "retire worker after durable readiness publication failure also failed: {cleanup:#}"
                ));
                "unproved"
            }
        };
        if let Err(settlement) = state.state_store.settle_worker_process(
            &identity.worker_instance_id,
            &identity.placement_thread_id,
            identity.boot_epoch,
            cleanup_state,
            "durable readiness publication failed",
        ) {
            error = error.context(format!(
                "persist durable readiness cleanup also failed: {settlement:#}"
            ));
        }
        return Err(if cleanup_state == "reaped" {
            error
        } else {
            error.context(ExclusiveWorkerCleanupUnproved)
        });
    }
    // Wake attachment-gated controllers only after the held process has been
    // released, the exclusive transport is bound, and the durable worker row
    // is live. The projection remains the authority; this process-local signal
    // only removes a polling loop and is safe to lose across restart.
    ryeos_app::dedicated_session_service::notify_projection_change(&identity.placement_thread_id);
    Ok(())
}

fn load_capsule(state: &AppState, hash: &str) -> Result<AdmittedPersistentSessionCapsule> {
    if !lillux::valid_hash(hash) {
        bail!("persistent-session capsule hash is not canonical");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let value = cas
        .get_object(hash)?
        .ok_or_else(|| anyhow!("persistent-session capsule {hash} is unavailable"))?;
    authority.ensure_guard(&guard)?;
    let capsule = AdmittedPersistentSessionCapsule::from_current_value(&value)?;
    if capsule.content_hash()? != hash {
        bail!("persistent-session capsule content hash changed");
    }
    Ok(capsule)
}

fn validate_capsule_current_trust(
    engine: &ryeos_engine::engine::Engine,
    capsule: &AdmittedPersistentSessionCapsule,
) -> Result<()> {
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        root_subject_signer_fingerprint,
        root_subject_source_identity,
        protocol_signer_fingerprint,
        executable_identity,
        runtime_identity,
        ..
    } = &capsule.artifact_identity
    else {
        bail!("persistent-session capsule has a non-direct artifact identity");
    };
    let mut signers = Vec::new();
    if let Some(signer) = root_subject_signer_fingerprint.as_deref() {
        signers.push(("root subject", signer));
    }
    signers.push(("protocol", protocol_signer_fingerprint.as_str()));
    signers.push((
        "runtime",
        runtime_identity.runtime_signer_fingerprint.as_str(),
    ));
    if let Some(signer) = runtime_identity
        .runtime_bundle_signer_fingerprint
        .as_deref()
    {
        signers.push(("runtime bundle", signer));
    }
    if let ryeos_state::objects::DirectRootSourceIdentity::Bundle {
        manifest_signer_fingerprint,
        ..
    } = root_subject_source_identity
    {
        signers.push(("root bundle", manifest_signer_fingerprint));
    }
    if let ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
        executor_manifest_signer_fingerprint,
        ..
    } = executable_identity
    {
        signers.push(("executable bundle", executor_manifest_signer_fingerprint));
    }
    for (label, signer) in signers {
        if !engine.node_trust_store.is_trusted(signer) {
            bail!("persistent-session {label} signer is no longer trusted: {signer}");
        }
    }
    Ok(())
}

fn capsule_protocol_identity(capsule: &AdmittedPersistentSessionCapsule) -> Result<(&str, &str)> {
    match &capsule.artifact_identity {
        ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            protocol_ref,
            protocol_content_hash,
            ..
        } => Ok((protocol_ref, protocol_content_hash)),
        _ => bail!("persistent-session capsule has a non-direct artifact identity"),
    }
}

fn direct_request(
    state: &AppState,
    dependency: &PreparedExecutionDependency,
    verified: &ryeos_engine::contracts::VerifiedItem,
    executor_ref: String,
) -> Result<ResolvedExecutionRequest> {
    // An execution dependency is an implementation selected inside a signed
    // runtime launch contract, not a child action dispatched by the outer
    // caller. `resolve_execution_dependencies` has already enforced that
    // contract's kind/space/trust ceiling and captured the exact verified
    // bundle subject. Give plan compilation only the exact execute scope for
    // that captured subject; never borrow the caller's scopes and never mint a
    // wildcard. The outer launch capsule seals both the signed runtime
    // descriptor and this prepared dependency before the session can run.
    dependency.validate()?;
    let principal =
        dependency_plan_principal(state.identity.fingerprint(), &dependency.canonical_ref)?;
    let site = state.threads.site_id().to_owned();
    Ok(ResolvedExecutionRequest {
        kind: verified.resolved.kind.clone(),
        item_ref: dependency.canonical_ref.clone(),
        executor_ref,
        launch_mode: "wait".to_owned(),
        current_site_id: site.clone(),
        origin_site_id: site.clone(),
        target_site_id: None,
        requested_by: Some(state.identity.fingerprint().to_owned()),
        usage_subject: None,
        usage_subject_asserted_by: None,
        parameters: Value::Object(Default::default()),
        ref_bindings: BTreeMap::new(),
        resolved_item: verified.resolved.clone(),
        root_raw_content_digest: dependency.subject.raw_content_digest.clone(),
        plan_context: PlanContext {
            requested_by: principal,
            project_context: ProjectContext::None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            current_site_id: site.clone(),
            origin_site_id: site,
            execution_hints: ExecutionHints::default(),
            validate_only: false,
        },
        root_admission: None,
    })
}

fn dependency_plan_principal(node_fingerprint: &str, item_ref: &str) -> Result<EffectivePrincipal> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)?;
    if canonical.suffix.is_some() || canonical.to_string() != item_ref {
        bail!("persistent-session dependency ref is not exact and unsuffixed");
    }
    let execute_cap =
        ryeos_runtime::authorizer::canonical_cap(&canonical.kind, &canonical.bare_id, "execute");
    Ok(EffectivePrincipal::Local(Principal {
        fingerprint: node_fingerprint.to_owned(),
        scopes: vec![execute_cap],
    }))
}

fn lifecycle_contract(
    declaration: &PersistentSessionDecl,
) -> Result<PersistentSessionLifecycleContract> {
    let contract = PersistentSessionLifecycleContract {
        max_processes: declaration.max_processes,
        max_inflight_per_process: declaration.max_inflight_per_process,
        max_address_space_bytes: declaration.max_address_space_bytes,
        max_cpu_seconds: declaration.max_cpu_seconds,
        real_uid_process_limit: declaration.real_uid_process_limit,
        ready_timeout_ms: declaration.ready_timeout_ms,
        request_timeout_ms: declaration.request_timeout_ms,
        idle_timeout_ms: declaration.idle_timeout_ms,
    };
    contract.validate()?;
    Ok(contract)
}

fn wire_contract(protocol: &VerifiedProtocol) -> Result<PersistentSessionWireContract> {
    let session = validate_persistent_session_protocol(&protocol.descriptor)
        .map_err(|error| anyhow!(error))?;
    let contract = PersistentSessionWireContract {
        channel_env: session.channel_env.clone(),
        wire_protocol: session.wire_protocol.clone(),
        wire_version: session.wire_version,
        max_frame_bytes: session.max_frame_bytes,
    };
    contract.validate()?;
    Ok(contract)
}

/// Canonical identity-space root for persistent-session plans. This is never
/// opened on the host. Recovery relocates it to the daemon-owned workspace in
/// the mutable spawn copy, keeping runtime-root paths out of plan hashes and
/// retained capsules.
fn logical_admission_workspace() -> PathBuf {
    PathBuf::from("/ryeos/persistent-session-workspace")
}

fn canonical_hash(value: &Value) -> Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(value)?.as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource_override_declaration() -> PersistentSessionDecl {
        PersistentSessionDecl {
            target_path: vec!["supported_target".to_owned()],
            max_processes: 1,
            max_inflight_per_process: 1,
            max_address_space_bytes: 64 * 1024 * 1024,
            max_cpu_seconds: 1,
            real_uid_process_limit: 512,
            resource_overrides_path: Some(vec!["session_resources".to_owned()]),
            max_real_uid_process_limit: 4_096,
            ready_timeout_ms: 1,
            request_timeout_ms: 1,
            idle_timeout_ms: 1,
        }
    }

    #[test]
    fn signed_worker_resource_override_is_capped_and_frozen() {
        let mut declaration = resource_override_declaration();
        apply_resource_overrides(
            &mut declaration,
            &json!({"session_resources":{"real_uid_process_limit":1024}}),
        )
        .unwrap();
        assert_eq!(declaration.real_uid_process_limit, 1_024);

        let mut absent = resource_override_declaration();
        apply_resource_overrides(&mut absent, &json!({})).unwrap();
        assert_eq!(absent.real_uid_process_limit, 512);

        let mut excessive = resource_override_declaration();
        assert!(
            apply_resource_overrides(
                &mut excessive,
                &json!({"session_resources":{"real_uid_process_limit":4097}}),
            )
            .is_err()
        );

        let mut unknown = resource_override_declaration();
        assert!(
            apply_resource_overrides(&mut unknown, &json!({"session_resources":{"unknown":1}}),)
                .is_err()
        );
    }

    fn retained_program_fixture(
        source_path: &str,
        body_digest_byte: char,
    ) -> PersistentSessionExactProgram {
        let resolution = ryeos_engine::resolution::ResolutionOutput {
            root: ryeos_engine::resolution::ResolvedAncestor {
                requested_id: "worker:fixture/session".to_owned(),
                resolved_ref: "worker:fixture/session".to_owned(),
                source_path: PathBuf::from(source_path),
                source_space: ryeos_engine::contracts::ItemSpace::Bundle,
                source_root: ryeos_engine::contracts::ItemSourceRoot::Bundle {
                    name: "fixture".to_owned(),
                },
                trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
                signer_fingerprint: Some("f".repeat(64)),
                alias_resolution: None,
                added_by: ryeos_engine::resolution::ResolutionStepName::PipelineInit,
                raw_content: format!("body-{body_digest_byte}"),
                source_content_digest: body_digest_byte.to_string().repeat(64),
                raw_content_digest: body_digest_byte.to_string().repeat(64),
            },
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: BTreeMap::new().into_iter().collect(),
            effective_trust_class: ryeos_engine::resolution::TrustClass::TrustedBundle,
            composed: ryeos_engine::resolution::KindComposedView::identity(json!({
                "supported_target": {
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH
                }
            })),
        };
        let digest = resolution.effective_definition_digest().unwrap();
        PersistentSessionExactProgram {
            effective_definition_digest: digest.as_str().to_owned(),
            resolution_output: ryeos_engine::resolution::RetainedResolutionOutput::capture(
                &resolution,
            ),
        }
    }

    fn capsule_fixture(
        exact_program: &PersistentSessionExactProgram,
    ) -> AdmittedPersistentSessionCapsule {
        use ryeos_state::objects::{
            AdmittedDirectCommandClosure, AdmittedExecutionClosure, AdmittedLaunchArtifactIdentity,
            DirectExecutableIdentity, DirectRootSourceIdentity, DirectRuntimeIdentity,
            DirectRuntimeSourceSpace,
        };

        let exact_program = serde_json::to_value(exact_program).unwrap();
        let exact_program_hash = canonical_hash(&exact_program).unwrap();
        let executable_blob_hash = "e".repeat(64);
        let execution_path = ryeos_state::objects::admitted_direct_command_execution_path(
            &executable_blob_hash,
            std::path::Path::new("ryeos-session-exec"),
        )
        .unwrap();
        AdmittedPersistentSessionCapsule {
            schema: PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
            kind: PERSISTENT_SESSION_CAPSULE_KIND.to_owned(),
            exact_program,
            exact_program_hash,
            lifecycle: PersistentSessionLifecycleContract {
                max_processes: 1,
                max_inflight_per_process: 1,
                max_address_space_bytes: 64 * 1024 * 1024,
                max_cpu_seconds: 1,
                real_uid_process_limit: 1,
                ready_timeout_ms: 1,
                request_timeout_ms: 1,
                idle_timeout_ms: 1,
            },
            wire: PersistentSessionWireContract {
                channel_env: "RYEOS_SESSION_FD".to_owned(),
                wire_protocol: "fixture.session".to_owned(),
                wire_version: 1,
                max_frame_bytes: 1024,
            },
            artifact_identity: AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                executor_ref: "native:fixture".to_owned(),
                root_subject_source_content_digest: "a".repeat(64),
                root_subject_signer_fingerprint: Some("f".repeat(64)),
                root_subject_source_identity: DirectRootSourceIdentity::Bundle {
                    manifest_hash: "b".repeat(64),
                    manifest_signer_fingerprint: "f".repeat(64),
                },
                protocol_ref: "protocol:fixture/session".to_owned(),
                protocol_content_hash: "c".repeat(64),
                protocol_signer_fingerprint: "f".repeat(64),
                execution_plan_hash: "d".repeat(64),
                executable_identity: DirectExecutableIdentity::CapturedContent {
                    content_hash: executable_blob_hash.clone(),
                },
                runtime_identity: DirectRuntimeIdentity {
                    runtime_ref: "runtime:fixture/session".to_owned(),
                    runtime_source_space: DirectRuntimeSourceSpace::Bundle,
                    runtime_content_hash: "6".repeat(64),
                    runtime_signer_fingerprint: "f".repeat(64),
                    runtime_bundle_manifest_hash: Some("7".repeat(64)),
                    runtime_bundle_signer_fingerprint: Some("f".repeat(64)),
                },
            },
            execution_closure: AdmittedExecutionClosure::DirectItemExecutor {
                execution_plan: json!({}),
                protocol_descriptor_document: "fixture protocol".to_owned(),
                command: AdmittedDirectCommandClosure::ContentAddressed {
                    executable_blob_hash,
                    execution_path,
                },
                admitted_project_root: Some(logical_admission_workspace()),
            },
            execution_realization_hash: "8".repeat(64),
            source_binding_hash: None,
            structured_session_profile: None,
            executable_search: Vec::new(),
            runtime_ref: "runtime:fixture/session".to_owned(),
            executor_ref: "native:fixture".to_owned(),
        }
    }

    fn downstream_identities(
        exact_program: &PersistentSessionExactProgram,
    ) -> (String, String, String) {
        let mut capsule = capsule_fixture(exact_program);
        let authority = capsule.authority();
        let realization = ryeos_state::objects::AdmittedExecutionRealization {
            schema: ryeos_state::objects::EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND.to_owned(),
            substrate_identity_hash: "1".repeat(64),
            substrate_attestation_hash: "2".repeat(64),
            launch_authority_digest: authority.digest().unwrap(),
            effective_definition_digest: exact_program.effective_definition_digest.clone(),
            artifact_identity_digest: authority.artifact_identity_digest().unwrap(),
            execution_closure_digest: authority.execution_closure_digest().unwrap(),
            contract_ref: "runtime:fixture/session".to_owned(),
            contract_digest: "3".repeat(64),
            components: Vec::new(),
            properties: BTreeMap::new(),
        };
        let realization_hash = realization.content_hash().unwrap();
        capsule.execution_realization_hash = realization_hash.clone();
        let capsule_hash = capsule.content_hash().unwrap();
        let coordinate = ryeos_provider_contract::RequestCoordinate {
            outer_effective_definition_digest: "4".repeat(64),
            transport: ryeos_provider_contract::TransportCoordinate::AdmittedLocalWorker {
                worker_ref: "worker:fixture/session".to_owned(),
                effective_definition_digest: exact_program.effective_definition_digest.clone(),
                capsule_hash: capsule_hash.clone(),
                execution_realization_hash: realization_hash.clone(),
            },
            provider_family: "local-fixture".to_owned(),
            provider_config_hash: "fixture-config".to_owned(),
            provider_config_value_digest: "5".repeat(64),
            provider_id: "local-fixture".to_owned(),
            profile_id: None,
            model_name: "fixture-model".to_owned(),
            public_headers: Vec::new(),
            credential_header_names: Vec::new(),
            body_sha256: "6".repeat(64),
            requested_output_ceiling: 1,
            credential_binding_hmac: "7".repeat(64),
            credential_authority_generation: "fixture-generation".to_owned(),
            authority_digest: "8".repeat(64),
            admitted_effect_class: Some(ryeos_effect_contract::EffectClass::Recorded),
        };
        let cache_key = coordinate.cache_key().unwrap();
        (capsule_hash, realization_hash, cache_key)
    }

    #[test]
    fn dependency_plan_authority_is_exact_and_never_wildcarded() {
        let EffectivePrincipal::Local(principal) =
            dependency_plan_principal("node-fp", "worker:fixture/session").unwrap()
        else {
            panic!("dependency plan principal must remain local node authority");
        };
        assert_eq!(principal.fingerprint, "node-fp");
        assert_eq!(principal.scopes, ["ryeos.execute.worker.fixture/session"]);
    }

    #[test]
    fn dependency_plan_authority_refuses_suffixes() {
        let error =
            dependency_plan_principal("node-fp", "worker:fixture/session@t:now").unwrap_err();
        assert!(error.to_string().contains("exact and unsuffixed"));
    }

    #[test]
    fn persistent_session_workspace_identity_is_node_path_independent() {
        let logical = logical_admission_workspace();
        assert_eq!(
            logical,
            std::path::Path::new("/ryeos/persistent-session-workspace")
        );
        assert!(!logical.starts_with("/tmp"));
        assert!(!logical.to_string_lossy().contains("cache"));
    }

    #[test]
    fn node_owned_runtime_view_is_snapshot_excluded_and_rejects_symlink_collisions() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let view = create_node_owned_runtime_view(workspace.path()).unwrap();
        assert_eq!(
            view.strip_prefix(workspace.path()).unwrap(),
            std::path::Path::new(".ai/cache/ryeos-runtime")
        );
        assert!(
            ryeos_state::project_sync::is_project_snapshot_floor_excluded(
                ".ai/cache/ryeos-runtime/structured-session"
            )
        );

        let colliding_workspace = tempfile::tempdir().unwrap();
        symlink("/tmp", colliding_workspace.path().join(".ai")).unwrap();
        let error = create_node_owned_runtime_view(colliding_workspace.path()).unwrap_err();
        assert!(error.to_string().contains("collides with a non-directory"));
    }

    #[test]
    fn structured_session_baseline_does_not_require_enforced_isolation() {
        use std::os::unix::fs::PermissionsExt as _;

        let source_root = tempfile::tempdir().unwrap();
        let source_entry = source_root.path().join("worker.yaml");
        std::fs::write(&source_entry, b"worker").unwrap();
        std::fs::write(
            source_root.path().join("baseline.toml"),
            b"setting = true\n",
        )
        .unwrap();
        let state_root = tempfile::tempdir().unwrap();
        let profile = ryeos_state::objects::AdmittedStructuredSessionProfile {
            profile_hash: "a".repeat(64),
            contract: json!({"fixture": true}),
            schema_hashes: BTreeMap::from([("fixture.json".to_owned(), "b".repeat(64))]),
            baseline_source: "baseline.toml".to_owned(),
            baseline_destination: "config.toml".to_owned(),
        };

        let overlay =
            prepare_structured_session_baseline(&profile, &source_entry, state_root.path(), false)
                .unwrap();

        assert!(overlay.is_none());
        let destination = state_root.path().join("config.toml");
        assert_eq!(std::fs::read(&destination).unwrap(), b"setting = true\n");
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&destination, b"workload = true\n").unwrap();
        let overlay =
            prepare_structured_session_baseline(&profile, &source_entry, state_root.path(), false)
                .unwrap();
        assert!(overlay.is_none());
        assert_eq!(std::fs::read(&destination).unwrap(), b"setting = true\n");
        assert_eq!(
            std::fs::metadata(destination).unwrap().permissions().mode() & 0o777,
            0o400
        );
    }

    #[test]
    fn persistent_session_capsule_identity_excludes_resolution_diagnostic_paths() {
        let first = retained_program_fixture("/opt/first/worker.yaml", 'a');
        let second = retained_program_fixture("/srv/second/worker.yaml", 'a');
        let first_capsule = capsule_fixture(&first);
        let second_capsule = capsule_fixture(&second);

        assert_eq!(
            first_capsule.exact_program_hash,
            second_capsule.exact_program_hash
        );
        assert_eq!(
            first_capsule.authority().digest().unwrap(),
            second_capsule.authority().digest().unwrap()
        );
        assert_eq!(
            first_capsule.content_hash().unwrap(),
            second_capsule.content_hash().unwrap()
        );
        assert_eq!(
            downstream_identities(&first),
            downstream_identities(&second)
        );
        let canonical = lillux::canonical_json(&first_capsule.exact_program).unwrap();
        assert!(!canonical.contains("/opt/first"));
        assert!(!canonical.contains("/srv/second"));

        let changed_capsule =
            capsule_fixture(&retained_program_fixture("/opt/first/worker.yaml", 'b'));
        assert_ne!(
            first_capsule.exact_program_hash,
            changed_capsule.exact_program_hash
        );
        assert_ne!(
            first_capsule.content_hash().unwrap(),
            changed_capsule.content_hash().unwrap()
        );
        assert_ne!(
            downstream_identities(&first),
            downstream_identities(&retained_program_fixture("/opt/first/worker.yaml", 'b'))
        );
    }

    #[test]
    fn persistent_session_target_is_checked_before_launch() {
        let path = vec!["supported_target".to_owned()];
        validate_session_target(
            &json!({"supported_target": {
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH
            }}),
            &path,
        )
        .unwrap();
        let error = validate_session_target(
            &json!({"supported_target": {"os": "other", "arch": "other"}}),
            &path,
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not admit this"));
    }
}
