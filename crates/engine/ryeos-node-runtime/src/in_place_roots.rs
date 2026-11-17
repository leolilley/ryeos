//! Import roots a node holds large files in place in: measured at node
//! start, reported by the capability report, and resolved for the large-object
//! store.
//!
//! Signed node policy names the roots and the proof grades each accepts. At
//! start the node measures every such root through Lillux and keeps what it
//! found: a root is usable only while its measurement meets an accepted grade
//! and it is still the directory policy admitted. The store asks this
//! resolver at every lease and residency answer, so a root whose proof does
//! not hold, or whose identity changed, refuses its objects by name until
//! they are imported again. Nothing here is trusted from an earlier boot.

use std::collections::BTreeMap;
use std::path::PathBuf;

use lillux::process_control::{
    CapabilityEvidence, ImmutableRootFacts, admit_immutable_root_proof, decide_immutable_root,
};
use ryeos_state::{ReferenceRefusalCode, ReferenceRoot, ReferenceRootRefusal, ReferenceRoots};
use serde::{Deserialize, Serialize};

use crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord;

/// What the node measured about one in-place import root at start, for the
/// capability report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InPlaceRootReport {
    pub path: String,
    /// The grades the root's declaration accepts; empty means measured only.
    pub accept: Vec<String>,
    pub facts: ImmutableRootFacts,
    /// The proof the facts decide, when they decide one.
    pub proof: Option<CapabilityEvidence>,
    /// Why the root is not usable now: no proof, a grade the declaration
    /// does not accept, or an identity that is no longer the admitted one.
    /// `None` when the root is usable.
    pub refusal: Option<String>,
}

#[derive(Debug, Clone)]
enum RootState {
    Usable {
        path: PathBuf,
        identity: lillux::PinnedDirectoryIdentity,
    },
    Refused {
        code: ReferenceRefusalCode,
        detail: String,
    },
}

/// The node's resolver of in-place roots, built from one start-time
/// measurement.
#[derive(Debug, Clone)]
pub struct InPlaceRoots {
    roots: BTreeMap<String, RootState>,
}

impl InPlaceRoots {
    /// Measure every root `policy` declares in place. Returns the resolver
    /// and the report of what was found for each.
    pub fn measure(
        policy: &ExternalContentImportPolicyRecord,
    ) -> (Self, BTreeMap<String, InPlaceRootReport>) {
        let mut roots = BTreeMap::new();
        let mut reports = BTreeMap::new();
        for (name, root) in &policy.roots {
            let Some(in_place) = &root.in_place else {
                continue;
            };
            let facts = ImmutableRootFacts::observe(&root.path);
            let decided = decide_immutable_root(&facts);
            let (proof, mut refusal) = match &decided {
                Ok(evidence) => (Some(evidence.clone()), None),
                Err(reason) => (None, Some(reason.clone())),
            };
            let mut state = match (&decided, &refusal) {
                (Ok(evidence), _) => match admit_immutable_root_proof(&in_place.accept, evidence) {
                    Ok(()) => RootState::Usable {
                        path: root.path.clone(),
                        identity: root.root_identity,
                    },
                    Err(reason) => {
                        refusal = Some(reason.clone());
                        RootState::Refused {
                            code: ReferenceRefusalCode::RootUnproven,
                            detail: reason,
                        }
                    }
                },
                (Err(reason), _) => RootState::Refused {
                    code: ReferenceRefusalCode::RootUnproven,
                    detail: reason.clone(),
                },
            };
            if matches!(state, RootState::Usable { .. }) {
                let current = lillux::PinnedDirectory::open(&root.path)
                    .ok()
                    .flatten()
                    .and_then(|directory| directory.identity().ok());
                if current != Some(root.root_identity) {
                    let reason = format!(
                        "the directory at {} is no longer the one node policy admitted",
                        root.path.display()
                    );
                    refusal = Some(reason.clone());
                    state = RootState::Refused {
                        code: ReferenceRefusalCode::RootChanged,
                        detail: reason,
                    };
                }
            }
            roots.insert(name.clone(), state);
            reports.insert(
                name.clone(),
                InPlaceRootReport {
                    path: root.path.display().to_string(),
                    accept: in_place.accept.clone(),
                    facts,
                    proof,
                    refusal,
                },
            );
        }
        (Self { roots }, reports)
    }
}

