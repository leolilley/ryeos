//! Admitted launch capsules and their verification.
use super::*;

pub(super) fn attach_admitted_launch_capsule(
    state_authority: &ryeos_state::PinnedStateAuthority,
    cas_guard: &ryeos_state::CasMutationGuard,
    mut snapshot: ThreadSnapshot,
    launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
) -> Result<ThreadSnapshot> {
    let Some(capsule) = launch_metadata
        .map(crate::launch_metadata::RuntimeLaunchMetadata::admitted_launch_capsule)
        .transpose()?
        .flatten()
    else {
        return Ok(snapshot);
    };
    if capsule.project_authority != snapshot.project_authority {
        bail!(
            "admitted launch capsule project authority contradicts thread {} birth authority",
            snapshot.thread_id
        );
    }
    if capsule.executor_ref != snapshot.executor_ref || capsule.runtime_ref.is_empty() {
        bail!(
            "admitted launch capsule runtime identity contradicts thread {} birth identity",
            snapshot.thread_id
        );
    }
    let metadata_authority_digest = launch_metadata
        .expect("capsule construction requires launch metadata")
        .admitted_launch_authority()?
        .ok_or_else(|| anyhow!("managed launch metadata lost its admitted authority"))?
        .digest()?;
    let capsule_authority_digest = capsule.launch_authority_digest()?;
    if metadata_authority_digest != capsule_authority_digest {
        bail!("admitted launch capsule construction changed its source launch authority");
    }
    state_authority.ensure_guard(cas_guard)?;
    capsule
        .verify_retained_execution_realization(
            &state_authority.cas_store()?,
            &state_authority.large_object_store()?,
            state_authority.trust_store(),
        )
        .context("verify admitted execution realization before thread birth")?;
    let canonical_capsule = capsule.to_canonical_bytes()?;
    let expected_hash = lillux::sha256_hex(&canonical_capsule);
    let stored_hash = state_authority
        .cas_store()?
        .store_canonical_object_bytes_bounded(
            &canonical_capsule,
            ryeos_state::objects::MAX_ADMITTED_LAUNCH_CAPSULE_BYTES,
        )
        .context("store admitted launch capsule before thread birth")?;
    if stored_hash != expected_hash {
        bail!(
            "admitted launch capsule hash mismatch: expected {expected_hash}, stored {stored_hash}"
        );
    }
    snapshot.admitted_launch_capsule_hash = Some(stored_hash);
    Ok(snapshot)
}

pub(super) fn load_admitted_launch_capsule(
    state_authority: &ryeos_state::PinnedStateAuthority,
    bundle_admission_trust: &BundleAdmissionTrust,
    capsule_hash: &str,
) -> Result<ryeos_state::objects::AdmittedLaunchCapsule> {
    Ok(load_admitted_launch_capsule_with_realization(
        state_authority,
        bundle_admission_trust,
        capsule_hash,
    )?
    .0)
}

pub(super) fn load_admitted_launch_capsule_with_realization(
    state_authority: &ryeos_state::PinnedStateAuthority,
    bundle_admission_trust: &BundleAdmissionTrust,
    capsule_hash: &str,
) -> Result<(
    ryeos_state::objects::AdmittedLaunchCapsule,
    ryeos_state::objects::AdmittedExecutionRealization,
)> {
    let value = state_authority
        .cas_store()?
        .get_object_bounded(
            capsule_hash,
            ryeos_state::objects::MAX_ADMITTED_LAUNCH_CAPSULE_BYTES as u64,
        )?
        .ok_or_else(|| anyhow!("admitted launch capsule is missing from CAS: {capsule_hash}"))?;
    let capsule = ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(value)
        .context("decode supported admitted launch capsule")?;
    let realization = capsule
        .verify_retained_execution_realization(
            &state_authority.cas_store()?,
            &state_authority.large_object_store()?,
            state_authority.trust_store(),
        )
        .context("verify retained admitted execution realization")?;
    if capsule.content_hash()? != capsule_hash {
        bail!("admitted launch capsule object hash is not canonical: {capsule_hash}");
    }
    verify_rooted_bundle_member_source_proof(&capsule, bundle_admission_trust)
        .context("revalidate rooted retained Bundle member-source signatures")?;
    Ok((capsule, realization))
}

