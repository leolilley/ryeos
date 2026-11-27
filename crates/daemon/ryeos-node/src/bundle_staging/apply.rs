//! Refusing to apply a stage: the closed refusal codes and the node-signed
//! marker a refused apply leaves in the stage directory.
//!
//! The marker is the node's testimony that a signed apply request named this
//! stage and was refused for one of the closed codes. It is never authority:
//! nothing reads it to decide anything but what an operator is shown. A marker
//! is written only for a stage whose record verifies, and one that does not
//! verify is ignored.

use std::path::Path;

use lillux::crypto::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::{RECORD_FILE, StagingArea, StagingRefusal, VerifiedStage};

/// The marker file inside a stage directory. Reserved: it can never be the
/// name of an evidence file.
pub const REFUSAL_FILE: &str = "apply.refused";
/// Schema tag of the marker body.
pub const REFUSAL_SCHEMA: &str = "ryeos.bundle-staging-refusal/v1";
/// The status a stage with a verified refusal marker reports to an operator.
pub const STATUS_APPLY_REFUSED: &str = "apply_refused";

const MAX_MARKER_BYTES: u64 = 16 * 1024;
/// Most bytes of detail a refusal carries. Never file contents.
pub const MAX_DETAIL_BYTES: usize = 1024;

/// Why applying a stage was refused. The set is closed: a code outside it is
/// not a refusal this node makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyRefusalCode {
    /// No staging area, no stage directory, or no stage by that digest.
    StageNotFound,
    /// The stage record's signature, schema, name or directory does not verify.
    StageRecordUnverified,
    /// An evidence file differs from the digest the signed record holds.
    StageEvidenceMismatch,
    /// The request names another bundle than the stage's record does.
    StageNameMismatch,
    /// The bundle set the request expects is not the one applying would make.
    BundleSetMismatch,
    /// The name is installed and the request did not say to replace it.
    BundleAlreadyInstalled,
    /// The copy of the tree is not one the tree digest accepts.
    TreeInvalid,
    /// The copy of the tree hashes to another digest than the stage's.
    TreeDigestMismatch,
    /// The installed tree no longer hashed to the stage's digest after it was
    /// moved into place.
    TreeChangedDuringInstall,
    /// Signer trust, node configuration or definition admission refused the
    /// bundle as the node stands now.
    AdmissionRefused,
    /// The publication lane's audit of the install's own copy refused the
    /// bundle: the detail starts with the audit's stable code.
    LaneRefused,
    /// The publication evidence the stage carries does not verify again, or
    /// disagrees with the signed record or the tree, or its publisher is no
    /// longer a signer this node trusts, or the record names a source this
    /// build does not know.
    LaneEvidenceInvalid,
    /// The node's `publication_lane` policy could not be loaded as it stands
    /// now. A missing section means the lane is off, never a default.
    LanePolicyUnavailable,
    /// The name may not be taken by a publication stage: it is `core`, it is
    /// held by a native install, or it is held by a laned bundle.
    LaneNameRefused,
}

impl ApplyRefusalCode {
    /// Every code, in the order the documentation lists them.
    pub const ALL: [Self; 14] = [
        Self::StageNotFound,
        Self::StageRecordUnverified,
        Self::StageEvidenceMismatch,
        Self::StageNameMismatch,
        Self::BundleSetMismatch,
        Self::BundleAlreadyInstalled,
        Self::TreeInvalid,
        Self::TreeDigestMismatch,
        Self::TreeChangedDuringInstall,
        Self::AdmissionRefused,
        Self::LaneRefused,
        Self::LaneEvidenceInvalid,
        Self::LanePolicyUnavailable,
        Self::LaneNameRefused,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::StageNotFound => "stage_not_found",
            Self::StageRecordUnverified => "stage_record_unverified",
            Self::StageEvidenceMismatch => "stage_evidence_mismatch",
            Self::StageNameMismatch => "stage_name_mismatch",
            Self::BundleSetMismatch => "bundle_set_mismatch",
            Self::BundleAlreadyInstalled => "bundle_already_installed",
            Self::TreeInvalid => "tree_invalid",
            Self::TreeDigestMismatch => "tree_digest_mismatch",
            Self::TreeChangedDuringInstall => "tree_changed_during_install",
            Self::AdmissionRefused => "admission_refused",
            Self::LaneRefused => "lane_refused",
            Self::LaneEvidenceInvalid => "lane_evidence_invalid",
            Self::LanePolicyUnavailable => "lane_policy_unavailable",
            Self::LaneNameRefused => "lane_name_refused",
        }
    }

    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|known| known.as_str() == code)
    }
}

