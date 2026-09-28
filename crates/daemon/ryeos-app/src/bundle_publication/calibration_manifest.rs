//! Node-scoped bundle-manifest signing for authority calibration.
//!
//! Portable/native lanes validate the candidate against the current typed
//! manifest materialized from the exact verified RyeOS ProjectSnapshot. The
//! Core seed lane additionally preserves the exact checked generated manifest
//! body. This exists only to measure producers before a publication catalog
//! exists; it emits domain-separated calibration evidence signed by the node
//! identity and never returns a `SignedBundleTree` or
//! `PublisherMaterializationResult`.

use std::{path::Path, sync::Arc};

use anyhow::{Context as _, bail};
use ryeos_state::{
    external_content::products::ProductRecipePurpose,
    objects::{Attestation, ExternalContentManifestEntryKind, ExternalContentManifestObject},
    signer::Signer,
};
use serde::{Deserialize, Serialize};

use super::{
    PublicationObjectReader, admitted_build::BundleSourceSnapshotAuthority,
    tree::validate_native_bundle_tree,
};

pub const CALIBRATION_MANIFEST_SCHEMA: &str = "ryeos.bundle_publication_calibration_manifest.v1";
pub const CALIBRATION_MANIFEST_CLAIM: &str = "bundle_publication_authority_calibration_manifest_v1";
pub const CALIBRATION_MANIFEST_POLICY: &str = "ryeos.bundle_publication_authority_calibration.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationManifestSource {
    /// Portable/native builders emit a manifest derived from the pinned
    /// manifest.source.yaml and current kind schemas. The checked generated
    /// manifest.yaml is not new-generation authority for these lanes.
    DerivedCurrent,
    /// The Core seed reproduces the pinned generated manifest body exactly,
    /// and that body must also equal the current source-derived manifest.
    CoreSeedGenerated,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationManifestRequest {
    pub project_path: String,
    pub source_snapshot_hash: String,
    pub bundle_name: String,
    pub input_content_manifest_hash: String,
    pub manifest_source: CalibrationManifestSource,
}

impl CalibrationManifestRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        let project = Path::new(&self.project_path);
        anyhow::ensure!(
            project.is_absolute()
                && !project
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir)),
            "calibration bundle source must be an absolute path without traversal"
        );
        require_hash("calibration source snapshot", &self.source_snapshot_hash)?;
        require_hash(
            "calibration bundle input manifest",
            &self.input_content_manifest_hash,
        )?;
        super::admitted_build::validate_bundle_name(&self.bundle_name)?;
        anyhow::ensure!(
            matches!(
                (&self.manifest_source, self.bundle_name.as_str()),
                (CalibrationManifestSource::DerivedCurrent, name) if name != "core"
            ) || matches!(
                (&self.manifest_source, self.bundle_name.as_str()),
                (CalibrationManifestSource::CoreSeedGenerated, "core")
            ),
            "calibration manifest source policy does not match bundle lane"
        );
        Ok(())
    }
}

/// The node-authored statement retained for the calibration runner.
///
/// Private fields and purpose-specific accessors prevent this value from being
/// structurally substituted for a bundle release materialization result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationManifestEvidence {
    schema: String,
    evidence_purpose: ProductRecipePurpose,
    source_snapshot_hash: String,
    bundle_name: String,
    manifest_source: CalibrationManifestSource,
    node_signer_fingerprint: String,
    input_content_manifest_hash: String,
    output_content_manifest_hash: String,
    output_manifest_item_hash: String,
}