pub(super) fn verify_rooted_bundle_member_source_proof(
    capsule: &ryeos_state::objects::AdmittedLaunchCapsule,
    bundle_admission_trust: &BundleAdmissionTrust,
) -> Result<()> {
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executable_identity:
            ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
                content_hash,
                executor_manifest_hash,
                executor_manifest_signer_fingerprint,
                member_source_proof,
            },
        ..
    } = &capsule.artifact_identity
    else {
        return Ok(());
    };
    // The selected Bundle identity is historical evidence in the exact
    // authenticated capsule/plan root; this durable proof does not retain the
    // signed Bundle manifest bytes needed to authenticate that identity alone.
    // Revalidate the retained executor-manifest ref and ItemSource sidecar,
    // while requiring the runtime-owner, Bundle-selection and executor-
    // manifest signer identities to remain in the current trust that admits
    // Bundles at launch.
    let trust_store = bundle_admission_trust.require()?;
    let owner = &member_source_proof.runtime_selection.definition_owner;
    if !matches!(
        owner.trust_class,
        ryeos_state::objects::RetainedTrustClassV1::TrustedBundle
            | ryeos_state::objects::RetainedTrustClassV1::TrustedProject
            | ryeos_state::objects::RetainedTrustClassV1::TrustedNode
    ) || owner
        .signer_fingerprint
        .as_ref()
        .is_none_or(|fingerprint| !trust_store.is_trusted(fingerprint))
    {
        bail!("retained Bundle runtime owner is no longer trusted");
    }
    if !trust_store.is_trusted(&member_source_proof.selected_bundle.signer_fingerprint) {
        bail!("retained Bundle selection signer is no longer trusted");
    }
    let verifier = &trust_store
        .get(executor_manifest_signer_fingerprint)
        .ok_or_else(|| anyhow!("retained executor manifest signer was revoked"))?
        .verifying_key;
    ryeos_engine::binary_resolver::verify_retained_bundle_executor_member_proof(
        member_source_proof,
        executor_manifest_hash,
        executor_manifest_signer_fingerprint,
        content_hash,
        verifier,
    )?;
    Ok(())
}

pub(super) struct ContinuationCapsuleTransition<'a> {
    pub(super) chain_root_id: &'a str,
    pub(super) source_thread_id: &'a str,
    pub(super) launch_metadata: Option<&'a crate::launch_metadata::RuntimeLaunchMetadata>,
    pub(super) expected_result_generation:
        Option<&'a ryeos_state::objects::WorkspaceGenerationPair>,
    pub(super) kind: crate::launch_metadata::ContinuationAuthorityTransitionKind,
}