/// A typed refusal to apply a stage. It travels through `anyhow` chains
/// (context added on the way keeps it findable), so the handler tells a
/// refusal from an environment fault by [`ApplyRefusal::find`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyRefusal {
    code: ApplyRefusalCode,
    detail: String,
}

impl ApplyRefusal {
    pub fn new(code: ApplyRefusalCode, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > MAX_DETAIL_BYTES {
            let mut cut = MAX_DETAIL_BYTES;
            while !detail.is_char_boundary(cut) {
                cut -= 1;
            }
            detail.truncate(cut);
        }
        Self { code, detail }
    }

    pub fn code(&self) -> ApplyRefusalCode {
        self.code
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The refusal an error chain holds, if it holds one.
    pub fn find(error: &anyhow::Error) -> Option<&Self> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

impl std::fmt::Display for ApplyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for ApplyRefusal {}

/// The signed body of a refusal marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyRefusalMarker {
    pub schema: String,
    /// The stage this refusal is about.
    pub tree_digest: String,
    /// The signed request that was refused.
    pub request_digest: String,
    pub code: ApplyRefusalCode,
    /// The key that signed the refused request, without `fp:`.
    pub signer_fingerprint: String,
}

/// Read the marker of the stage in `dir`. `None` when there is none or it does
/// not verify: signed by this node, this schema, naming this stage.
pub(super) fn read_refusal_marker(
    dir: &Path,
    tree_digest: &str,
    node_key: &VerifyingKey,
) -> Option<ApplyRefusalMarker> {
    let path = dir.join(REFUSAL_FILE);
    let file = std::fs::File::open(&path).ok()?;
    let bytes = lillux::read_open_regular_file_bounded(file, MAX_MARKER_BYTES).ok()?;
    let body = ryeos_engine::trust::verify_node_signed_text(&path, &bytes, node_key).ok()?;
    let marker: ApplyRefusalMarker = serde_json::from_str(&body).ok()?;
    (marker.schema == REFUSAL_SCHEMA
        && marker.tree_digest == tree_digest
        && lillux::valid_hash(&marker.request_digest))
    .then_some(marker)
}

impl StagingArea {
    /// Record that a signed apply request was refused, in a node-signed marker
    /// beside the stage's record. A marker is written only for a stage whose
    /// record verifies under `node_key`; for any other directory this fails and
    /// writes nothing. A later refusal replaces an earlier marker.
    pub fn write_refusal_marker(
        &self,
        tree_digest: &str,
        request_digest: &str,
        code: ApplyRefusalCode,
        signer_fingerprint: &str,
        node_key: &SigningKey,
    ) -> Result<ApplyRefusalMarker, StagingRefusal> {
        let _lock = self
            .directory
            .lock_exclusive()
            .map_err(StagingRefusal::store)?;
        self.directory
            .ensure_path_binding()
            .map_err(StagingRefusal::store)?;
        let stage = self.read_record(tree_digest, &node_key.verifying_key())?;
        if !lillux::valid_hash(request_digest) {
            return Err(StagingRefusal::Invalid(
                "a request is named by a SHA-256 digest".into(),
            ));
        }
        let marker = ApplyRefusalMarker {
            schema: REFUSAL_SCHEMA.to_owned(),
            tree_digest: tree_digest.to_owned(),
            request_digest: request_digest.to_owned(),
            code,
            signer_fingerprint: signer_fingerprint.to_owned(),
        };
        let body = serde_json::to_string(&marker).map_err(StagingRefusal::store)?;
        let signed = lillux::signature::sign_content(&body, node_key, "#", None);
        lillux::atomic_write_private(&stage.dir.join(REFUSAL_FILE), signed.as_bytes())
            .map_err(StagingRefusal::store)?;
        Ok(marker)
    }