impl CalibrationManifestEvidence {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == CALIBRATION_MANIFEST_SCHEMA
                && self.evidence_purpose == ProductRecipePurpose::AuthorityCalibrationV1,
            "bundle manifest evidence is not authority-calibration evidence"
        );
        anyhow::ensure!(
            matches!(
                (&self.manifest_source, self.bundle_name.as_str()),
                (CalibrationManifestSource::DerivedCurrent, name) if name != "core"
            ) || matches!(
                (&self.manifest_source, self.bundle_name.as_str()),
                (CalibrationManifestSource::CoreSeedGenerated, "core")
            ),
            "calibration manifest evidence has a mismatched source policy"
        );
        for (label, value) in [
            ("calibration source snapshot", &self.source_snapshot_hash),
            ("calibration node signer", &self.node_signer_fingerprint),
            (
                "calibration bundle input manifest",
                &self.input_content_manifest_hash,
            ),
            (
                "calibration bundle output manifest",
                &self.output_content_manifest_hash,
            ),
            (
                "calibration signed bundle manifest item",
                &self.output_manifest_item_hash,
            ),
        ] {
            require_hash(label, value)?;
        }
        super::admitted_build::validate_bundle_name(&self.bundle_name)?;
        anyhow::ensure!(
            self.input_content_manifest_hash != self.output_content_manifest_hash,
            "calibration bundle signing did not change the captured tree"
        );
        Ok(())
    }

    pub fn source_snapshot_hash(&self) -> &str {
        &self.source_snapshot_hash
    }

    pub fn bundle_name(&self) -> &str {
        &self.bundle_name
    }

    pub fn evidence_purpose(&self) -> ProductRecipePurpose {
        self.evidence_purpose
    }

    pub fn node_signer_fingerprint(&self) -> &str {
        &self.node_signer_fingerprint
    }

    pub fn input_content_manifest_hash(&self) -> &str {
        &self.input_content_manifest_hash
    }

    pub fn output_content_manifest_hash(&self) -> &str {
        &self.output_content_manifest_hash
    }

    pub fn output_manifest_item_hash(&self) -> &str {
        &self.output_manifest_item_hash
    }

    fn content_hash(&self) -> anyhow::Result<String> {
        self.validate()?;
        ryeos_state::objects::canonical_value_digest(&serde_json::to_value(self)?)
    }

    pub fn from_attestation(attestation: &Attestation) -> anyhow::Result<Self> {
        anyhow::ensure!(
            attestation.claim == CALIBRATION_MANIFEST_CLAIM
                && attestation.policy == CALIBRATION_MANIFEST_POLICY,
            "attestation is not bundle-manifest calibration evidence"
        );
        let evidence: Self = serde_json::from_value(attestation.evidence.clone())?;
        evidence.validate()?;
        anyhow::ensure!(
            attestation.subject_hash == evidence.content_hash()?,
            "calibration bundle manifest attestation subject changed"
        );
        anyhow::ensure!(
            attestation.issuer_fingerprint()? == evidence.node_signer_fingerprint,
            "calibration bundle manifest attestation belongs to another node"
        );
        Ok(evidence)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationManifestResult {
    evidence_attestation_hash: String,
    evidence: CalibrationManifestEvidence,
}

impl CalibrationManifestResult {
    pub fn evidence_attestation_hash(&self) -> &str {
        &self.evidence_attestation_hash
    }

    pub fn evidence(&self) -> &CalibrationManifestEvidence {
        &self.evidence
    }

    pub fn to_value(&self) -> anyhow::Result<serde_json::Value> {
        self.evidence.validate()?;
        Ok(serde_json::json!({
            "schema": CALIBRATION_MANIFEST_SCHEMA,
            "evidence_attestation_hash": self.evidence_attestation_hash,
            "evidence": self.evidence,
        }))
    }
}

pub struct CalibrationManifestAuthority {
    cas: Arc<lillux::CasStore>,
    node_identity: crate::identity::NodeIdentity,
    source: Arc<dyn BundleSourceSnapshotAuthority>,
}

impl CalibrationManifestAuthority {
    pub fn new(
        cas: Arc<lillux::CasStore>,
        node_identity: crate::identity::NodeIdentity,
        source: Arc<dyn BundleSourceSnapshotAuthority>,
    ) -> Self {
        Self {
            cas,
            node_identity,
            source,
        }
    }

    pub fn sign_and_capture(
        &self,
        request: CalibrationManifestRequest,
    ) -> anyhow::Result<CalibrationManifestResult> {
        request.validate()?;
        self.source.verify_project_snapshot(
            Path::new(&request.project_path),
            &request.source_snapshot_hash,
        )?;
        let source_root = self.source.authoritative_project_root();

        let mut output = ExternalContentManifestObject::from_value(&read_object(
            self.cas.as_ref(),
            &request.input_content_manifest_hash,
        )?)?;
        validate_native_bundle_tree(&output)?;
        let entry = output
            .entries
            .iter_mut()
            .find(|entry| entry.path == ".ai/manifest.yaml")
            .context("calibration bundle tree omits .ai/manifest.yaml")?;
        anyhow::ensure!(
            entry.kind == ExternalContentManifestEntryKind::File && entry.mode == Some(0o644),
            "calibration bundle manifest must be a portable regular file"
        );
        let input_item_hash = entry
            .blob_hash
            .as_deref()
            .context("calibration bundle manifest entry omits its blob")?;
        let input_bytes = self
            .cas
            .get_blob(input_item_hash)?
            .context("calibration bundle manifest blob is absent")?;
        let body = std::str::from_utf8(&input_bytes)
            .context("calibration bundle manifest is not UTF-8")?;
        reject_signed_manifest(body)?;
        validate_candidate_manifest(
            source_root,
            &request.bundle_name,
            request.manifest_source,
            body,
        )?;

        let signed =
            lillux::signature::sign_content(body, self.node_identity.signing_key(), "#", None);
        let signed_item = self.cas.put_blob(signed.as_bytes())?;
        let old_size = entry
            .size
            .context("calibration bundle manifest omits size")?;
        entry.blob_hash = Some(signed_item.hash.clone());
        entry.size = Some(signed.len() as u64);
        output.total_bytes = output
            .total_bytes
            .checked_sub(old_size)
            .and_then(|bytes| bytes.checked_add(signed.len() as u64))
            .context("calibration signed bundle tree size overflow")?;
        validate_native_bundle_tree(&output)?;
        let output_manifest = self.cas.put_object(&serde_json::to_value(&output)?)?;

        let evidence = CalibrationManifestEvidence {
            schema: CALIBRATION_MANIFEST_SCHEMA.to_owned(),
            evidence_purpose: ProductRecipePurpose::AuthorityCalibrationV1,
            source_snapshot_hash: request.source_snapshot_hash,
            bundle_name: request.bundle_name,
            manifest_source: request.manifest_source,
            node_signer_fingerprint: self.node_identity.fingerprint().to_owned(),
            input_content_manifest_hash: request.input_content_manifest_hash,
            output_content_manifest_hash: output_manifest.hash,
            output_manifest_item_hash: signed_item.hash,
        };
        evidence.validate()?;
        let attestation = Attestation::unsigned(
            evidence.content_hash()?,
            CALIBRATION_MANIFEST_CLAIM.to_owned(),
            CALIBRATION_MANIFEST_POLICY.to_owned(),
            lillux::time::iso8601_now(),
            None,
            serde_json::to_value(&evidence)?,
        )
        .sign(&NodeSigner(&self.node_identity))?;
        let evidence_attestation_hash = self.cas.put_object(&attestation.to_value())?.hash;
        Ok(CalibrationManifestResult {
            evidence_attestation_hash,
            evidence,
        })
    }
}

struct NodeSigner<'a>(&'a crate::identity::NodeIdentity);