pub(super) fn attach_continuation_launch_capsule(
    state_authority: &ryeos_state::PinnedStateAuthority,
    bundle_admission_trust: &BundleAdmissionTrust,
    cas_guard: &ryeos_state::CasMutationGuard,
    inner: &Inner,
    snapshot: ThreadSnapshot,
    transition: ContinuationCapsuleTransition<'_>,
) -> Result<(ThreadSnapshot, ThreadSnapshot)> {
    let ContinuationCapsuleTransition {
        chain_root_id,
        source_thread_id,
        launch_metadata,
        expected_result_generation,
        kind,
    } = transition;
    let source_snapshot =
        authoritative_snapshot_for_transition(inner, chain_root_id, source_thread_id)?;
    if let Some(successor_metadata) = launch_metadata {
        let source_metadata = inner
            .runtime_db
            .get_runtime_info(source_thread_id)?
            .and_then(|runtime| runtime.launch_metadata)
            .ok_or_else(|| {
                anyhow!("continuation source {source_thread_id} has no captured launch metadata")
            })?;
        verify_admitted_launch_capsule(
            state_authority,
            bundle_admission_trust,
            &source_snapshot,
            Some(&source_metadata),
        )
        .with_context(|| {
            format!(
                "verify continuation source {source_thread_id} metadata against its authoritative admitted capsule"
            )
        })?;
        let source_resume = source_metadata.resume_context.as_ref().ok_or_else(|| {
            anyhow!("continuation source {source_thread_id} has no captured ResumeContext")
        })?;
        let successor_resume = successor_metadata.resume_context.as_ref().ok_or_else(|| {
            anyhow!(
                "continuation successor {} has no captured ResumeContext",
                snapshot.thread_id
            )
        })?;
        source_snapshot.verify_result_workspace_output_capture(&state_authority.cas_store()?)?;
        let terminal_generation =
            source_snapshot
                .result_project_snapshot_hash
                .as_ref()
                .map(
                    |snapshot_hash| ryeos_state::objects::WorkspaceGenerationPair {
                        snapshot_hash: snapshot_hash.clone(),
                        output_capture_hash: source_snapshot
                            .result_workspace_output_capture_hash
                            .clone(),
                    },
                );
        if let (Some(expected), Some(terminal)) =
            (expected_result_generation, terminal_generation.as_ref())
            && expected != terminal
        {
            bail!(
                "continuation frozen result contradicts the source's authoritative terminal generation"
            );
        }
        successor_resume.validate_continuation_transition_from(
            source_resume,
            expected_result_generation.or(terminal_generation.as_ref()),
            kind.clone(),
        )?;
    }
    let rebound_names = match &kind {
        crate::launch_metadata::ContinuationAuthorityTransitionKind::OperatorRebind { names } => {
            Some(names)
        }
        crate::launch_metadata::ContinuationAuthorityTransitionKind::Inherit
        | crate::launch_metadata::ContinuationAuthorityTransitionKind::OperatorFollowUp => None,
    };
    let source_capsule = source_snapshot
        .admitted_launch_capsule_hash
        .as_deref()
        .map(|hash| {
            load_admitted_launch_capsule_with_realization(
                state_authority,
                bundle_admission_trust,
                hash,
            )
        })
        .transpose()?;
    let successor_capsule = launch_metadata
        .map(crate::launch_metadata::RuntimeLaunchMetadata::admitted_launch_capsule)
        .transpose()?
        .flatten();
    match (&source_capsule, &successor_capsule) {
        // A rebind re-admits part of an admitted capsule. Without one on both
        // sides there is nothing to hold the rest of the program equal to.
        (None, None) if rebound_names.is_some() => bail!(
            "operator rebind continuation {} requires an admitted launch capsule on source {}",
            snapshot.thread_id,
            source_thread_id
        ),
        (None, None) => {}
        (Some(_), None) => bail!(
            "continuation successor {} dropped source {} admitted program authority",
            snapshot.thread_id,
            source_thread_id
        ),
        (None, Some(_)) => bail!(
            "continuation successor {} introduced admitted program authority absent from source {}",
            snapshot.thread_id,
            source_thread_id
        ),
        (Some((source, source_realization)), Some(successor)) => {
            // The comparison is chosen by the typed transition being
            // committed, never by which capsule fields happen to differ.
            let admitted =
                continuation_capsule_admitted(source, successor, &kind).with_context(|| {
                    format!(
                        "continuation successor {} continues source {}",
                        snapshot.thread_id, source_thread_id
                    )
                })?;
            if !admitted {
                bail!(
                    "continuation successor {} changed immutable admitted launch capsule from source {}",
                    snapshot.thread_id,
                    source_thread_id
                );
            }
            let successor_realization = successor
                .verify_retained_execution_realization(
                    &state_authority.cas_store()?,
                    &state_authority.large_object_store()?,
                    state_authority.trust_store(),
                )
                .context("verify continuation successor execution realization")?;
            let expected_successor_realization = expected_continuation_successor_realization(
                source_realization,
                successor,
                rebound_names.is_some(),
            )?;
            if expected_successor_realization != successor_realization {
                bail!(
                    "continuation successor {} changed retained execution realization outside its launch authority",
                    snapshot.thread_id
                );
            }
        }
    }
    let snapshot =
        attach_admitted_launch_capsule(state_authority, cas_guard, snapshot, launch_metadata)?;
    Ok((snapshot, source_snapshot))
}