    /// Read the stage a signed apply request names, as the apply needs it: the
    /// directory is the node user's own and no wider than 0o700, the record is
    /// signed by this node and names this digest, and every evidence file is
    /// the one the record signs. The returned stage carries the evidence bytes
    /// that were verified. Nothing from the record is trusted beyond that.
    pub fn read_for_apply(
        &self,
        tree_digest: &str,
        node_key: &VerifyingKey,
    ) -> Result<VerifiedStage, ApplyRefusal> {
        use ApplyRefusalCode::{StageEvidenceMismatch, StageNotFound, StageRecordUnverified};
        if !lillux::valid_hash(tree_digest) {
            return Err(ApplyRefusal::new(
                StageNotFound,
                "a stage is named by a SHA-256 digest",
            ));
        }
        let dir = self.root.join(tree_digest);
        let observed = match lillux::observe_tree_entry_no_follow(&dir) {
            Ok(observed) => observed,
            Err(_) if !dir.exists() => {
                return Err(ApplyRefusal::new(StageNotFound, "no such stage"));
            }
            Err(error) => {
                return Err(ApplyRefusal::new(
                    StageRecordUnverified,
                    format!("the stage directory cannot be observed: {error:#}"),
                ));
            }
        };
        let uid = lillux::effective_uid().map_err(|error| {
            ApplyRefusal::new(
                StageRecordUnverified,
                format!("the node's user cannot be determined: {error:#}"),
            )
        })?;
        if observed.kind != lillux::TreeEntryKind::Directory
            || observed.owner != uid
            || observed.mode & 0o077 != 0
        {
            return Err(ApplyRefusal::new(
                StageRecordUnverified,
                "the stage directory is not the node user's own private directory",
            ));
        }
        if !dir.join(RECORD_FILE).exists() {
            return Err(ApplyRefusal::new(StageNotFound, "the stage has no record"));
        }
        let bundle = self
            .read_record(tree_digest, node_key)
            .map_err(|error| ApplyRefusal::new(StageRecordUnverified, error.to_string()))?;
        let evidence = bundle
            .verify_evidence()
            .map_err(|error| ApplyRefusal::new(StageEvidenceMismatch, error.to_string()))?;
        Ok(VerifiedStage { bundle, evidence })
    }

    /// The verified refusal marker of a stage, if it has one.
    pub fn refusal_marker(
        &self,
        tree_digest: &str,
        node_key: &VerifyingKey,
    ) -> Option<ApplyRefusalMarker> {
        if !lillux::valid_hash(tree_digest) {
            return None;
        }
        read_refusal_marker(&self.root.join(tree_digest), tree_digest, node_key)
    }

