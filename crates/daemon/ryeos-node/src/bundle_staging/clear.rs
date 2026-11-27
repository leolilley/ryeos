//! Clearing stages: how an operator frees the staging area.
//!
//! A stage is inert bytes outside every bundle root, so removing one is safe
//! while the node runs; the only thing a clear must not do is interleave with
//! a commit, so it takes the same exclusive area lock `commit` takes. There is
//! no age selector: `staged_at` is the node's own claim and nothing orders by
//! it. An operator names stages by digest, by a refused apply, or all of them.
//!
//! A clear is not an offline operation and leaves no receipt: the ledger is
//! written only by a signed apply.

use std::time::SystemTime;

use lillux::crypto::VerifyingKey;
use serde::Serialize;

use super::{STALE_TEMP_AGE, StagedBundle, StagingArea, StagingRefusal, TEMP_PREFIX};

/// Most digests one clear may name.
pub const MAX_CLEAR_DIGESTS: usize = 64;

/// Which stages a clear removes. The set is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearSelector {
    /// These stages exactly. Every one must exist or nothing is removed.
    Digests(Vec<String>),
    /// Every stage whose latest signed apply was refused.
    ApplyRefused,
    /// Every committed stage, every digest directory with no verifying record,
    /// and every temporary directory old enough to belong to a transfer that is
    /// gone. A younger temporary directory may be a transfer in flight and is
    /// never touched.
    All,
}

/// One stage as the clear reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClearedStage {
    pub tree_digest: String,
    pub name: String,
    pub requested_by: String,
    pub bytes: u64,
}

impl ClearedStage {
    fn of(stage: &StagedBundle) -> Self {
        Self {
            tree_digest: stage.record.tree_digest.clone(),
            name: stage.record.bundle_name.clone(),
            requested_by: stage.record.requested_by.clone(),
            bytes: stage.record.total_bytes,
        }
    }
}

/// What a clear did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Cleared {
    /// Stages with a verified record that were removed.
    pub removed: Vec<ClearedStage>,
    /// Stages with a verified record that stay.
    pub kept: Vec<ClearedStage>,
    /// Directories with no verifying record, or stale temporary directories,
    /// that were removed.
    pub remnants: u64,
}

impl StagingArea {
    /// Remove the stages `selector` names, under the area's exclusive lock.
    /// Everything the selector names is checked before the first removal, so a
    /// refused clear removes nothing.
    pub fn remove_matching(
        &self,
        selector: &ClearSelector,
        node_key: &VerifyingKey,
    ) -> Result<Cleared, StagingRefusal> {
        let digests = match selector {
            ClearSelector::Digests(digests) => {
                if digests.is_empty() || digests.len() > MAX_CLEAR_DIGESTS {
                    return Err(StagingRefusal::Invalid(format!(
                        "a clear names between 1 and {MAX_CLEAR_DIGESTS} stages"
                    )));
                }
                if let Some(bad) = digests.iter().find(|digest| !lillux::valid_hash(digest)) {
                    return Err(StagingRefusal::Invalid(format!(
                        "a stage is named by a SHA-256 digest, not `{bad}`"
                    )));
                }
                Some(digests)
            }
            ClearSelector::ApplyRefused | ClearSelector::All => None,
        };

        let _lock = self
            .directory
            .lock_exclusive()
            .map_err(StagingRefusal::store)?;
        self.directory
            .ensure_path_binding()
            .map_err(StagingRefusal::store)?;

        let held = self.list(node_key)?;
        let mut doomed_stages: Vec<&StagedBundle> = Vec::new();
        let mut doomed_remnants: Vec<String> = Vec::new();
        match selector {
            ClearSelector::Digests(_) => {
                for digest in digests.into_iter().flatten() {
                    if doomed_stages
                        .iter()
                        .any(|stage| &stage.record.tree_digest == digest)
                        || doomed_remnants.contains(digest)
                    {
                        continue;
                    }
                    if let Some(stage) = held
                        .iter()
                        .find(|stage| &stage.record.tree_digest == digest)
                    {
                        doomed_stages.push(stage);
                    } else if self.is_directory(digest) {
                        doomed_remnants.push(digest.clone());
                    } else {
                        return Err(StagingRefusal::NotFound(digest.clone()));
                    }
                }
            }
            ClearSelector::ApplyRefused => {
                doomed_stages.extend(held.iter().filter(|stage| stage.refusal.is_some()));
            }
            ClearSelector::All => {
                doomed_stages.extend(held.iter());
                let now = SystemTime::now();
                let entries = std::fs::read_dir(&self.root).map_err(StagingRefusal::store)?;
                for entry in entries {
                    let entry = entry.map_err(StagingRefusal::store)?;
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        continue;
                    }
                    let remnant = if name.starts_with(TEMP_PREFIX) {
                        entry
                            .metadata()
                            .and_then(|metadata| metadata.modified())
                            .ok()
                            .and_then(|modified| now.duration_since(modified).ok())
                            .is_some_and(|age| age >= STALE_TEMP_AGE)
                    } else {
                        lillux::valid_hash(&name)
                            && !held.iter().any(|stage| stage.record.tree_digest == name)
                    };
                    if remnant {
                        doomed_remnants.push(name);
                    }
                }
            }
        }