/// Whether `successor` is a capsule the typed transition `kind` may commit
/// after `source`.
///
/// A rebind is admitted only when the runtime contract sealed in the source
/// lets an operator rebind every named binding, and the successor differs
/// from the source only where a rebind may. Every other transition requires
/// the same admitted program, so a later continuation of a rebound thread
/// carries the rebound capsule exactly and cannot fall back to an earlier
/// binding.
fn continuation_capsule_admitted(
    source: &ryeos_state::objects::AdmittedLaunchCapsule,
    successor: &ryeos_state::objects::AdmittedLaunchCapsule,
    kind: &crate::launch_metadata::ContinuationAuthorityTransitionKind,
) -> Result<bool> {
    match kind {
        crate::launch_metadata::ContinuationAuthorityTransitionKind::OperatorRebind { names } => {
            require_operator_rebindable(source, names)?;
            source.same_rebound_continuation_admission(successor, names)
        }
        crate::launch_metadata::ContinuationAuthorityTransitionKind::Inherit
        | crate::launch_metadata::ContinuationAuthorityTransitionKind::OperatorFollowUp => {
            source.same_continuation_program_admission(successor)
        }
    }
}

/// The one realization a continuation successor may retain: the source's
/// verified realization, committed to the successor's launch authority.
///
/// A rebound successor replaces part of its execution closure, so its
/// realization commits the successor's closure digest as well. In either
/// form the substrate evidence, effective definition, artifact identity,
/// execution contract, components and properties are the source's exactly;
/// nothing is re-resolved against the node's current state.
fn expected_continuation_successor_realization(
    source_realization: &ryeos_state::objects::AdmittedExecutionRealization,
    successor: &ryeos_state::objects::AdmittedLaunchCapsule,
    rebound: bool,
) -> Result<ryeos_state::objects::AdmittedExecutionRealization> {
    let successor_authority = successor.launch_authority();
    let mut expected = source_realization.clone();
    expected.launch_authority_digest = successor_authority.digest()?;
    if rebound {
        expected.execution_closure_digest = successor_authority.execution_closure_digest()?;
    }
    Ok(expected)
}

/// Require that the runtime contract admitted with `source` lets an operator
/// rebind every name. The contract is read from the signed descriptor sealed
/// in the source capsule, never from the runtime registry current now, so a
/// thread admitted under a contract that seals a binding stays sealed.
pub fn require_operator_rebindable(
    source: &ryeos_state::objects::AdmittedLaunchCapsule,
    names: &std::collections::BTreeSet<String>,
) -> Result<()> {
    let ryeos_state::objects::AdmittedExecutionClosure::ManagedRuntime {
        runtime_descriptor_document,
        ..
    } = &source.execution_closure
    else {
        bail!("only a managed runtime launch declares rebindable ref bindings");
    };
    // The capsule's own validation already ties this document's body to the
    // admitted runtime content hash and signer.
    let runtime: ryeos_engine::runtime_registry::RuntimeYaml = serde_yaml::from_str(
        &lillux::signature::strip_signature_lines(runtime_descriptor_document),
    )
    .context("decode admitted runtime descriptor")?;
    if names.is_empty() {
        bail!("an operator rebind continuation names no ref binding");
    }
    for name in names {
        let rebindable =
            runtime
                .launch_contract
                .ref_bindings
                .get(name)
                .is_some_and(|declaration| {
                    declaration.continuation_rebind
                        == ryeos_engine::runtime_registry::ContinuationRebind::Operator
                });
        if !rebindable {
            bail!("the admitted runtime contract does not let an operator rebind `{name}`");
        }
    }
    Ok(())
}