impl ReferenceRoots for InPlaceRoots {
    fn open_root(&self, name: &str) -> Result<ReferenceRoot, ReferenceRootRefusal> {
        let refuse = |code, detail: String| ReferenceRootRefusal { code, detail };
        match self.roots.get(name) {
            None => Err(refuse(
                ReferenceRefusalCode::RootUnproven,
                format!("root `{name}` is not declared for use in place by node policy"),
            )),
            Some(RootState::Refused { code, detail }) => {
                Err(refuse(*code, format!("root `{name}`: {detail}")))
            }
            Some(RootState::Usable { path, identity }) => {
                let directory = lillux::PinnedDirectory::open(path)
                    .map_err(|error| {
                        refuse(
                            ReferenceRefusalCode::RootChanged,
                            format!("root `{name}` cannot be opened: {error:#}"),
                        )
                    })?
                    .ok_or_else(|| {
                        refuse(
                            ReferenceRefusalCode::RootChanged,
                            format!("root `{name}` is absent at {}", path.display()),
                        )
                    })?;
                let current = directory.identity().map_err(|error| {
                    refuse(
                        ReferenceRefusalCode::RootChanged,
                        format!("root `{name}` cannot be measured: {error:#}"),
                    )
                })?;
                if current != *identity {
                    return Err(refuse(
                        ReferenceRefusalCode::RootChanged,
                        format!("root `{name}` is no longer the directory node policy admitted"),
                    ));
                }
                Ok(ReferenceRoot {
                    directory,
                    identity: current,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_policy::sections::external_content::{
        ExternalContentImportLimits, ExternalContentImportRoot, InPlaceRootPolicy,
        ManagedExternalContentPolicy,
    };

    fn policy_with(
        name: &str,
        path: &std::path::Path,
        in_place: Option<InPlaceRootPolicy>,
    ) -> ExternalContentImportPolicyRecord {
        let identity = lillux::PinnedDirectory::open(path)
            .unwrap()
            .unwrap()
            .identity()
            .unwrap();
        ExternalContentImportPolicyRecord {
            schema: crate::node_policy::sections::external_content::EXTERNAL_CONTENT_POLICY_SCHEMA,
            roots: BTreeMap::from([(
                name.to_owned(),
                ExternalContentImportRoot {
                    path: path.to_path_buf(),
                    root_identity: identity,
                    in_place,
                },
            )]),
            declared_roots: BTreeMap::new(),
            limits: ExternalContentImportLimits {
                max_depth: 8,
                max_entries: 8,
                max_file_bytes: 1024,
                max_total_bytes: 1024,
                store_budget_bytes: 2048,
                minimum_free_bytes: 1024,
            },
            managed_activation: ManagedExternalContentPolicy {
                enabled: false,
                limits: None,
            },
        }
    }

    #[test]
    fn only_roots_declared_in_place_are_measured() {
        let root = tempfile::tempdir().unwrap();
        let (roots, reports) = InPlaceRoots::measure(&policy_with("plain", root.path(), None));
        assert!(reports.is_empty());
        let refused = roots.open_root("plain").err().unwrap();
        assert_eq!(refused.code, ReferenceRefusalCode::RootUnproven);
    }

    #[test]
    fn a_root_the_node_can_write_is_reported_unproven_and_refused() {
        // The test's own files are owned by the node's uid, so no
        // permission proof holds and the root must be refused by name.
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("blob"), b"bytes").unwrap();
        let policy = policy_with(
            "image",
            root.path(),
            Some(InPlaceRootPolicy { accept: Vec::new() }),
        );
        let (roots, reports) = InPlaceRoots::measure(&policy);
        let report = &reports["image"];
        let on_read_only_mount = report.facts.mount_read_only == Some(true)
            || report.facts.superblock_read_only == Some(true);
        if !on_read_only_mount {
            assert!(report.proof.is_none(), "{report:?}");
            assert!(report.refusal.is_some());
            let refused = roots.open_root("image").err().unwrap();
            assert_eq!(refused.code, ReferenceRefusalCode::RootUnproven);
        }
    }

    #[test]
    fn a_proven_root_resolves_until_its_directory_is_gone_or_replaced() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("root");
        std::fs::create_dir(&path).unwrap();
        let policy = policy_with(
            "image",
            &path,
            Some(InPlaceRootPolicy { accept: Vec::new() }),
        );
        let identity = policy.roots["image"].root_identity;
        // A usable state without depending on this host's mounts.
        let usable = |identity| InPlaceRoots {
            roots: BTreeMap::from([(
                "image".to_owned(),
                RootState::Usable {
                    path: path.clone(),
                    identity,
                },
            )]),
        };
        assert_eq!(
            usable(identity).open_root("image").ok().unwrap().identity,
            identity
        );
        // A different directory's identity is a changed root.
        let other = lillux::PinnedDirectory::open(parent.path())
            .unwrap()
            .unwrap()
            .identity()
            .unwrap();
        let changed = usable(other).open_root("image").err().unwrap();
        assert_eq!(changed.code, ReferenceRefusalCode::RootChanged);
        std::fs::remove_dir(&path).unwrap();
        let absent = usable(identity).open_root("image").err().unwrap();
        assert_eq!(absent.code, ReferenceRefusalCode::RootChanged);
    }
}