impl Signer for NodeSigner<'_> {
    fn fingerprint(&self) -> &str {
        self.0.fingerprint()
    }

    fn sign(&self, data: &[u8]) -> Vec<u8> {
        use lillux::crypto::Signer as _;
        self.0.signing_key().sign(data).to_bytes().to_vec()
    }

    fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
        *self.0.verifying_key()
    }
}

fn read_object(
    reader: &impl PublicationObjectReader,
    hash: &str,
) -> anyhow::Result<serde_json::Value> {
    reader
        .get_object(hash)?
        .with_context(|| format!("calibration object {hash} is absent"))
}

fn reject_signed_manifest(body: &str) -> anyhow::Result<()> {
    if body
        .lines()
        .next()
        .is_some_and(|line| lillux::signature::parse_signature_line(line, "#", None).is_some())
    {
        bail!("calibration bundle input manifest is already signed")
    }
    Ok(())
}

/// Resolve the checked generated Core manifest only for the Core seed lane.
/// Portable/native candidates instead derive from manifest.source.yaml.
fn source_generated_manifest_path(project: &Path, bundle_name: &str) -> std::path::PathBuf {
    project
        .join("bundles")
        .join(bundle_name)
        .join(".ai/manifest.yaml")
}

fn require_generated_core_manifest(source_bytes: &[u8], candidate: &str) -> anyhow::Result<()> {
    let source_text =
        std::str::from_utf8(source_bytes).context("generated Core manifest is not UTF-8")?;
    let (authored, _) =
        lillux::signature::strip_canonical_signature_with_envelope(source_text, "#", None, false)
            .context("generated Core manifest has a noncanonical signature envelope")?;
    anyhow::ensure!(
        authored == candidate,
        "calibration Core seed differs from the exact generated manifest in the source snapshot"
    );
    Ok(())
}