pub(super) fn verify_admitted_launch_capsule(
    state_authority: &ryeos_state::PinnedStateAuthority,
    bundle_admission_trust: &BundleAdmissionTrust,
    snapshot: &ThreadSnapshot,
    launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
) -> Result<()> {
    let expected = launch_metadata
        .map(crate::launch_metadata::RuntimeLaunchMetadata::admitted_launch_capsule)
        .transpose()?
        .flatten();
    match (snapshot.admitted_launch_capsule_hash.as_deref(), expected) {
        (None, None) => Ok(()),
        (Some(_), None) => bail!(
            "thread {} has an admitted launch capsule but no matching runtime launch identity",
            snapshot.thread_id
        ),
        (None, Some(_)) => bail!(
            "thread {} managed launch is missing its authoritative admitted capsule",
            snapshot.thread_id
        ),
        (Some(rooted_hash), Some(expected)) => {
            let expected_hash = expected.content_hash()?;
            let rooted =
                load_admitted_launch_capsule(state_authority, bundle_admission_trust, rooted_hash)?;
            if rooted.schema == expected.schema && rooted_hash != expected_hash {
                bail!(
                    "thread {} admitted capsule drift: authoritative {}, attempted {}",
                    snapshot.thread_id,
                    rooted_hash,
                    expected_hash
                );
            }
            if !rooted.same_continuation_admission(&expected)? {
                bail!(
                    "thread {} launch attempt changed immutable admitted capsule authority",
                    snapshot.thread_id
                );
            }
            Ok(())
        }
    }
}

impl StateStore {
    pub fn admitted_launch_capsule_hash(&self, thread_id: &str) -> Result<Option<String>> {
        let g = self.lock()?;
        Ok(g.state_db
            .get_thread(thread_id)?
            .and_then(|thread| thread.admitted_launch_capsule_hash))
    }

    /// Load the authoritative CAS-rooted launch capsule for an admitted
    /// thread. Operational SQLite metadata is deliberately not consulted.
    pub fn admitted_launch_capsule(
        &self,
        thread_id: &str,
    ) -> Result<Option<ryeos_state::objects::AdmittedLaunchCapsule>> {
        Ok(self
            .admitted_launch_capsule_with_coordinates(thread_id)?
            .map(|(_, _, capsule)| capsule))
    }

    /// Read one immutable admission coordinate while holding the short state
    /// lock, then verify its retained CAS closure outside that lock. The shared
    /// storage guard covers both steps so GC cannot retire the selected bytes.
    pub fn admitted_launch_capsule_with_coordinates(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, String, ryeos_state::objects::AdmittedLaunchCapsule)>> {
        let _guard = self.state_authority.acquire_shared_guard()?;
        let Some((chain_root_id, capsule_hash)) =
            self.admitted_launch_capsule_coordinate(thread_id)?
        else {
            return Ok(None);
        };
        let capsule = load_admitted_launch_capsule(
            &self.state_authority,
            &self.bundle_admission_trust,
            &capsule_hash,
        )?;
        Ok(Some((chain_root_id, capsule_hash, capsule)))
    }

    pub(super) fn admitted_launch_capsule_coordinate(
        &self,
        thread_id: &str,
    ) -> Result<Option<(String, String)>> {
        let g = self.lock()?;
        let Some(snapshot) = g.state_db.get_thread(thread_id)? else {
            return Ok(None);
        };
        Ok(snapshot
            .admitted_launch_capsule_hash
            .map(|hash| (snapshot.chain_root_id, hash)))
    }

    /// Load and independently verify the exact root program retained by the
    /// thread's CAS-rooted admitted capsule. Only the subject and capsule hash
    /// cross this boundary; sealed invocation, capabilities, and closure stay
    /// private to execution recovery.
    pub fn admitted_program_evidence(
        &self,
        thread_id: &str,
    ) -> Result<Option<AdmittedProgramEvidence>> {
        Ok(self
            .admitted_execution_comparison_evidence(thread_id)?
            .map(|evidence| evidence.program))
    }

