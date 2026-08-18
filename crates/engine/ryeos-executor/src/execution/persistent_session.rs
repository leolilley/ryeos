//! Admission, recovery, and execution of kind-neutral persistent sessions.
//!
//! A kind owns the executable definition and declares the mechanical session
//! contract.  This module does not interpret request or response bodies.  It
//! captures that already-resolved definition into an immutable capsule before
//! the outer runtime capsule is minted, then reopens only retained content.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ryeos_app::persistent_session::StartedPersistentSession;
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::{ResolvedExecutionRequest, prepare_captured_item_plan};
use ryeos_engine::contracts::{
    EffectivePrincipal, ExecutionHints, PlanContext, Principal, ProjectContext,
    SubjectResolutionAuthority,
};
use ryeos_engine::kind_registry::{PersistentSessionDecl, TerminatorDecl};
use ryeos_engine::protocols::{VerifiedProtocol, validate_persistent_session_protocol};
use ryeos_state::objects::{
    AdmittedPersistentSessionCapsule, PERSISTENT_SESSION_CAPSULE_KIND,
    PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION, PersistentSessionAuthority,
    PersistentSessionLifecycleContract, PersistentSessionWireContract,
};

use super::launch_preparation::{PreparedExecutionDependency, PreparedRuntimeLaunch};

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

/// Admit fresh session dependencies or verify recovered capsule references.
/// No mutable item/config lookup occurs on recovery.
pub(crate) fn admit_or_verify_prepared_sessions(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    prepared: &mut PreparedRuntimeLaunch,
    recovered: bool,
) -> Result<AdmittedSessionPublications> {
    let mut expected_names = BTreeSet::new();
    let mut publications = Vec::new();
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
            verify_session_capsule(state, engine, dependency, hash)
                .with_context(|| format!("verify recovered session dependency `{name}`"))?;
        } else {
            let Some((declaration, protocol)) = session_contract(engine, dependency)? else {
                continue;
            };
            expected_names.insert(name.clone());
            if admitted_sessions.contains_key(name) {
                bail!("fresh session dependency `{name}` already carries a capsule hash");
            }
            let (hash, admitted_publications) =
                admit_session_capsule(state, engine, dependency, &declaration, &protocol)
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
    let Some(declaration) = execution.persistent_session.clone() else {
        return Ok(None);
    };
    let TerminatorDecl::Subprocess { protocol_ref } = execution
        .terminator
        .as_ref()
        .ok_or_else(|| anyhow!("persistent-session kind `{kind}` has no terminator"))?
    else {
        bail!("persistent-session kind `{kind}` is not subprocess-terminated");
    };
    let protocol =
        engine.protocols.get(protocol_ref).cloned().ok_or_else(|| {
            anyhow!("persistent-session protocol `{protocol_ref}` is not installed")
        })?;
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
            None,
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
        .unwrap_or(
            state
                .state_store
                .with_state_db(|db| db.pinned_authority())?,
        );
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
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
    let observed_digest = exact.resolution_output.effective_definition_digest()?;
    if observed_digest.as_str() != exact.effective_definition_digest {
        bail!("persistent-session exact effective-definition digest changed");
    }
    validate_capsule_current_trust(engine, &capsule)?;
    let (protocol_ref, protocol_digest) = capsule_protocol_identity(&capsule)?;
    let resolution = exact.resolution_output.restore();
    ryeos_app::source_closure_admission::recover_source_closure(state, &state.engine, &resolution)?;
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

fn start_capsule_process(
    state: &AppState,
    capsule_hash: &str,
    capsule: &AdmittedPersistentSessionCapsule,
    exact: &PersistentSessionExactProgram,
) -> Result<StartedPersistentSession> {
    let workspace_name = format!(
        "persistent-session-{}-{:08x}",
        &capsule_hash[..16],
        rand::random::<u32>()
    );
    let (workspace, workspace_lifeline) = ryeos_app::temp_dir_guard::create_projectless_workspace(
        &state.config.runtime_root().cache(),
        &workspace_name,
    )?;
    let resolution = exact.resolution_output.restore();
    super::source_closure::validate_external_mount_separation(state, &resolution)?;
    let private_budget = (!state.isolation.is_enforced())
        .then(super::external_content::private_materialization_budget)
        .transpose()?;
    let bound = if state.isolation.is_enforced() {
        super::external_content::bind_external_realizations(state, &resolution, &workspace)?
    } else {
        super::external_content::bind_external_realizations_in_private_workspace_with_budget(
            state,
            &resolution,
            &workspace,
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
            &workspace,
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
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
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
    plan.bind_persistent_session_spawn_environment(
        external_env.as_deref(),
        source_env,
        source_entry,
    )?;
    let running = plan.spawn_persistent_session(
        state,
        &workspace,
        mounts,
        target_channel,
        &capsule.lifecycle,
        &format!("session-{}", &capsule_hash[..24]),
    )?;
    let mut lifelines: Vec<Box<dyn Send + Sync>> = Vec::with_capacity(leases.len() + 1);
    lifelines.push(Box::new(workspace_lifeline));
    lifelines.extend(
        leases
            .into_iter()
            .map(|lease| Box::new(lease) as Box<dyn Send + Sync>),
    );
    if let Some(source) = source {
        lifelines.push(Box::new(source));
    }
    Ok(StartedPersistentSession {
        running,
        socket: daemon_socket,
        lifelines,
    })
}

fn load_capsule(state: &AppState, hash: &str) -> Result<AdmittedPersistentSessionCapsule> {
    if !lillux::valid_hash(hash) {
        bail!("persistent-session capsule hash is not canonical");
    }
    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
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
