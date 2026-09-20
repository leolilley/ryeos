//! Admission wrappers for purpose-owned bundle build and qualification owners.
//!
//! These wrappers do not launch an ambient command and do not possess publisher
//! keys.  A composition root supplies the RyeOS execution owner that produced
//! the retained product/qualification testimony.  This module binds that
//! testimony to the exact release input before it can cross the release API.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
};

use anyhow::{Context as _, bail};
use ryeos_bundle_publication_contract::{BundlePayloadOwnership, BundleTarget};
use ryeos_state::{
    external_content::products::accepted_result::ProductBuildAcceptedResult,
    objects::ExternalContentManifestObject,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::{
    PublicationObjectReader,
    producer::{
        BundleBuildAuthority, BundleQualificationAuthority, GenerationBuildRequest,
        GenerationQualifyRequest, InputInspectRequest, ProducerFuture,
    },
};

const RELEASE_INPUT_SCHEMA: &str = "ryeos.bundle_release_input_plan.v1";
pub const PAYLOAD_OWNERSHIP_ITEM_REF: &str = "config:bundle-release/payload-ownership";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadOwnershipConfigItem {
    pub category: String,
    pub version: String,
    pub description: String,
    pub payload_ownership: BundlePayloadOwnership,
}

impl PayloadOwnershipConfigItem {
    pub fn into_current(self) -> anyhow::Result<BundlePayloadOwnership> {
        anyhow::ensure!(
            self.category == "bundle-release",
            "wrong payload ownership category"
        );
        anyhow::ensure!(
            self.version == "1.0.0",
            "unsupported payload ownership config version"
        );
        anyhow::ensure!(
            !self.description.is_empty(),
            "payload ownership description is empty"
        );
        let value = serde_json::to_value(self.payload_ownership)?;
        BundlePayloadOwnership::from_current_value(&value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedPayload {
    pub bundle: String,
    pub binary: String,
    pub cargo_package: String,
    pub build_class: String,
    pub bundle_sets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedReleaseInput {
    pub schema: String,
    pub project_path: String,
    pub bundle_name: String,
    pub authored_manifest: ryeos_bundle::manifest::BundleManifest,
    pub source_snapshot_hash: String,
    pub predecessor_generation_hash: Option<String>,
    pub target: BundleTarget,
    pub build_profile: String,
    pub payload_ownership_item_ref: String,
    pub payload_ownership_content_hash: String,
    pub payloads: Vec<OwnedPayload>,
    pub cargo_packages: Vec<String>,
    pub build_classes: Vec<String>,
    pub requires_binary_build: bool,
    pub clean_output_required: bool,
    pub ambient_target_reuse_allowed: bool,
}

impl AdmittedReleaseInput {
    pub(crate) fn from_value(value: &Value) -> anyhow::Result<Self> {
        let input: Self = serde_json::from_value(value.clone())?;
        validate_bundle_name(&input.bundle_name)?;
        anyhow::ensure!(
            input.authored_manifest.name == input.bundle_name,
            "authored manifest names another bundle"
        );
        anyhow::ensure!(
            input.schema == RELEASE_INPUT_SCHEMA,
            "unsupported release input schema"
        );
        anyhow::ensure!(
            input.build_profile == "release",
            "unsupported build profile"
        );
        anyhow::ensure!(
            input.payload_ownership_item_ref == PAYLOAD_OWNERSHIP_ITEM_REF,
            "unsupported payload ownership config item"
        );
        anyhow::ensure!(
            input.payload_ownership_content_hash.len() == 64
                && input
                    .payload_ownership_content_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "invalid payload ownership content identity"
        );
        anyhow::ensure!(
            input.clean_output_required && !input.ambient_target_reuse_allowed,
            "release input permits an ambient build output"
        );
        anyhow::ensure!(input.payloads.len() <= 256, "too many owned payloads");
        anyhow::ensure!(
            input.bundle_name != "core",
            "core is substrate-owned and cannot use bundle-only publication"
        );
        anyhow::ensure!(
            input.payloads.iter().all(|p| p.bundle == input.bundle_name),
            "release input contains another bundle's payload"
        );
        anyhow::ensure!(
            input.payloads.windows(2).all(|p| p[0].binary < p[1].binary),
            "owned payloads must be sorted and unique"
        );
        anyhow::ensure!(
            input.payloads.is_empty() == matches!(&input.target, BundleTarget::Portable),
            "data-only bundles require the portable target and native bundles require an exact triple"
        );
        let mut packages: Vec<_> = input
            .payloads
            .iter()
            .map(|p| p.cargo_package.clone())
            .collect();
        packages.sort();
        packages.dedup();
        let mut classes: Vec<_> = input
            .payloads
            .iter()
            .map(|p| p.build_class.clone())
            .collect();
        classes.sort();
        classes.dedup();
        anyhow::ensure!(
            input.cargo_packages == packages && input.build_classes == classes,
            "release input projections disagree with owned payloads"
        );
        anyhow::ensure!(
            input.requires_binary_build == !input.payloads.is_empty(),
            "binary-build flag disagrees with owned payloads"
        );
        Ok(input)
    }

    fn digest(&self) -> anyhow::Result<String> {
        Ok(hex_digest(
            lillux::canonical_json(&serde_json::to_value(self)?)?.as_bytes(),
        ))
    }
}

/// Current source authority. It must prove that `project_path` is the retained
/// tree denoted by `source_snapshot_hash`; pathname existence is insufficient.
pub trait BundleSourceSnapshotAuthority: Send + Sync {
    fn verify_project_snapshot(
        &self,
        project_path: &Path,
        source_snapshot_hash: &str,
    ) -> anyhow::Result<()>;
}

/// Exact authority for a clean Git worktree. The public coordinate is the
/// SHA-256 digest of Git's canonical tar serialization of `HEAD`, not the
/// repository's potentially SHA-1 object id. Dirty/untracked work and gitlinks
/// are rejected so a successful check proves the path read by the build is the
/// same closed source tree named by the request.
#[derive(Debug, Default)]
pub struct CleanGitSourceSnapshotAuthority;

impl CleanGitSourceSnapshotAuthority {
    pub fn snapshot_hash(project_path: &Path) -> anyhow::Result<String> {
        let project_path = project_path.canonicalize()?;
        let status = Command::new("git")
            .args(["status", "--porcelain=v1", "--untracked-files=all"])
            .current_dir(&project_path)
            .output()
            .context("inspect release source worktree")?;
        anyhow::ensure!(
            status.status.success(),
            "release source is not a Git worktree"
        );
        anyhow::ensure!(
            status.stdout.is_empty(),
            "release source worktree is dirty or contains untracked files"
        );
        let ignored = Command::new("git")
            .args([
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ])
            .current_dir(&project_path)
            .output()
            .context("inspect ignored release source files")?;
        anyhow::ensure!(
            ignored.status.success() && ignored.stdout.is_empty(),
            "release source contains ignored files outside the source snapshot; use a clean release worktree"
        );

        let index = Command::new("git")
            .args(["ls-files", "--stage", "-z"])
            .current_dir(&project_path)
            .output()
            .context("inspect release source index")?;
        anyhow::ensure!(
            index.status.success(),
            "could not read release source index"
        );
        anyhow::ensure!(
            !index
                .stdout
                .split(|byte| *byte == 0)
                .any(|record| record.starts_with(b"160000 ")),
            "release source contains a gitlink whose content is outside the snapshot"
        );

        let mut child = Command::new("git")
            .args(["archive", "--format=tar", "HEAD"])
            .current_dir(&project_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start release source snapshot")?;
        let mut digest = Sha256::new();
        let mut stdout = child.stdout.take().context("Git archive has no stdout")?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = stdout.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        let output = child.wait_with_output()?;
        anyhow::ensure!(
            output.status.success(),
            "release source snapshot failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(format!("{:x}", digest.finalize()))
    }
}

impl BundleSourceSnapshotAuthority for CleanGitSourceSnapshotAuthority {
    fn verify_project_snapshot(
        &self,
        project_path: &Path,
        source_snapshot_hash: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            source_snapshot_hash.len() == 64
                && source_snapshot_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
            "invalid source snapshot identity"
        );
        anyhow::ensure!(
            Self::snapshot_hash(project_path)? == source_snapshot_hash,
            "release source does not match the requested snapshot"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AdmittedBuildCoordinate {
    pub release_input: AdmittedReleaseInput,
    pub release_input_digest: String,
}

#[derive(Debug, Clone)]
pub struct AdmittedBuildReceipt {
    pub release_input_digest: String,
    pub accepted_product_result_hash: String,
    pub selected_product_identity: String,
    pub selected_product_witness: String,
    pub input_content_manifest_hash: String,
    /// Exact retained RyeOS execution evidence for the build process.
    pub execution_evidence_hash: String,
}

/// Purpose-owned execution boundary. Implementations receive the resolved
/// ownership list, so they cannot substitute an ambient package selection.
pub trait NativeBundleBuildExecutor: Send + Sync {
    fn build(&self, coordinate: &AdmittedBuildCoordinate) -> anyhow::Result<AdmittedBuildReceipt>;
}

#[derive(Debug, Clone)]
pub struct QualificationCoordinate {
    pub release_input: AdmittedReleaseInput,
    pub release_input_digest: String,
    pub captured_tree_manifest_hash: String,
    pub manifest_item_hash: String,
}

#[derive(Debug, Clone)]
pub struct QualificationReceipt {
    pub release_input_digest: String,
    pub captured_tree_manifest_hash: String,
    pub manifest_item_hash: String,
    /// Attestations emitted by the exact checks actually run.
    pub evidence_hashes: Vec<String>,
    pub provenance_hash: Option<String>,
    pub sbom_hash: Option<String>,
    pub execution_evidence_hash: String,
}

pub trait NativeBundleQualificationExecutor: Send + Sync {
    fn qualify(&self, coordinate: &QualificationCoordinate)
    -> anyhow::Result<QualificationReceipt>;
}

pub struct AdmittedNativeBundleAuthorities {
    ownership: BundlePayloadOwnership,
    objects: lillux::CasStore,
    source: Arc<dyn BundleSourceSnapshotAuthority>,
    build: Arc<dyn NativeBundleBuildExecutor>,
    qualification: Arc<dyn NativeBundleQualificationExecutor>,
}

impl AdmittedNativeBundleAuthorities {
    pub fn new(
        ownership: BundlePayloadOwnership,
        objects: lillux::CasStore,
        source: Arc<dyn BundleSourceSnapshotAuthority>,
        build: Arc<dyn NativeBundleBuildExecutor>,
        qualification: Arc<dyn NativeBundleQualificationExecutor>,
    ) -> anyhow::Result<Self> {
        ownership.validate()?;
        Ok(Self {
            ownership,
            objects,
            source,
            build,
            qualification,
        })
    }

    fn inspect_exact(&self, request: InputInspectRequest) -> anyhow::Result<Value> {
        inspect_release_input(&self.ownership, self.source.as_ref(), request)
    }

    fn build_exact(&self, request: GenerationBuildRequest) -> anyhow::Result<Value> {
        let input = AdmittedReleaseInput::from_value(&request.release_input)?;
        let project = Path::new(&input.project_path);
        self.source
            .verify_project_snapshot(project, &input.source_snapshot_hash)?;
        anyhow::ensure!(
            input.authored_manifest == materialize_release_manifest(project, &input.bundle_name)?,
            "release manifest differs from the exact source snapshot"
        );
        anyhow::ensure!(
            input.payload_ownership_item_ref == PAYLOAD_OWNERSHIP_ITEM_REF,
            "release input names an unsupported payload ownership item"
        );
        anyhow::ensure!(
            self.ownership.content_hash()? == input.payload_ownership_content_hash,
            "payload ownership config changed after inspection"
        );
        let digest = input.digest()?;
        let receipt = self.build.build(&AdmittedBuildCoordinate {
            release_input: input,
            release_input_digest: digest.clone(),
        })?;
        validate_build_receipt(&self.objects, &digest, receipt)
    }

    fn qualify_exact(&self, request: GenerationQualifyRequest) -> anyhow::Result<Value> {
        let input = AdmittedReleaseInput::from_value(&request.release_input)?;
        let digest = input.digest()?;
        let coordinate = QualificationCoordinate {
            release_input: input,
            release_input_digest: digest.clone(),
            captured_tree_manifest_hash: request.captured_tree_manifest_hash,
            manifest_item_hash: request.manifest_item_hash,
        };
        let receipt = self.qualification.qualify(&coordinate)?;
        validate_qualification_receipt(&self.objects, &coordinate, receipt)
    }
}

/// Resolve the only admitted build inputs after proving the requested source
/// snapshot. Kept separate from executors so an authenticated per-call API
/// handler can inspect inputs without retaining `AppState` in an extension.
pub fn inspect_release_input(
    ownership: &BundlePayloadOwnership,
    source: &dyn BundleSourceSnapshotAuthority,
    request: InputInspectRequest,
) -> anyhow::Result<Value> {
    validate_bundle_name(&request.bundle_name)?;
    anyhow::ensure!(
        request.bundle_name != "core",
        "core is substrate-owned and cannot use bundle-only publication"
    );
    let project = PathBuf::from(&request.project_path).canonicalize()?;
    source.verify_project_snapshot(&project, &request.source_snapshot_hash)?;
    ownership.validate()?;
    let payloads = ownership
        .owner(&request.bundle_name)
        .map(|owner| {
            owner
                .payloads
                .iter()
                .map(|payload| OwnedPayload {
                    bundle: owner.bundle_name.clone(),
                    binary: payload.binary.clone(),
                    cargo_package: payload.cargo_package.clone(),
                    build_class: serde_json::to_value(payload.build_class)
                        .expect("payload build class serializes")
                        .as_str()
                        .expect("payload build class is a string")
                        .to_owned(),
                    bundle_sets: owner.bundle_sets.clone(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut packages: Vec<_> = payloads.iter().map(|p| p.cargo_package.clone()).collect();
    packages.sort();
    packages.dedup();
    let mut classes: Vec<_> = payloads.iter().map(|p| p.build_class.clone()).collect();
    classes.sort();
    classes.dedup();
    let target: BundleTarget = serde_json::from_value(request.target)?;
    if !payloads.is_empty() && matches!(target, BundleTarget::Portable) {
        bail!("a native-payload bundle requires an exact target triple");
    }
    let input = AdmittedReleaseInput {
        schema: RELEASE_INPUT_SCHEMA.to_owned(),
        project_path: project.display().to_string(),
        authored_manifest: materialize_release_manifest(&project, &request.bundle_name)?,
        bundle_name: request.bundle_name,
        source_snapshot_hash: request.source_snapshot_hash,
        predecessor_generation_hash: None,
        target,
        build_profile: request.build_profile,
        payload_ownership_item_ref: PAYLOAD_OWNERSHIP_ITEM_REF.to_owned(),
        payload_ownership_content_hash: ownership.content_hash()?,
        requires_binary_build: !payloads.is_empty(),
        clean_output_required: true,
        ambient_target_reuse_allowed: false,
        payloads,
        cargo_packages: packages,
        build_classes: classes,
    };
    AdmittedReleaseInput::from_value(&serde_json::to_value(&input)?)?;
    Ok(serde_json::to_value(input)?)
}

/// Use the same source-manifest generator as bundle packaging. Generated,
/// previously signed manifests are not source authority for a new release.
pub fn materialize_release_manifest(
    project: &Path,
    name: &str,
) -> anyhow::Result<ryeos_bundle::manifest::BundleManifest> {
    validate_bundle_name(name)?;
    let ai_dir = project.join("bundles").join(name).join(".ai");
    let path = ai_dir.join("manifest.source.yaml");
    anyhow::ensure!(
        path.canonicalize()?.starts_with(project.canonicalize()?),
        "manifest source escapes project"
    );
    let metadata = std::fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= 1024 * 1024,
        "unsafe or oversized manifest source"
    );
    let source = serde_yaml::from_slice(&std::fs::read(path)?)?;
    ryeos_bundle::manifest::materialize_manifest(source, &ai_dir, name)
}

fn validate_bundle_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 128
            && (name.as_bytes()[0].is_ascii_lowercase() || name.as_bytes()[0].is_ascii_digit())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "invalid release bundle name"
    );
    Ok(())
}

pub fn validate_build_receipt(
    objects: &impl PublicationObjectReader,
    release_input_digest: &str,
    receipt: AdmittedBuildReceipt,
) -> anyhow::Result<Value> {
    anyhow::ensure!(
        receipt.release_input_digest == release_input_digest,
        "build receipt is for another release input"
    );
    let accepted = ProductBuildAcceptedResult::from_value(&read(
        objects,
        &receipt.accepted_product_result_hash,
    )?)?;
    let selected = accepted
        .products
        .iter()
        .find(|p| p.product_name == receipt.selected_product_identity)
        .context("selected bundle product is absent from accepted build")?;
    anyhow::ensure!(
        selected.witness_hash == receipt.selected_product_witness,
        "selected bundle witness disagrees with accepted build"
    );
    let manifest = ExternalContentManifestObject::from_value(&read(
        objects,
        &receipt.input_content_manifest_hash,
    )?)?;
    super::tree::validate_native_bundle_tree(&manifest)?;
    read(objects, &receipt.execution_evidence_hash)
        .context("build execution evidence is absent")?;
    Ok(
        serde_json::json!({"schema":"ryeos.bundle_generation_build_result.v1",
            "accepted_product_result_hash":receipt.accepted_product_result_hash,
            "selected_product_identity":receipt.selected_product_identity,
            "selected_product_witness":receipt.selected_product_witness,
            "input_content_manifest_hash":receipt.input_content_manifest_hash,
            "build_execution_evidence_hash":receipt.execution_evidence_hash}),
    )
}

pub fn validate_qualification_receipt(
    objects: &impl PublicationObjectReader,
    coordinate: &QualificationCoordinate,
    receipt: QualificationReceipt,
) -> anyhow::Result<Value> {
    anyhow::ensure!(
        receipt.release_input_digest == coordinate.release_input_digest
            && receipt.captured_tree_manifest_hash == coordinate.captured_tree_manifest_hash
            && receipt.manifest_item_hash == coordinate.manifest_item_hash,
        "qualification receipt is for another release coordinate"
    );
    anyhow::ensure!(
        receipt.evidence_hashes.windows(2).all(|p| p[0] < p[1]),
        "qualification evidence must be sorted and unique"
    );
    for hash in receipt
        .evidence_hashes
        .iter()
        .chain(receipt.provenance_hash.iter())
        .chain(receipt.sbom_hash.iter())
        .chain(std::iter::once(&receipt.execution_evidence_hash))
    {
        read(objects, hash).with_context(|| format!("qualification evidence {hash} is absent"))?;
    }
    Ok(
        serde_json::json!({"schema":"ryeos.bundle_generation_qualification_result.v1",
            "evidence_hashes":receipt.evidence_hashes,"provenance_hash":receipt.provenance_hash,
            "sbom_hash":receipt.sbom_hash,"qualification_execution_evidence_hash":receipt.execution_evidence_hash}),
    )
}

impl BundleBuildAuthority for AdmittedNativeBundleAuthorities {
    fn inspect<'a>(&'a self, request: InputInspectRequest) -> ProducerFuture<'a> {
        Box::pin(async move { self.inspect_exact(request) })
    }
    fn build<'a>(&'a self, request: GenerationBuildRequest) -> ProducerFuture<'a> {
        Box::pin(async move { self.build_exact(request) })
    }
}

impl BundleQualificationAuthority for AdmittedNativeBundleAuthorities {
    fn qualify<'a>(&'a self, request: GenerationQualifyRequest) -> ProducerFuture<'a> {
        Box::pin(async move { self.qualify_exact(request) })
    }
}

fn read(objects: &impl PublicationObjectReader, hash: &str) -> anyhow::Result<Value> {
    anyhow::ensure!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid CAS identity"
    );
    objects.get_object(hash)?.context("CAS object is absent")
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod input_contract_tests {
    use super::*;

    #[test]
    fn source_only_bundle_generates_current_manifest() {
        let source = tempfile::tempdir().unwrap();
        let ai = source.path().join("bundles/example/.ai");
        std::fs::create_dir_all(&ai).unwrap();
        std::fs::write(
            ai.join("manifest.source.yaml"),
            "name: example\nversion: 0.1.0\nrequires_kinds: []\n",
        )
        .unwrap();
        let manifest = materialize_release_manifest(source.path(), "example").unwrap();
        assert_eq!(manifest.name, "example");
        assert_eq!(manifest.version, "0.1.0");
        assert!(!ai.join("manifest.yaml").exists());
    }

    #[test]
    fn ignored_files_are_not_admitted_as_snapshot_content() {
        let source = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(source.path())
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        git(&["init", "--quiet"]);
        std::fs::write(source.path().join(".gitignore"), "ignored\n").unwrap();
        git(&["add", ".gitignore"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ]);
        assert!(CleanGitSourceSnapshotAuthority::snapshot_hash(source.path()).is_ok());
        std::fs::write(source.path().join("ignored"), "outside snapshot").unwrap();
        assert!(CleanGitSourceSnapshotAuthority::snapshot_hash(source.path()).is_err());
    }

    #[test]
    fn bundle_name_cannot_escape_source_root() {
        for name in ["", "../web", "/web", "web/other", ".", "-web", "Web"] {
            assert!(validate_bundle_name(name).is_err(), "accepted {name:?}");
        }
        for name in ["web", "central-auth", "3d-tools"] {
            assert!(validate_bundle_name(name).is_ok());
        }
    }

    #[test]
    fn build_input_excludes_inspection_authority_metadata() {
        let input = serde_json::json!({
            "schema": RELEASE_INPUT_SCHEMA,
            "project_path": "/source",
            "bundle_name": "central-auth",
            "authored_manifest": {"name": "central-auth", "version": "0.1.0", "provides_kinds": [], "requires_kinds": []},
            "source_snapshot_hash": "a".repeat(64),
            "predecessor_generation_hash": null,
            "target": {"kind": "portable"},
            "build_profile": "release",
            "payload_ownership_item_ref": PAYLOAD_OWNERSHIP_ITEM_REF,
            "payload_ownership_content_hash": "b".repeat(64),
            "payloads": [], "cargo_packages": [], "build_classes": [],
            "requires_binary_build": false,
            "clean_output_required": true,
            "ambient_target_reuse_allowed": false,
        });
        assert!(AdmittedReleaseInput::from_value(&input).is_ok());
        for field in [
            "trust_epoch",
            "bundle_publication_policy_section_digest",
            "authored_version",
        ] {
            let mut polluted = input.clone();
            polluted[field] = serde_json::json!(1);
            assert!(AdmittedReleaseInput::from_value(&polluted).is_err());
        }
    }
}