fn validate_candidate_manifest(
    project: &Path,
    bundle_name: &str,
    manifest_source: CalibrationManifestSource,
    candidate: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(
            (&manifest_source, bundle_name),
            (CalibrationManifestSource::DerivedCurrent, name) if name != "core"
        ) || matches!(
            (&manifest_source, bundle_name),
            (CalibrationManifestSource::CoreSeedGenerated, "core")
        ),
        "calibration manifest source policy does not match bundle lane"
    );
    let expected = super::admitted_build::materialize_release_manifest(project, bundle_name)?;
    let candidate_manifest: ryeos_bundle::manifest::BundleManifest =
        serde_yaml::from_str(candidate).context("decode calibration bundle manifest")?;
    anyhow::ensure!(
        candidate_manifest == expected,
        "calibration {bundle_name} manifest differs from current manifest.source.yaml materialization"
    );
    if manifest_source == CalibrationManifestSource::CoreSeedGenerated {
        let source_path = source_generated_manifest_path(project, bundle_name);
        let source_file = lillux::open_pinned_regular_file_no_follow(&source_path)
            .with_context(|| format!("pin generated Core manifest at {}", source_path.display()))?;
        let source_observation = source_file.observation()?;
        let source_bytes = source_file.read_stable_bounded(&source_observation, 128 * 1024)?;
        require_generated_core_manifest(&source_bytes, candidate)?;
    }
    Ok(())
}