    /// Load the exact admitted program and its retained execution realization
    /// under one verification path. This is intentionally keyed by an
    /// already-authorized thread id; hashes are evidence identities, not
    /// lookup authority.
    pub fn admitted_execution_comparison_evidence(
        &self,
        thread_id: &str,
    ) -> Result<Option<AdmittedExecutionComparisonEvidence>> {
        let _guard = self.state_authority.acquire_shared_guard()?;
        let Some((_, capsule_hash)) = self.admitted_launch_capsule_coordinate(thread_id)? else {
            return Ok(None);
        };
        let (capsule, execution_realization) = load_admitted_launch_capsule_with_realization(
            &self.state_authority,
            &self.bundle_admission_trust,
            &capsule_hash,
        )?;
        capsule.validate()?;
        let request: crate::thread_lifecycle::SealedRootExecutionRequest =
            serde_json::from_value(capsule.sealed_invocation.clone())
                .context("decode admitted sealed invocation")?;
        if request.admitted_program_value()? != capsule.exact_program
            || request.admitted_program_hash()? != capsule.exact_program_hash
        {
            bail!("admitted sealed invocation contradicts its exact program projection");
        }
        let subject = request
            .admitted_program_subject()
            .context("verify exact admitted root subject")?;
        let resolution = request.admitted_effective_resolution()?.clone();
        let effective_definition_digest = request.effective_definition_digest().clone();
        Ok(Some(AdmittedExecutionComparisonEvidence {
            execution_realization_hash: capsule.execution_realization_hash.clone(),
            execution_realization,
            program: AdmittedProgramEvidence {
                admitted_launch_capsule_hash: capsule_hash,
                subject,
                effective_definition_digest,
                resolution,
            },
        }))
    }

    /// Compare a recovery attempt with the CAS-rooted artifact identity before
    /// any process is permitted to execute. Runtime SQLite metadata is an
    /// operational index; the signed thread snapshot and its capsule are the
    /// authority boundary.
    pub fn verify_admitted_artifact_identity(
        &self,
        thread_id: &str,
        attempted: &ryeos_state::objects::AdmittedLaunchArtifactIdentity,
    ) -> Result<()> {
        attempted.validate()?;
        let g = self.lock()?;
        let snapshot = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        let capsule_hash = snapshot
            .admitted_launch_capsule_hash
            .as_deref()
            .ok_or_else(|| {
                anyhow!("thread {thread_id} has no authoritative admitted launch capsule")
            })?;
        let capsule = load_admitted_launch_capsule(
            &self.state_authority,
            &self.bundle_admission_trust,
            capsule_hash,
        )?;
        if &capsule.artifact_identity != attempted {
            bail!(
                "thread {thread_id} recovery artifact identity differs from its authoritative \
                 admitted capsule: admitted={:?}, attempted={attempted:?}",
                capsule.artifact_identity
            );
        }
        Ok(())
    }

    /// Inspect the retained placement and its original deadlines. This read
    /// does not reserve contact, renew authority, or settle its obligation.
    pub fn external_allocation(
        &self,
        placement: &str,
    ) -> Result<Option<runtime_db::external_execution::ExternalAllocationRecord>> {
        self.lock()?.runtime_db.external_allocation(placement)
    }
}

#[cfg(test)]
mod rebind_tests {
    use super::*;
    use ryeos_state::objects::{
        AdmittedExecutionClosure, AdmittedExecutionRealization, AdmittedLaunchArtifactIdentity,
        AdmittedLaunchCapsule,
    };

    /// The shipped directive runtime lets an operator rebind `model`; the
    /// shipped graph runtime declares `product_recipe` and seals it.
    const REBINDABLE_RUNTIME: &str =
        include_str!("../../../../../bundles/directive/.ai/runtimes/directive-runtime.yaml");
    const SEALED_RUNTIME: &str =
        include_str!("../../../../../bundles/graph/.ai/runtimes/graph-runtime.yaml");

