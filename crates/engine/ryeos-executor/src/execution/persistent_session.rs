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
    pub(crate) resolution_output: ryeos_engine::resolution::ResolutionOutput,
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
            let (hash, publication) =
                admit_session_capsule(state, engine, dependency, &declaration, &protocol)
                    .with_context(|| format!("admit persistent-session dependency `{name}`"))?;
            admitted_sessions.insert(name.clone(), hash);
            publications.push(publication);
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
    Ok(Some((declaration, protocol)))
}

fn admit_session_capsule(
    state: &AppState,
    engine: &ryeos_engine::engine::Engine,
    dependency: &mut PreparedExecutionDependency,
    declaration: &PersistentSessionDecl,
    protocol: &VerifiedProtocol,
) -> Result<(String, ryeos_state::PendingCasPublication)> {
    let roots = engine.resolution_roots(None);
    let mut resolution = dependency.resolution.clone();
    let captured_external = ryeos_app::external_content_admission::admit_external_realizations(
        state,
        engine,
        &dependency.captured_verified_subject()?.resolved.kind,
        &mut resolution,
        &roots,
        None,
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
    )?;
    let finalized = ryeos_engine::effective_program::finalize_effective_program(candidate, proof)?;
    // The outer runtime capsule must retain the exact augmented dependency the
    // session capsule executes, including its admitted external-realization
    // projection. Keeping only the pre-admission resolution would make outer
    // recovery compare two different programs.
    dependency.resolution = finalized.resolution().clone();
    let exact_program = PersistentSessionExactProgram {
        effective_definition_digest: finalized.effective_definition_digest().as_str().to_owned(),
        resolution_output: finalized.resolution().clone(),
    };
    let exact_program_value = serde_json::to_value(&exact_program)?;
    let exact_program_hash = canonical_hash(&exact_program_value)?;
    let workspace = deterministic_admission_workspace(state, &exact_program_hash);
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

    let mut publication = captured_external.and_then(|captured| captured.into_publication());
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
    Ok((stored, publication))
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
    if exact.resolution_output.root.resolved_ref != dependency.canonical_ref
        || canonical_hash(&serde_json::to_value(&exact.resolution_output)?)?
            != canonical_hash(&serde_json::to_value(&dependency.resolution)?)?
    {
        bail!("persistent-session capsule contradicts its captured dependency");
    }
    let observed_digest = exact.resolution_output.effective_definition_digest()?;
    if observed_digest.as_str() != exact.effective_definition_digest {
        bail!("persistent-session exact effective-definition digest changed");
    }
    validate_capsule_current_trust(engine, &capsule)?;
    let (protocol_ref, protocol_digest) = capsule_protocol_identity(&capsule)?;
    super::execution_realization::verify_persistent_session(
        state,
        &capsule,
        &exact.resolution_output,
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
    super::execution_realization::verify_persistent_session(
        state,
        &capsule,
        &exact.resolution_output,
        &exact.effective_definition_digest,
        capsule_protocol_identity(&capsule)?.0,
        capsule_protocol_identity(&capsule)?.1,
    )?;
    Ok(AdmittedPersistentSessionIdentity {
        canonical_ref: exact.resolution_output.root.resolved_ref,
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
    super::execution_realization::verify_persistent_session(
        state,
        &capsule,
        &exact.resolution_output,
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
    let bound = if state.isolation.is_enforced() {
        super::external_content::bind_external_realizations(
            state,
            &exact.resolution_output,
            &workspace,
        )?
    } else {
        super::external_content::bind_external_realizations_in_private_workspace(
            state,
            &exact.resolution_output,
            &workspace,
        )?
    };
    let (mounts, external_env, leases) = match bound {
        Some(bound) => {
            let (mounts, env, leases) = bound.into_spawn_parts();
            (mounts, Some(env), leases)
        }
        None => (Vec::new(), None, Vec::new()),
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
    plan.bind_persistent_session_spawn_environment(external_env.as_deref())?;
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

fn deterministic_admission_workspace(state: &AppState, exact_program_hash: &str) -> PathBuf {
    state
        .config
        .runtime_root()
        .cache()
        .join("executions")
        .join(format!(
            "persistent-session-plan-{}",
            &exact_program_hash[..24]
        ))
}

fn canonical_hash(value: &Value) -> Result<String> {
    Ok(lillux::sha256_hex(
        lillux::canonical_json(value)?.as_bytes(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