fn require_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{label} must be a lowercase sha256 digest")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicBool, Ordering},
    };

    struct ExactSource {
        expected_path: std::path::PathBuf,
        expected_hash: String,
        observed: AtomicBool,
    }

    impl BundleSourceSnapshotAuthority for ExactSource {
        fn authoritative_project_root(&self) -> &Path {
            &self.expected_path
        }

        fn verify_project_snapshot(
            &self,
            project_path: &Path,
            source_snapshot_hash: &str,
        ) -> anyhow::Result<()> {
            anyhow::ensure!(project_path == self.expected_path);
            anyhow::ensure!(source_snapshot_hash == self.expected_hash);
            self.observed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn write_source_manifest(project: &Path, bundle_name: &str) -> String {
        let ai = project.join("bundles").join(bundle_name).join(".ai");
        fs::create_dir_all(&ai).unwrap();
        fs::write(
            ai.join("manifest.source.yaml"),
            format!("name: {bundle_name}\nversion: \"1.0.0\"\nrequires_kinds: []\n"),
        )
        .unwrap();
        fs::read_to_string(ai.join("manifest.source.yaml")).unwrap()
    }

    fn assert_signed_output_manifest_body(
        cas: &lillux::CasStore,
        result: &CalibrationManifestResult,
        expected_body: &str,
        identity: &crate::identity::NodeIdentity,
    ) {
        let output = ExternalContentManifestObject::from_value(
            &cas.get_object(result.evidence().output_content_manifest_hash())
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let entry = output
            .entries
            .iter()
            .find(|entry| entry.path == ".ai/manifest.yaml")
            .unwrap();
        let signed_item_hash = entry.blob_hash.as_deref().unwrap();
        assert_eq!(
            signed_item_hash,
            result.evidence().output_manifest_item_hash()
        );
        let signed_bytes = cas.get_blob(signed_item_hash).unwrap().unwrap();
        let signed_text = std::str::from_utf8(&signed_bytes).unwrap();
        let (body, header) = lillux::signature::strip_canonical_signature_with_envelope(
            signed_text,
            "#",
            None,
            false,
        )
        .unwrap();
        assert_eq!(body, expected_body);
        let header = header.expect("node-signed output manifest has a signature header");
        assert!(lillux::signature::is_valid_signature_for(
            &header.content_hash,
            &header.signature_b64,
            &header.signer_fingerprint,
            &body,
            identity.verifying_key(),
            identity.fingerprint(),
        ));
    }

    #[test]
    fn signs_validated_core_bundle_into_node_bound_calibration_evidence() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("source");
        let tree = temporary.path().join("tree");
        write_source_manifest(&project, "core");
        fs::create_dir_all(tree.join(".ai")).unwrap();
        let identity =
            crate::identity::NodeIdentity::create(&temporary.path().join("node-signing-key.pem"))
                .unwrap();
        let expected =
            super::super::admitted_build::materialize_release_manifest(&project, "core").unwrap();
        let unsigned = serde_yaml::to_string(&expected).unwrap();
        let signed_source =
            lillux::signature::sign_content(&unsigned, identity.signing_key(), "#", None);
        fs::write(
            project.join("bundles/core/.ai/manifest.yaml"),
            signed_source,
        )
        .unwrap();
        fs::write(tree.join(".ai/manifest.yaml"), &unsigned).unwrap();

        let cas = Arc::new(lillux::CasStore::new(temporary.path().join("cas")));
        let captured = super::super::tree::capture_bundle_tree(&tree, cas.as_ref()).unwrap();
        let snapshot = "a".repeat(64);
        let source = Arc::new(ExactSource {
            expected_path: project.clone(),
            expected_hash: snapshot.clone(),
            observed: AtomicBool::new(false),
        });
        let node_fingerprint = identity.fingerprint().to_owned();
        let authority =
            CalibrationManifestAuthority::new(Arc::clone(&cas), identity, source.clone());
        let result = authority
            .sign_and_capture(CalibrationManifestRequest {
                project_path: project.to_string_lossy().into_owned(),
                source_snapshot_hash: snapshot.clone(),
                bundle_name: "core".to_owned(),
                input_content_manifest_hash: captured.manifest_hash().to_owned(),
                manifest_source: CalibrationManifestSource::CoreSeedGenerated,
            })
            .unwrap();

        assert!(source.observed.load(Ordering::SeqCst));
        assert_eq!(result.evidence().source_snapshot_hash(), snapshot);
        assert_eq!(
            result.evidence().node_signer_fingerprint(),
            node_fingerprint
        );
        assert_ne!(
            result.evidence().input_content_manifest_hash(),
            result.evidence().output_content_manifest_hash()
        );
        assert_signed_output_manifest_body(
            cas.as_ref(),
            &result,
            &unsigned,
            &authority.node_identity,
        );
        let attestation = Attestation::from_value(
            &cas.get_object(result.evidence_attestation_hash())
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        attestation
            .verify_with_key(authority.node_identity.verifying_key())
            .unwrap();
        assert_eq!(
            CalibrationManifestEvidence::from_attestation(&attestation).unwrap(),
            result.evidence
        );
        assert_ne!(
            attestation.claim,
            super::super::attestation::BUNDLE_GENERATION_RELEASE_CLAIM
        );
        assert!(
            serde_json::from_value::<
                ryeos_bundle_publication_contract::PublisherMaterializationResult,
            >(serde_json::to_value(result.evidence()).unwrap())
            .is_err()
        );
        let mut mismatched_evidence = result.evidence().clone();
        mismatched_evidence.manifest_source = CalibrationManifestSource::DerivedCurrent;
        assert!(mismatched_evidence.validate().is_err());
    }

    #[test]
    fn request_rejects_relative_source_and_noncanonical_hashes() {
        let request = CalibrationManifestRequest {
            project_path: "relative/source".to_owned(),
            source_snapshot_hash: "A".repeat(64),
            bundle_name: "core".to_owned(),
            input_content_manifest_hash: "b".repeat(64),
            manifest_source: CalibrationManifestSource::CoreSeedGenerated,
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn request_rejects_manifest_source_from_another_lane() {
        let request = |bundle_name: &str, manifest_source| CalibrationManifestRequest {
            project_path: "/verified/project".to_owned(),
            source_snapshot_hash: "a".repeat(64),
            bundle_name: bundle_name.to_owned(),
            input_content_manifest_hash: "b".repeat(64),
            manifest_source,
        };
        assert!(
            request("core", CalibrationManifestSource::DerivedCurrent)
                .validate()
                .is_err()
        );
        assert!(
            request("web", CalibrationManifestSource::CoreSeedGenerated)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn derived_lanes_compare_against_source_materialization_not_generated_yaml() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        write_source_manifest(project, "web");
        fs::write(
            project.join("bundles/web/.ai/manifest.yaml"),
            "stale generated bytes are not authority\n",
        )
        .unwrap();
        let expected =
            super::super::admitted_build::materialize_release_manifest(project, "web").unwrap();
        let canonical_json = format!(
            "{}\n",
            serde_json::to_string(&serde_json::to_value(&expected).unwrap()).unwrap()
        );

        validate_candidate_manifest(
            project,
            "web",
            CalibrationManifestSource::DerivedCurrent,
            &canonical_json,
        )
        .unwrap();

        let tree = temporary.path().join("portable-tree");
        fs::create_dir_all(tree.join(".ai")).unwrap();
        fs::write(tree.join(".ai/manifest.yaml"), &canonical_json).unwrap();
        let identity =
            crate::identity::NodeIdentity::create(&temporary.path().join("derived-node.pem"))
                .unwrap();
        let cas = Arc::new(lillux::CasStore::new(temporary.path().join("derived-cas")));
        let captured = super::super::tree::capture_bundle_tree(&tree, cas.as_ref()).unwrap();
        let snapshot = "c".repeat(64);
        let source = Arc::new(ExactSource {
            expected_path: project.to_path_buf(),
            expected_hash: snapshot.clone(),
            observed: AtomicBool::new(false),
        });
        let authority =
            CalibrationManifestAuthority::new(Arc::clone(&cas), identity, source.clone());
        let result = authority
            .sign_and_capture(CalibrationManifestRequest {
                project_path: project.to_string_lossy().into_owned(),
                source_snapshot_hash: snapshot,
                bundle_name: "web".to_owned(),
                input_content_manifest_hash: captured.manifest_hash().to_owned(),
                manifest_source: CalibrationManifestSource::DerivedCurrent,
            })
            .unwrap();
        assert!(source.observed.load(Ordering::SeqCst));
        assert_signed_output_manifest_body(
            cas.as_ref(),
            &result,
            &canonical_json,
            &authority.node_identity,
        );
        let mut mismatched_evidence = result.evidence().clone();
        mismatched_evidence.manifest_source = CalibrationManifestSource::CoreSeedGenerated;
        assert!(mismatched_evidence.validate().is_err());

        let mut changed = expected;
        changed.version = "9.9.9".to_owned();
        let changed_json = format!(
            "{}\n",
            serde_json::to_string(&serde_json::to_value(&changed).unwrap()).unwrap()
        );
        assert!(
            validate_candidate_manifest(
                project,
                "web",
                CalibrationManifestSource::DerivedCurrent,
                &changed_json,
            )
            .is_err()
        );
    }

    #[test]
    fn core_manifest_requires_both_current_semantics_and_exact_generated_body() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path();
        write_source_manifest(project, "core");
        let expected =
            super::super::admitted_build::materialize_release_manifest(project, "core").unwrap();
        let body = serde_yaml::to_string(&expected).unwrap();
        let identity =
            crate::identity::NodeIdentity::create(&temporary.path().join("source-key.pem"))
                .unwrap();
        let signed = lillux::signature::sign_content(&body, identity.signing_key(), "#", None);
        fs::write(project.join("bundles/core/.ai/manifest.yaml"), signed).unwrap();

        validate_candidate_manifest(
            project,
            "core",
            CalibrationManifestSource::CoreSeedGenerated,
            &body,
        )
        .unwrap();

        let semantically_equivalent = format!("{body}\n");
        assert!(
            validate_candidate_manifest(
                project,
                "core",
                CalibrationManifestSource::CoreSeedGenerated,
                &semantically_equivalent,
            )
            .is_err()
        );

        let mut changed = expected;
        changed.version = "9.9.9".to_owned();
        let changed_body = serde_yaml::to_string(&changed).unwrap();
        assert!(
            validate_candidate_manifest(
                project,
                "core",
                CalibrationManifestSource::CoreSeedGenerated,
                &changed_body,
            )
            .is_err()
        );
    }
}