        let mut cleared = Cleared::default();
        for stage in &doomed_stages {
            lillux::remove_dir_all_durable(&self.root.join(&stage.record.tree_digest))
                .map_err(StagingRefusal::store)?;
            cleared.removed.push(ClearedStage::of(stage));
        }
        for name in &doomed_remnants {
            lillux::remove_dir_all_durable(&self.root.join(name)).map_err(StagingRefusal::store)?;
            cleared.remnants += 1;
        }
        cleared.kept = held
            .iter()
            .filter(|stage| {
                !doomed_stages
                    .iter()
                    .any(|doomed| doomed.record.tree_digest == stage.record.tree_digest)
            })
            .map(ClearedStage::of)
            .collect();
        Ok(cleared)
    }

    /// Whether `name` is a real directory directly under the area (a link is
    /// not one).
    fn is_directory(&self, name: &str) -> bool {
        std::fs::symlink_metadata(self.root.join(name)).is_ok_and(|meta| meta.file_type().is_dir())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle_staging::apply::ApplyRefusalCode;
    use crate::bundle_staging::{
        DeclaredFacets, SOURCE_REMOTE, STAGING_SCHEMA, StagedSource, StagingCaps, StagingRecord,
        TreeFile, tree_digest,
    };
    use lillux::crypto::SigningKey;
    use std::collections::BTreeMap;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn commit(area: &StagingArea, node: &SigningKey, name: &str, requester: &str) -> String {
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
            requested_by: requester.into(),
            policy_generation: "4".repeat(64),
            declared: DeclaredFacets::default(),
            file_count: 1,
            total_bytes: 1,
            evidence: BTreeMap::new(),
            staged_at: "2026-11-27T00:00:00Z".into(),
        };
        let temp = area.begin().unwrap();
        std::fs::write(temp.tree().join("f.txt"), b"x").unwrap();
        area.commit(temp, record, &[], &StagingCaps::TEST_GENEROUS, node)
            .unwrap();
        digest
    }

    fn names(area: &StagingArea) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(area.root())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    fn open() -> (tempfile::TempDir, StagingArea, SigningKey) {
        let dir = tempfile::tempdir().unwrap();
        let area = StagingArea::open(dir.path()).unwrap();
        (dir, area, key(1))
    }

    #[test]
    fn clearing_by_digest_removes_only_those_stages() {
        let (_dir, area, node) = open();
        let one = commit(&area, &node, "one", "aa");
        let two = commit(&area, &node, "two", "bb");
        let cleared = area
            .remove_matching(
                &ClearSelector::Digests(vec![one.clone()]),
                &node.verifying_key(),
            )
            .unwrap();
        assert_eq!(cleared.removed.len(), 1);
        assert_eq!(cleared.removed[0].tree_digest, one);
        assert_eq!(cleared.removed[0].name, "one");
        assert_eq!(cleared.removed[0].requested_by, "aa");
        assert_eq!(cleared.kept.len(), 1);
        assert_eq!(cleared.kept[0].tree_digest, two);
        assert_eq!(names(&area), vec![two]);
    }

    #[test]
    fn an_unknown_digest_is_typed_and_removes_nothing() {
        let (_dir, area, node) = open();
        let one = commit(&area, &node, "one", "aa");
        let unknown = "e".repeat(64);
        let refused = area
            .remove_matching(
                &ClearSelector::Digests(vec![one.clone(), unknown]),
                &node.verifying_key(),
            )
            .unwrap_err();
        assert_eq!(refused.code(), "bundle_stage_not_found");
        assert_eq!(refused.status(), 404);
        assert_eq!(names(&area), vec![one]);

        let malformed = area
            .remove_matching(
                &ClearSelector::Digests(vec!["not-a-digest".into()]),
                &node.verifying_key(),
            )
            .unwrap_err();
        assert_eq!(malformed.code(), "bundle_stage_invalid");
        let empty = area
            .remove_matching(&ClearSelector::Digests(vec![]), &node.verifying_key())
            .unwrap_err();
        assert_eq!(empty.code(), "bundle_stage_invalid");
        let too_many: Vec<String> = (0..=MAX_CLEAR_DIGESTS)
            .map(|n| format!("{n:064x}"))
            .collect();
        assert!(
            area.remove_matching(&ClearSelector::Digests(too_many), &node.verifying_key())
                .is_err()
        );
    }

    #[test]
    fn clearing_apply_refused_leaves_every_other_stage() {
        let (_dir, area, node) = open();
        let refused = commit(&area, &node, "refused", "aa");
        let fresh = commit(&area, &node, "fresh", "aa");
        area.write_refusal_marker(
            &refused,
            &"d".repeat(64),
            ApplyRefusalCode::TreeDigestMismatch,
            "ee",
            &node,
        )
        .unwrap();
        let cleared = area
            .remove_matching(&ClearSelector::ApplyRefused, &node.verifying_key())
            .unwrap();
        assert_eq!(cleared.removed.len(), 1);
        assert_eq!(cleared.removed[0].tree_digest, refused);
        assert_eq!(names(&area), vec![fresh]);
    }

    #[test]
    fn clearing_all_removes_stages_and_remnants_but_not_a_transfer_in_flight() {
        let (_dir, area, node) = open();
        commit(&area, &node, "one", "aa");
        commit(&area, &node, "two", "bb");
        // A digest directory with no record: a crash remnant.
        let remnant = "c".repeat(64);
        lillux::create_directory_exact_mode(&area.root().join(&remnant), 0o700).unwrap();
        // A temporary directory a day old belongs to a transfer that is gone.
        let stale_dir = area
            .root()
            .join(format!("{}{}", super::TEMP_PREFIX, "d".repeat(32)));
        lillux::create_directory_exact_mode(&stale_dir, 0o700).unwrap();
        std::fs::File::open(&stale_dir)
            .unwrap()
            .set_modified(SystemTime::now() - STALE_TEMP_AGE - std::time::Duration::from_secs(60))
            .unwrap();
        // A fresh temporary directory may be in flight.
        let live = area.begin().unwrap();

        let cleared = area
            .remove_matching(&ClearSelector::All, &node.verifying_key())
            .unwrap();
        assert_eq!(cleared.removed.len(), 2);
        assert!(cleared.kept.is_empty());
        assert_eq!(cleared.remnants, 2);
        assert!(!stale_dir.exists());
        assert!(live.dir().exists(), "a transfer in flight was removed");
        assert_eq!(names(&area).len(), 1);
    }

    #[test]
    fn a_cleared_stage_frees_its_caps() {
        let (_dir, area, node) = open();
        let caps = StagingCaps {
            max_node_bytes: 10,
            max_node_stages: 1,
            max_key_bytes: 10,
            max_key_stages: 1,
        };
        let first = commit(&area, &node, "one", "aa");
        assert!(
            area.check_admission(&caps, "aa", 1, &node.verifying_key())
                .is_err()
        );
        area.remove_matching(&ClearSelector::Digests(vec![first]), &node.verifying_key())
            .unwrap();
        area.check_admission(&caps, "aa", 1, &node.verifying_key())
            .unwrap();
    }

    #[test]
    fn a_clear_and_commits_in_two_threads_leave_only_whole_stages() {
        let (dir, area, node) = open();
        let root = dir.path().to_owned();
        let committer = {
            let node = key(1);
            std::thread::spawn(move || {
                let area = StagingArea::open(&root).unwrap();
                for n in 0..6 {
                    commit(&area, &node, &format!("b{n}"), "aa");
                }
            })
        };
        for _ in 0..6 {
            area.remove_matching(&ClearSelector::All, &node.verifying_key())
                .unwrap();
        }
        committer.join().unwrap();
        let held = area.list(&node.verifying_key()).unwrap();
        for stage in held {
            area.read(&stage.record.tree_digest, &node.verifying_key())
                .unwrap();
        }
    }
}