    /// Remove a stage after it was applied. Held under the area's exclusive
    /// lock so a commit cannot interleave. Absent is success.
    pub fn remove(&self, tree_digest: &str) -> Result<(), StagingRefusal> {
        if !lillux::valid_hash(tree_digest) {
            return Err(StagingRefusal::Invalid(
                "a stage is named by a SHA-256 digest".into(),
            ));
        }
        let _lock = self
            .directory
            .lock_exclusive()
            .map_err(StagingRefusal::store)?;
        self.directory
            .ensure_path_binding()
            .map_err(StagingRefusal::store)?;
        lillux::remove_dir_all_durable(&self.root.join(tree_digest)).map_err(StagingRefusal::store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle_staging::{
        DeclaredFacets, SOURCE_REMOTE, STAGING_SCHEMA, StagedSource, StagingCaps, StagingRecord,
        TreeFile, tree_digest,
    };
    use std::collections::BTreeMap;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn committed(area: &StagingArea, node: &SigningKey, name: &str) -> String {
        let digest = tree_digest(
            name,
            &[TreeFile {
                path: "f.txt".into(),
                hash: lillux::sha256_hex(b"x"),
                size: 1,
                mode: 0o644,
            }],
        )
        .unwrap();
        let record = StagingRecord {
            schema: STAGING_SCHEMA.to_owned(),
            bundle_name: name.to_owned(),
            tree_digest: digest.clone(),
            source: StagedSource {
                kind: SOURCE_REMOTE.to_owned(),
                origin: "remote:test".to_owned(),
                publication_record_hash: None,
                pinned_node: None,
                manifest_hash: None,
            },
            requested_by: "aa".into(),
            policy_generation: "4".repeat(64),
            declared: DeclaredFacets::default(),
            file_count: 1,
            total_bytes: 1,
            evidence: BTreeMap::new(),
            staged_at: "2026-11-27T00:00:00Z".into(),
        };
        let temp = area.begin().unwrap();
        std::fs::write(temp.tree().join("f.txt"), b"x").unwrap();
        area.commit(
            temp,
            record,
            &[("note.json".to_owned(), b"{}".to_vec())],
            &StagingCaps::TEST_GENEROUS,
            node,
        )
        .unwrap();
        digest
    }

    #[test]
    fn codes_are_closed_and_round_trip() {
        for code in ApplyRefusalCode::ALL {
            assert_eq!(ApplyRefusalCode::parse(code.as_str()), Some(code));
            let json = serde_json::to_string(&code).unwrap();
            assert_eq!(json, format!("\"{}\"", code.as_str()));
        }
        assert_eq!(ApplyRefusalCode::parse("signers_not_trusted"), None);
        assert_eq!(ApplyRefusalCode::parse("lane_unknown"), None);
    }

    #[test]
    fn a_refusal_survives_context_and_its_detail_is_bounded() {
        let long = "é".repeat(2000);
        let refusal = ApplyRefusal::new(ApplyRefusalCode::TreeInvalid, long);
        assert!(refusal.detail().len() <= MAX_DETAIL_BYTES);
        let error = anyhow::Error::new(refusal.clone())
            .context("admission of the copy")
            .context("install");
        assert_eq!(ApplyRefusal::find(&error), Some(&refusal));
        assert!(ApplyRefusal::find(&anyhow::anyhow!("disk full")).is_none());
    }

    #[test]
    fn the_marker_name_is_not_a_legal_evidence_name() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let digest = tree_digest(
            "demo",
            &[TreeFile {
                path: "f.txt".into(),
                hash: lillux::sha256_hex(b"x"),
                size: 1,
                mode: 0o644,
            }],
        )
        .unwrap();
        let record = StagingRecord {
            schema: STAGING_SCHEMA.to_owned(),
            bundle_name: "demo".into(),
            tree_digest: digest,
            source: StagedSource {
                kind: SOURCE_REMOTE.to_owned(),
                origin: "remote:test".to_owned(),
                publication_record_hash: None,
                pinned_node: None,
                manifest_hash: None,
            },
            requested_by: "aa".into(),
            policy_generation: "4".repeat(64),
            declared: DeclaredFacets::default(),
            file_count: 1,
            total_bytes: 1,
            evidence: BTreeMap::new(),
            staged_at: "2026-11-27T00:00:00Z".into(),
        };
        let temp = area.begin().unwrap();
        std::fs::write(temp.tree().join("f.txt"), b"x").unwrap();
        let refused = area.commit(
            temp,
            record,
            &[(REFUSAL_FILE.to_owned(), b"{}".to_vec())],
            &StagingCaps::TEST_GENEROUS,
            &node,
        );
        assert!(matches!(refused, Err(StagingRefusal::Invalid(_))));
        assert!(area.list(&node.verifying_key()).unwrap().is_empty());
    }

    #[test]
    fn a_marker_is_written_for_a_verified_stage_and_listed() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let digest = committed(&area, &node, "demo");
        let request = "7".repeat(64);
        let marker = area
            .write_refusal_marker(
                &digest,
                &request,
                ApplyRefusalCode::TreeDigestMismatch,
                "aa",
                &node,
            )
            .unwrap();
        assert_eq!(marker.code, ApplyRefusalCode::TreeDigestMismatch);
        let listed = area.list(&node.verifying_key()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].refusal.as_ref(), Some(&marker));
        // The marker is not evidence and does not disturb reading the stage.
        let read = area.read(&digest, &node.verifying_key()).unwrap();
        assert_eq!(read.refusal.as_ref(), Some(&marker));
        assert_eq!(read.evidence.len(), 1);
    }