    fn names(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn capsule(runtime_descriptor_document: &str, prepared_marker: &str) -> AdmittedLaunchCapsule {
        let runtime = lillux::signature::parse_signature_line(
            runtime_descriptor_document.lines().next().unwrap(),
            "#",
            None,
        )
        .expect("shipped runtime descriptor signature");
        let protocol_document = lillux::signature::sign_content(
            "protocol fixture\n",
            &lillux::crypto::SigningKey::from_bytes(&[22; 32]),
            "#",
            None,
        );
        let protocol = lillux::signature::parse_signature_line(
            protocol_document.lines().next().unwrap(),
            "#",
            None,
        )
        .expect("protocol descriptor signature");
        AdmittedLaunchCapsule {
            schema: ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
            kind: "admitted_launch_capsule".to_owned(),
            exact_program: serde_json::json!({}),
            exact_program_hash: lillux::sha256_hex(prepared_marker.as_bytes()),
            sealed_invocation: serde_json::json!({}),
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            lifecycle_authority:
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE,
            launch_driver: ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime,
            artifact_identity: AdmittedLaunchArtifactIdentity::ManagedRuntime {
                runtime_ref: "runtime:test/fixture".to_owned(),
                runtime_content_hash: runtime.content_hash,
                runtime_signer_fingerprint: runtime.signer_fingerprint,
                protocol_ref: "protocol:test/fixture".to_owned(),
                protocol_content_hash: protocol.content_hash,
                protocol_signer_fingerprint: protocol.signer_fingerprint,
                executor_ref: "executor:test/fixture".to_owned(),
                executor_content_hash: "7".repeat(64),
                executor_bundle_manifest_hash: "8".repeat(64),
                executor_bundle_signer_fingerprint: "9".repeat(64),
            },
            execution_closure: AdmittedExecutionClosure::ManagedRuntime {
                prepared_runtime_launch: serde_json::json!({
                    "binding_records": {},
                    "runtime_facts": {"model_name": prepared_marker},
                }),
                runtime_descriptor_document: runtime_descriptor_document.to_owned(),
                protocol_descriptor_document: protocol_document,
                executor_blob_hash: "7".repeat(64),
            },
            execution_realization_hash: "a".repeat(64),
            source_binding_hash: None,
            accounting_scope: None,
            effective_caps: Vec::new(),
            parent_delegation_caps: None,
            thread_ceiling: ryeos_state::objects::ThreadWorkloadCeiling::node_root(),
            runtime_ref: "runtime:test/fixture".to_owned(),
            executor_ref: "executor:test/fixture".to_owned(),
        }
    }

    #[test]
    fn rebind_permission_is_read_from_the_admitted_runtime_descriptor() {
        let rebindable = capsule(REBINDABLE_RUNTIME, "first");
        require_operator_rebindable(&rebindable, &names(&["model"])).unwrap();

        // A name the admitted contract does not declare, or names nothing.
        for refused in [&["absent"][..], &["model", "absent"][..], &[][..]] {
            assert!(
                require_operator_rebindable(&rebindable, &names(refused)).is_err(),
                "{refused:?}"
            );
        }
        // A declared binding the admitted contract seals.
        let sealed = capsule(SEALED_RUNTIME, "first");
        let error = require_operator_rebindable(&sealed, &names(&["product_recipe"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not let an operator rebind"), "{error}");
        assert!(require_operator_rebindable(&sealed, &names(&["model"])).is_err());
    }

    /// A capsule that validates, binding `model` to the given item.
    fn bound_capsule(model: &str) -> AdmittedLaunchCapsule {
        let project = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "project:rebind-boundary".into(),
            None,
            "b".repeat(64),
            ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();
        crate::state_store::external_fixture_launch_metadata(
            project,
            serde_json::json!({"binding_records": {"model": {"canonical_ref": model}}}),
            serde_json::json!({}),
        )
        .admitted_launch_capsule()
        .unwrap()
        .unwrap()
    }

    #[test]
    fn the_state_boundary_refuses_a_rebind_the_sealed_contract_does_not_permit() {
        use crate::launch_metadata::ContinuationAuthorityTransitionKind as Kind;
        // The graph runtime declares `product_recipe` and seals it: the
        // boundary refuses the rebind whatever the successor looks like.
        let sealed = capsule(SEALED_RUNTIME, "first");
        let successor = capsule(SEALED_RUNTIME, "second");
        let error = continuation_capsule_admitted(
            &sealed,
            &successor,
            &Kind::OperatorRebind {
                names: names(&["product_recipe"]),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("does not let an operator rebind"), "{error}");
        // A name the contract does not declare at all is refused the same way.
        assert!(
            continuation_capsule_admitted(
                &sealed,
                &successor,
                &Kind::OperatorRebind {
                    names: names(&["model"]),
                },
            )
            .is_err()
        );
        // A capsule whose sealed descriptor is not a runtime contract that
        // permits the name cannot be rebound either.
        assert!(
            continuation_capsule_admitted(
                &bound_capsule("directive:test/models/first"),
                &bound_capsule("directive:test/models/second"),
                &Kind::OperatorRebind {
                    names: names(&["model"]),
                },
            )
            .is_err()
        );
    }

    #[test]
    fn a_continuation_after_a_rebind_inherits_the_rebound_capsule_exactly() {
        use crate::launch_metadata::ContinuationAuthorityTransitionKind as Kind;
        let original = bound_capsule("directive:test/models/first");
        let rebound = bound_capsule("directive:test/models/second");

        for kind in [Kind::Inherit, Kind::OperatorFollowUp] {
            // A machine or operator continuation of the rebound thread that
            // carries the rebound capsule is admitted.
            assert!(
                continuation_capsule_admitted(&rebound, &rebound.clone(), &kind).unwrap(),
                "{kind:?}"
            );
            // One that carries the binding from before the rebind is not, and
            // neither transition can itself change a binding.
            assert!(
                !continuation_capsule_admitted(&rebound, &original, &kind).unwrap(),
                "{kind:?}"
            );
            assert!(
                !continuation_capsule_admitted(&original, &rebound, &kind).unwrap(),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn only_a_rebound_successor_realization_commits_a_new_execution_closure() {
        let source = capsule(REBINDABLE_RUNTIME, "first");
        let successor = capsule(REBINDABLE_RUNTIME, "second");
        let source_authority = source.launch_authority();
        let successor_authority = successor.launch_authority();
        assert_ne!(
            source_authority.execution_closure_digest().unwrap(),
            successor_authority.execution_closure_digest().unwrap()
        );
        let source_realization = AdmittedExecutionRealization {
            schema: ryeos_state::objects::EXECUTION_REALIZATION_SCHEMA_VERSION,
            kind: ryeos_state::objects::ADMITTED_EXECUTION_REALIZATION_KIND.to_owned(),
            substrate_identity_hash: "1".repeat(64),
            substrate_attestation_hash: "2".repeat(64),
            launch_authority_digest: source_authority.digest().unwrap(),
            effective_definition_digest: "3".repeat(64),
            artifact_identity_digest: source_authority.artifact_identity_digest().unwrap(),
            execution_closure_digest: source_authority.execution_closure_digest().unwrap(),
            contract_ref: "runtime:test/fixture".to_owned(),
            contract_digest: "4".repeat(64),
            components: Vec::new(),
            properties: std::collections::BTreeMap::from([(
                "isolation_enforced".to_owned(),
                serde_json::Value::Bool(true),
            )]),
        };

        // Every other continuation keeps the one-field rule: the closure
        // digest stays the source's, so a successor whose closure moved does
        // not verify against the expected realization.
        let inherited =
            expected_continuation_successor_realization(&source_realization, &successor, false)
                .unwrap();
        let mut one_field = source_realization.clone();
        one_field.launch_authority_digest = successor_authority.digest().unwrap();
        assert_eq!(inherited, one_field);

        // A rebound successor commits its own closure digest and nothing else.
        let rebound =
            expected_continuation_successor_realization(&source_realization, &successor, true)
                .unwrap();
        let mut two_fields = one_field.clone();
        two_fields.execution_closure_digest =
            successor_authority.execution_closure_digest().unwrap();
        assert_eq!(rebound, two_fields);
        assert_ne!(rebound, inherited);

        // Fresh substrate evidence, components or properties are not the
        // source's realization under either rule.
        let mut fresh = rebound.clone();
        fresh.substrate_attestation_hash = "5".repeat(64);
        assert_ne!(fresh, rebound);
        let mut widened = rebound.clone();
        widened.properties.insert(
            "isolation_enforced".to_owned(),
            serde_json::Value::Bool(false),
        );
        assert_ne!(widened, rebound);
    }
}
