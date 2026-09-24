//! Node-scoped Core manifest signing for authority calibration.
//!
//! This lane exists only to measure the Core producer before a publication
//! catalog exists.  Its input is an already captured Core tree, its source is
//! an exact verified RyeOS ProjectSnapshot, and its only output is domain-separated
//! calibration evidence signed by the node identity.  It deliberately does
//! not implement the release publisher traits and never returns a
//! `SignedBundleTree` or `PublisherMaterializationResult`.

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

pub const CALIBRATION_CORE_MANIFEST_SCHEMA: &str =
    "ryeos.bundle_publication_calibration_core_manifest.v1";
pub const CALIBRATION_CORE_MANIFEST_CLAIM: &str =
    "bundle_publication_authority_calibration_core_manifest_v1";
pub const CALIBRATION_CORE_MANIFEST_POLICY: &str =
    "ryeos.bundle_publication_authority_calibration.v1";
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationCoreManifestRequest {
    pub project_path: String,
    pub source_snapshot_hash: String,
    pub bundle_name: String,
    pub input_content_manifest_hash: String,
}

impl CalibrationCoreManifestRequest {
    pub fn validate(&self) -> anyhow::Result<()> {
        let project = Path::new(&self.project_path);
        anyhow::ensure!(
            project.is_absolute()
                && !project
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir)),
            "calibration Core source must be an absolute path without traversal"
        );
        require_hash("calibration source snapshot", &self.source_snapshot_hash)?;
        require_hash(
            "calibration Core input manifest",
            &self.input_content_manifest_hash,
        )?;
        super::admitted_build::validate_bundle_name(&self.bundle_name)
    }
}

/// The node-authored statement retained for the calibration runner.
///
/// Private fields and purpose-specific accessors prevent this value from being
/// structurally substituted for a bundle release materialization result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationCoreManifestEvidence {
    schema: String,
    evidence_purpose: ProductRecipePurpose,
    source_snapshot_hash: String,
    bundle_name: String,
    node_signer_fingerprint: String,
    input_content_manifest_hash: String,
    output_content_manifest_hash: String,
    output_manifest_item_hash: String,
}

impl CalibrationCoreManifestEvidence {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == CALIBRATION_CORE_MANIFEST_SCHEMA
                && self.evidence_purpose == ProductRecipePurpose::AuthorityCalibrationV1,
            "Core manifest evidence is not authority-calibration evidence"
        );
        for (label, value) in [
            ("calibration source snapshot", &self.source_snapshot_hash),
            ("calibration node signer", &self.node_signer_fingerprint),
            (
                "calibration Core input manifest",
                &self.input_content_manifest_hash,
            ),
            (
                "calibration Core output manifest",
                &self.output_content_manifest_hash,
            ),
            (
                "calibration Core signed manifest item",
                &self.output_manifest_item_hash,
            ),
        ] {
            require_hash(label, value)?;
        }
        super::admitted_build::validate_bundle_name(&self.bundle_name)?;
        anyhow::ensure!(
            self.input_content_manifest_hash != self.output_content_manifest_hash,
            "calibration Core signing did not change the captured tree"
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
            attestation.claim == CALIBRATION_CORE_MANIFEST_CLAIM
                && attestation.policy == CALIBRATION_CORE_MANIFEST_POLICY,
            "attestation is not calibration Core manifest evidence"
        );
        let evidence: Self = serde_json::from_value(attestation.evidence.clone())?;
        evidence.validate()?;
        anyhow::ensure!(
            attestation.subject_hash == evidence.content_hash()?,
            "calibration Core manifest attestation subject changed"
        );
        anyhow::ensure!(
            attestation.issuer_fingerprint()? == evidence.node_signer_fingerprint,
            "calibration Core manifest attestation belongs to another node"
        );
        Ok(evidence)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationCoreManifestResult {
    evidence_attestation_hash: String,
    evidence: CalibrationCoreManifestEvidence,
}

impl CalibrationCoreManifestResult {
    pub fn evidence_attestation_hash(&self) -> &str {
        &self.evidence_attestation_hash
    }