    #[test]
    fn no_marker_is_written_for_a_stage_whose_record_does_not_verify() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let digest = committed(&area, &node, "demo");
        // Another node's key does not verify the record.
        let other = key(2);
        let refused = area.write_refusal_marker(
            &digest,
            &"7".repeat(64),
            ApplyRefusalCode::AdmissionRefused,
            "aa",
            &other,
        );
        assert!(refused.is_err());
        assert!(!area.root().join(&digest).join(REFUSAL_FILE).exists());
        // A directory with no record at all.
        let bare = "b".repeat(64);
        std::fs::create_dir(area.root().join(&bare)).unwrap();
        assert!(
            area.write_refusal_marker(
                &bare,
                &"7".repeat(64),
                ApplyRefusalCode::StageNotFound,
                "aa",
                &node,
            )
            .is_err()
        );
        assert!(!area.root().join(&bare).join(REFUSAL_FILE).exists());
        // An edited record.
        let record_path = area
            .root()
            .join(&digest)
            .join(crate::bundle_staging::RECORD_FILE);
        let mut bytes = std::fs::read(&record_path).unwrap();
        let at = bytes.iter().position(|b| *b == b'd').unwrap();
        bytes[at] = b'e';
        std::fs::write(&record_path, bytes).unwrap();
        assert!(
            area.write_refusal_marker(
                &digest,
                &"7".repeat(64),
                ApplyRefusalCode::AdmissionRefused,
                "aa",
                &node,
            )
            .is_err()
        );
        assert!(!area.root().join(&digest).join(REFUSAL_FILE).exists());
    }

    #[test]
    fn a_forged_or_misplaced_marker_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let digest = committed(&area, &node, "demo");
        let path = area.root().join(&digest).join(REFUSAL_FILE);
        // Signed by another key.
        let body = serde_json::to_string(&ApplyRefusalMarker {
            schema: REFUSAL_SCHEMA.to_owned(),
            tree_digest: digest.clone(),
            request_digest: "7".repeat(64),
            code: ApplyRefusalCode::TreeInvalid,
            signer_fingerprint: "aa".into(),
        })
        .unwrap();
        let forged = lillux::signature::sign_content(&body, &key(2), "#", None);
        std::fs::write(&path, forged).unwrap();
        assert!(
            area.refusal_marker(&digest, &node.verifying_key())
                .is_none()
        );
        // Signed by the node but naming another stage.
        let other = serde_json::to_string(&ApplyRefusalMarker {
            schema: REFUSAL_SCHEMA.to_owned(),
            tree_digest: "c".repeat(64),
            request_digest: "7".repeat(64),
            code: ApplyRefusalCode::TreeInvalid,
            signer_fingerprint: "aa".into(),
        })
        .unwrap();
        let misplaced = lillux::signature::sign_content(&other, &node, "#", None);
        std::fs::write(&path, misplaced).unwrap();
        assert!(
            area.refusal_marker(&digest, &node.verifying_key())
                .is_none()
        );
        assert!(
            area.list(&node.verifying_key()).unwrap()[0]
                .refusal
                .is_none()
        );
    }

    #[test]
    fn read_for_apply_returns_the_evidence_bytes_it_verified() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let digest = committed(&area, &node, "demo");
        let read = area.read_for_apply(&digest, &node.verifying_key()).unwrap();
        assert_eq!(
            read.evidence.get("note.json").map(Vec::as_slice),
            Some(&b"{}"[..])
        );
        assert_eq!(read.record.bundle_name, "demo");
    }

    #[test]
    fn read_for_apply_names_each_failure_with_its_closed_code() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let code = |digest: &str, key: &SigningKey| {
            area.read_for_apply(digest, &key.verifying_key())
                .unwrap_err()
                .code()
        };
        // No such stage, and a name that is not a digest.
        assert_eq!(
            code(&"d".repeat(64), &node),
            ApplyRefusalCode::StageNotFound
        );
        assert_eq!(code("../x", &node), ApplyRefusalCode::StageNotFound);
        let digest = committed(&area, &node, "demo");
        // Another node's key.
        assert_eq!(
            code(&digest, &key(2)),
            ApplyRefusalCode::StageRecordUnverified
        );
        // A directory with no record.
        let bare = "b".repeat(64);
        std::fs::create_dir(area.root().join(&bare)).unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                area.root().join(&bare),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        assert_eq!(code(&bare, &node), ApplyRefusalCode::StageNotFound);
        // A wider directory.
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(
                area.root().join(&digest),
                std::fs::Permissions::from_mode(0o750),
            )
            .unwrap();
            assert_eq!(
                code(&digest, &node),
                ApplyRefusalCode::StageRecordUnverified
            );
            std::fs::set_permissions(
                area.root().join(&digest),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        // An edited evidence file.
        std::fs::write(area.root().join(&digest).join("note.json"), b"{ }").unwrap();
        assert_eq!(
            code(&digest, &node),
            ApplyRefusalCode::StageEvidenceMismatch
        );
    }

    #[test]
    fn removing_a_stage_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        let node = key(1);
        let digest = committed(&area, &node, "demo");
        area.remove(&digest).unwrap();
        assert!(area.list(&node.verifying_key()).unwrap().is_empty());
        area.remove(&digest).unwrap();
        assert!(area.remove("not-a-digest").is_err());
    }
}
