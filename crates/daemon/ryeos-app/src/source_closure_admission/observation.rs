//! Bounded source observation under the existing validation owner. These are
//! candidate identities, never a retained source proof or executable admission.

use super::*;

const MAX_OBSERVATION_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, serde::Serialize)]
pub struct StructuredSourceValidationObservation {
    pub schema: &'static str,
    pub authority: &'static str,
    pub canonical_ref: String,
    pub bundle_generation_identity: String,
    pub signed_effective_definition_digest: String,
    pub publisher_fingerprint: String,
    pub expected_source_content_manifest_hash: Option<String>,
    pub candidate_source_content_manifest_hash: String,
    pub candidate_source_binding_hash: String,
    pub candidate_profile_hash: String,
    pub file_count: usize,
    pub total_bytes: u64,
    /// Existing literal-content observations only. A ready helper does not
    /// establish product selection, Worker admission or qualification.
    pub external_content:
        Option<crate::external_content_admission::ExternalContentValidationPreview>,
}

impl StructuredSourceValidationObservation {
    fn require_bounded(&self) -> anyhow::Result<()> {
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_OBSERVATION_BYTES,
            "structured source observation exceeds its response bound"
        );
        Ok(())
    }
}

/// Observe one exact trusted Bundle subject selected by its signed session
/// protocol and source contract. No kind name, product, installed executable,
/// credential or runtime is selected here. Ordinary validation remains separate.
pub fn observe_bundle_structured_source(
    state: &AppState,
    item_ref: &str,
) -> anyhow::Result<StructuredSourceValidationObservation> {
    let canonical = CanonicalRef::parse(item_ref)?;
    ensure!(
        canonical.to_string() == item_ref && canonical.suffix.is_none(),
        "structured source observation requires an exact canonical ref"
    );
    let kind = canonical.kind.clone();
    state.engine.with_checked_bundle_generation(|generation| {
        let engine = &state.engine;
        let roots = engine.resolution_roots(None);
        let resolution = generation.effective_resolution_output(
            ryeos_engine::engine::EffectiveItemRequest {
                item_ref: canonical,
                expected_kind: Some(kind.clone()),
                project_root: None,
                subject_resolution_authority: SubjectResolutionAuthority::Projectless,
            },
        )?;
        ensure!(
            resolution.root.resolved_ref == item_ref
                && resolution.effective_trust_class
                    == ryeos_engine::resolution::TrustClass::TrustedBundle
                && resolution.root.source_space == ItemSpace::Bundle
                && matches!(resolution.root.source_root, ItemSourceRoot::Bundle { .. }),
            "structured source observation requires a trusted Bundle subject"
        );
        let schema = engine.kinds.get(&kind).context("source kind is absent")?;
        let execution = schema.execution.as_ref().context("source kind is not executable")?;
        let source_contract = execution.source_closure.as_ref().context("source contract is absent")?;
        ensure!(
            execution.persistent_session.is_some()
                && matches!(&source_contract.location,
                    ryeos_engine::kind_registry::SourceClosureLocationDecl::OwnerRelativeSource { .. }),
            "source observation requires an owner-relative session source contract"
        );
        let ryeos_engine::kind_registry::TerminatorDecl::Subprocess { protocol } = execution
            .terminator.as_ref().context("source kind has no terminator")?
        else {
            anyhow::bail!("source observation requires a subprocess session protocol");
        };
        let protocol_ref = protocol.resolve(&resolution.composed.composed).map_err(anyhow::Error::msg)?;
        let protocol = engine.protocols.get(&protocol_ref).context("source protocol is not installed")?;
        ryeos_engine::protocols::validate_persistent_session_protocol(&protocol.descriptor)
            .map_err(anyhow::Error::msg)?;
        ensure!(
            protocol.descriptor.session.as_ref().is_some_and(|session|
                session.wire_protocol == "ryeos.structured-session"),
            "source observation requires the structured-session wire contract"
        );

        // The strict shared path checks actual signed testimony and constructs
        // the binding before returning; it stops before any publication/CAS.
        let captured = capture_source_closure(
            state, engine, &kind, &resolution, &roots, None, None, None,
        )?.context("structured source closure is absent")?;
        let blobs: std::collections::BTreeMap<_, _> = captured.candidate.blobs.iter()
            .map(|blob| (blob.blob_hash.as_str(), blob.bytes.as_slice())).collect();
        let profile = compile_structured_worker_from_blobs(
            &captured.binding, &captured.candidate.manifest,
            |hash, _| blobs.get(hash).map(|bytes| bytes.to_vec())
                .context("captured profile source blob is absent"),
        )?;
        // Literal pins are complete before unrelated product slots. This uses
        // the original resolution without removing or inventing derived data.
        let declarations = ryeos_engine::external_content::external_content_declarations_for_binding(
            &resolution, schema.external_content_contract(),
            ryeos_engine::external_content::declaring_authority(&resolution)?,
        )?;
        let external_content = declarations.as_ref().map(|declarations|
            crate::external_content_admission::preview_external_content_declarations(
                state, engine, &kind, &resolution, &roots,
                &SubjectResolutionAuthority::Projectless, declarations,
            )
        ).transpose()?;
        // Auxiliary profile and helper reads must not hide a changed source
        // tree. Repeat the same bounded, strict pre-CAS capture and testimony
        // under this generation and require the complete original observation.
        // Neither capture publishes; the enclosing generation checks again
        // after this closure returns.
        let current = capture_source_closure(
            state, engine, &kind, &resolution, &roots, None, None, None,
        )?.context("structured source disappeared during observation")?;
        ensure!(
            current.candidate == captured.candidate && current.binding == captured.binding,
            "structured source changed during profile or helper observation"
        );
        let expected_source_content_manifest_hash = match &captured.binding.testimony {
            ryeos_state::objects::SourceTestimonyProof::OwnerSignedDigest { expected_manifest_hash } =>
                Some(expected_manifest_hash.clone()),
            ryeos_state::objects::SourceTestimonyProof::OwnerSignedFiles { .. } => None,
        };
        let response = StructuredSourceValidationObservation {
            schema: "ryeos.structured_source_validation_observation.v1",
            authority: "source_observation_only",
            canonical_ref: item_ref.to_owned(),
            bundle_generation_identity: generation.request_engine_generation_identity().to_owned(),
            signed_effective_definition_digest: resolution.effective_definition_digest()?.as_str().to_owned(),
            publisher_fingerprint: resolution.root.signer_fingerprint.clone().context("trusted source has no publisher")?,
            expected_source_content_manifest_hash,
            candidate_source_content_manifest_hash: captured.candidate.manifest.digest()?,
            candidate_source_binding_hash: captured.binding.digest()?,
            candidate_profile_hash: profile.profile_hash,
            file_count: captured.candidate.manifest.totals.file_count,
            total_bytes: captured.candidate.manifest.totals.total_bytes,
            external_content,
        };
        response.require_bounded()?;
        Ok(response)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::objects::{
        LogicalSourceRoot, SourceClosureFile, SourceClosureManifest, SourceFileMode,
    };

    fn manifest() -> SourceClosureManifest {
        SourceClosureManifest::new(
            vec![LogicalSourceRoot {
                id: "source".into(),
            }],
            vec![SourceClosureFile {
                root: "source".into(),
                path: "profile.json".into(),
                blob_hash: lillux::sha256_hex(b"{}"),
                size: 2,
                mode: SourceFileMode::ReadOnly,
            }],
        )
        .unwrap()
    }

    #[test]
    fn shared_profile_loader_refuses_missing_changed_and_wrong_length_bytes() {
        let manifest = manifest();
        let files = load_profile_source_files(&manifest, |hash, size| {
            assert_eq!(hash, lillux::sha256_hex(b"{}"));
            assert_eq!(size, 2);
            Ok(b"{}".to_vec())
        })
        .unwrap();
        assert_eq!(files["profile.json"], b"{}");
        for changed in [b"[]".as_slice(), b"{} ".as_slice()] {
            assert!(load_profile_source_files(&manifest, |_, _| Ok(changed.to_vec())).is_err());
        }
        assert!(
            load_profile_source_files(&manifest, |_, _| anyhow::bail!("missing captured blob"))
                .is_err()
        );
    }

    #[test]
    fn shared_profile_loader_refuses_cross_root_path_aliases() {
        let original = manifest();
        let mut other = original.entries[0].clone();
        other.root = "other".into();
        let manifest = SourceClosureManifest::new(
            vec![
                LogicalSourceRoot {
                    id: "source".into(),
                },
                LogicalSourceRoot { id: "other".into() },
            ],
            vec![original.entries[0].clone(), other],
        )
        .unwrap();
        assert!(load_profile_source_files(&manifest, |_, _| Ok(b"{}".to_vec())).is_err());
    }

    #[test]
    fn source_observation_bounds_nested_existing_readiness_without_claims() {
        let mut response = StructuredSourceValidationObservation {
            schema: "ryeos.structured_source_validation_observation.v1",
            authority: "source_observation_only",
            canonical_ref: "worker:example/session".into(),
            bundle_generation_identity: "generation".into(),
            signed_effective_definition_digest: "a".repeat(64),
            publisher_fingerprint: "b".repeat(64),
            expected_source_content_manifest_hash: Some("c".repeat(64)),
            candidate_source_content_manifest_hash: "c".repeat(64),
            candidate_source_binding_hash: "d".repeat(64),
            candidate_profile_hash: "e".repeat(64),
            file_count: 1,
            total_bytes: 2,
            external_content: Some(
                crate::external_content_admission::ExternalContentValidationPreview {
                    declarations: vec![],
                    ready_for_admission: false,
                },
            ),
        };
        response.require_bounded().unwrap();
        let value = serde_json::to_value(&response).unwrap();
        assert!(value.get("qualified").is_none());
        assert!(value.get("ready_for_admission").is_none());
        response
            .external_content
            .as_mut()
            .unwrap()
            .declarations
            .push(
                crate::external_content_admission::ExternalContentPinPreview {
                    id: "x".repeat(MAX_OBSERVATION_BYTES),
                    expected_digest: None,
                    observed_digest: None,
                    binding_digest: None,
                    status: "missing".into(),
                },
            );
        assert!(response.require_bounded().is_err());
    }
}

#[cfg(test)]
mod signed_tests;