    pub fn evidence(&self) -> &CalibrationCoreManifestEvidence {
        &self.evidence
    }

    pub fn to_value(&self) -> anyhow::Result<serde_json::Value> {
        self.evidence.validate()?;
        Ok(serde_json::json!({
            "schema": CALIBRATION_CORE_MANIFEST_SCHEMA,
            "evidence_attestation_hash": self.evidence_attestation_hash,
            "evidence": self.evidence,
        }))
    }
}

pub struct CalibrationCoreManifestAuthority {
    cas: Arc<lillux::CasStore>,
    node_identity: crate::identity::NodeIdentity,
    source: Arc<dyn BundleSourceSnapshotAuthority>,
}

impl CalibrationCoreManifestAuthority {
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
        request: CalibrationCoreManifestRequest,
    ) -> anyhow::Result<CalibrationCoreManifestResult> {
        request.validate()?;
        self.source.verify_project_snapshot(
            Path::new(&request.project_path),
            &request.source_snapshot_hash,
        )?;
        let source_path = source_core_manifest_path(
            self.source.authoritative_project_root(),
            &request.bundle_name,
        );
        let source_file = lillux::open_pinned_regular_file_no_follow(&source_path)
            .with_context(|| format!("pin source Core manifest at {}", source_path.display()))?;
        let source_observation = source_file.observation()?;
        let source_bytes = source_file.read_stable_bounded(&source_observation, 128 * 1024)?;

        let mut output = ExternalContentManifestObject::from_value(&read_object(
            self.cas.as_ref(),
            &request.input_content_manifest_hash,
        )?)?;
        validate_native_bundle_tree(&output)?;
        let entry = output
            .entries
            .iter_mut()
            .find(|entry| entry.path == ".ai/manifest.yaml")
            .context("calibration Core tree omits .ai/manifest.yaml")?;
        anyhow::ensure!(
            entry.kind == ExternalContentManifestEntryKind::File && entry.mode == Some(0o644),
            "calibration Core manifest must be a portable regular file"
        );
        let input_item_hash = entry
            .blob_hash
            .as_deref()
            .context("calibration Core manifest entry omits its blob")?;
        let input_bytes = self
            .cas
            .get_blob(input_item_hash)?
            .context("calibration Core manifest blob is absent")?;
        let body =
            std::str::from_utf8(&input_bytes).context("calibration Core manifest is not UTF-8")?;
        reject_signed_manifest(body)?;
        require_source_core_manifest(&source_bytes, body)?;
        let manifest: ryeos_bundle::manifest::BundleManifest =
            serde_yaml::from_str(body).context("decode calibration Core manifest")?;
        anyhow::ensure!(
            manifest.name == request.bundle_name,
            "calibration manifest names another bundle"
        );

        let signed =
            lillux::signature::sign_content(body, self.node_identity.signing_key(), "#", None);
        let signed_item = self.cas.put_blob(signed.as_bytes())?;
        let old_size = entry.size.context("calibration Core manifest omits size")?;
        entry.blob_hash = Some(signed_item.hash.clone());
        entry.size = Some(signed.len() as u64);
        output.total_bytes = output
            .total_bytes
            .checked_sub(old_size)
            .and_then(|bytes| bytes.checked_add(signed.len() as u64))
            .context("calibration Core signed tree size overflow")?;
        validate_native_bundle_tree(&output)?;
        let output_manifest = self.cas.put_object(&serde_json::to_value(&output)?)?;

        let evidence = CalibrationCoreManifestEvidence {
            schema: CALIBRATION_CORE_MANIFEST_SCHEMA.to_owned(),
            evidence_purpose: ProductRecipePurpose::AuthorityCalibrationV1,
            source_snapshot_hash: request.source_snapshot_hash,
            bundle_name: request.bundle_name,
            node_signer_fingerprint: self.node_identity.fingerprint().to_owned(),
            input_content_manifest_hash: request.input_content_manifest_hash,
            output_content_manifest_hash: output_manifest.hash,
            output_manifest_item_hash: signed_item.hash,
        };
        evidence.validate()?;
        let attestation = Attestation::unsigned(
            evidence.content_hash()?,
            CALIBRATION_CORE_MANIFEST_CLAIM.to_owned(),
            CALIBRATION_CORE_MANIFEST_POLICY.to_owned(),
            lillux::time::iso8601_now(),
            None,
            serde_json::to_value(&evidence)?,
        )
        .sign(&NodeSigner(&self.node_identity))?;
        let evidence_attestation_hash = self.cas.put_object(&attestation.to_value())?.hash;
        Ok(CalibrationCoreManifestResult {
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
        bail!("calibration Core input manifest is already signed")
    }
    Ok(())
}

/// Bind the unsigned candidate manifest to the exact Core manifest committed
/// by the already verified source snapshot. The source may carry its normal
/// signature envelope; calibration replaces that envelope but cannot replace
/// any authored manifest byte.
fn source_core_manifest_path(project: &Path, bundle_name: &str) -> std::path::PathBuf {
    project
        .join("bundles")
        .join(bundle_name)
        .join(".ai/manifest.yaml")
}

fn require_source_core_manifest(source_bytes: &[u8], candidate: &str) -> anyhow::Result<()> {
    let source_text =
        std::str::from_utf8(source_bytes).context("source Core manifest is not UTF-8")?;
    let (authored, _) =
        lillux::signature::strip_canonical_signature_with_envelope(source_text, "#", None, false)
            .context("source Core manifest has a noncanonical signature envelope")?;
    anyhow::ensure!(
        authored == candidate,
        "calibration Core manifest differs from the verified source snapshot"
    );
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

    #[test]
    fn signs_exact_bundle_into_node_bound_calibration_evidence() {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("source");
        let tree = temporary.path().join("tree");
        fs::create_dir_all(project.join("bundles/core/.ai")).unwrap();
        fs::create_dir_all(tree.join(".ai")).unwrap();
        let checked_in = include_str!("../../../../../bundles/core/.ai/manifest.yaml");
        let unsigned = checked_in.split_once('\n').unwrap().1;
        fs::write(project.join("bundles/core/.ai/manifest.yaml"), checked_in).unwrap();
        fs::write(tree.join(".ai/manifest.yaml"), unsigned).unwrap();

        let cas = Arc::new(lillux::CasStore::new(temporary.path().join("cas")));
        let captured = super::super::tree::capture_bundle_tree(&tree, cas.as_ref()).unwrap();
        let snapshot = "a".repeat(64);
        let source = Arc::new(ExactSource {
            expected_path: project.clone(),
            expected_hash: snapshot.clone(),
            observed: AtomicBool::new(false),
        });
        let identity =
            crate::identity::NodeIdentity::create(&temporary.path().join("node-signing-key.pem"))
                .unwrap();
        let node_fingerprint = identity.fingerprint().to_owned();
        let authority =
            CalibrationCoreManifestAuthority::new(Arc::clone(&cas), identity, source.clone());
        let result = authority
            .sign_and_capture(CalibrationCoreManifestRequest {
                project_path: project.to_string_lossy().into_owned(),
                source_snapshot_hash: snapshot.clone(),
                bundle_name: "core".to_owned(),
                input_content_manifest_hash: captured.manifest_hash().to_owned(),
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
            CalibrationCoreManifestEvidence::from_attestation(&attestation).unwrap(),
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
    }

    #[test]
    fn request_rejects_relative_source_and_noncanonical_hashes() {
        let request = CalibrationCoreManifestRequest {
            project_path: "relative/source".to_owned(),
            source_snapshot_hash: "A".repeat(64),
            bundle_name: "core".to_owned(),
            input_content_manifest_hash: "b".repeat(64),
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn core_manifest_requires_exact_unsigned_source_bytes() {
        let signed = include_str!("../../../../../bundles/core/.ai/manifest.yaml");
        let body = signed.split_once('\n').unwrap().1;
        require_source_core_manifest(signed.as_bytes(), body).unwrap();
        let equivalent = serde_json::to_string(
            &serde_yaml::from_str::<serde_json::Value>(body).unwrap(),
        )
        .unwrap();
        assert!(require_source_core_manifest(signed.as_bytes(), &equivalent).is_err());
    }
}
