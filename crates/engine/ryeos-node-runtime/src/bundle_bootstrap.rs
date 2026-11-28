//! Trusted flat bundle-registration bootstrap, independent of App services.

use crate::node_config_source::{
    BundleRecord, NodeItemContext, check_bundle_collisions, scan_yaml_files, verify_and_parse,
};
use anyhow::{Context, Result, bail};
use ryeos_engine::trust::TrustStore;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

pub const BUNDLE_REGISTRATIONS_MISSING: &str = "bundle_registrations_missing";
pub const BUNDLE_REGISTRATIONS_EMPTY: &str = "bundle_registrations_empty";
pub const BUNDLE_REGISTRATION_NOT_NODE_SIGNED: &str = "bundle_registration_not_node_signed";
pub const BUNDLE_REGISTRATION_ENTRY_REFUSED: &str = "bundle_registration_entry_refused";
pub const BUNDLE_REGISTRATION_COLLISION: &str = "bundle_registration_collision";

/// A refusal of the node's bundle registrations, carrying a stable code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleRegistrationRefusal {
    pub code: &'static str,
    pub message: String,
}

impl BundleRegistrationRefusal {
    fn error(code: &'static str, message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self {
            code,
            message: message.into(),
        })
    }

    /// The refusal in an error's chain, if there is one.
    pub fn find(error: &anyhow::Error) -> Option<&BundleRegistrationRefusal> {
        error.chain().find_map(|cause| cause.downcast_ref::<Self>())
    }
}

impl std::fmt::Display for BundleRegistrationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for BundleRegistrationRefusal {}

pub struct BundleBootstrapLoader<'a> {
    pub app_root: &'a Path,
    pub trust_store: &'a TrustStore,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBundleRecord {
    kind: String,
    path: std::path::PathBuf,
    #[serde(default)]
    lane: Option<ryeos_engine::trust::BundleLane>,
}

pub fn parse_bundle(ctx: &NodeItemContext, body: &Value) -> Result<BundleRecord> {
    let raw: RawBundleRecord =
        serde_json::from_value(body.clone()).context("failed to parse bundle record")?;
    if raw.kind != "node" {
        bail!(
            "bundle '{}' declares kind {:?}, expected kind 'node'",
            ctx.id,
            raw.kind
        );
    }

    if !raw.path.is_absolute() {
        bail!(
            "bundle '{}' path must be absolute, got: {}",
            ctx.id,
            raw.path.display()
        );
    }

    if let Some(lane) = &raw.lane {
        lane.validate_shape()
            .map_err(|error| anyhow::anyhow!("bundle '{}' has an invalid lane: {error}", ctx.id))?;
    }

    Ok(BundleRecord {
        name: ctx.id.clone(),
        lane: raw.lane,
        path: raw.path,
        source_file: std::path::PathBuf::new(),
    })
}

/// A laned bundle the loader left out because it no longer passes what this
/// node admits from the publication lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedLaneBundle {
    pub name: String,
    /// The stable refusal code.
    pub code: &'static str,
    pub message: String,
}

impl<'a> BundleBootstrapLoader<'a> {
    /// Phase 1: load only the `bundles` section to determine effective bundle roots.
    ///
    /// Scans `<app_root>/.ai/node/bundles/` (flat only — no recursion).
    /// Uses minimal bootstrap verifier (signature + hash, no full engine).
    pub fn load_bundle_section(&self) -> Result<Vec<BundleRecord>> {
        self.load_bundle_section_reporting_skipped()
            .map(|(records, _)| records)
    }

    /// As [`Self::load_bundle_section`], and also the laned bundles that were
    /// left out. A laned bundle is checked against the node's kind registry
    /// (built from the native bundles) and its `publication_lane` policy; one
    /// that fails is not loaded, is logged at error level and is reported
    /// here. It never stops the node from loading the others.
    pub fn load_bundle_section_reporting_skipped(
        &self,
    ) -> Result<(Vec<BundleRecord>, Vec<SkippedLaneBundle>)> {
        let records = self.load_registrations()?;
        let (records, skipped) = self.audit_laned_bundles(records);
        if records.is_empty() {
            return Err(BundleRegistrationRefusal::error(
                BUNDLE_REGISTRATIONS_EMPTY,
                "no bundle registration remains after the publication-lane audit",
            ));
        }

        Ok((records, skipped))
    }

    /// Every verified registration, laned ones included, with no lane audit.
    ///
    /// This is the read for a question about what a name is registered as: a
    /// laned bundle that no longer passes its audit is not loaded by the node,
    /// but it is still registered, still holds its name, and is still laned.
    /// Replacement and name guards read here, never through the audit.
    pub fn load_registrations(&self) -> Result<Vec<BundleRecord>> {
        let mut records: Vec<BundleRecord> = Vec::new();

        let node_dir = self
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("bundles");
        // A node always holds at least one registration (core), so a missing
        // directory or an empty one is a refusal, never an empty set.
        if matches!(
            std::fs::symlink_metadata(&node_dir),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ) {
            return Err(BundleRegistrationRefusal::error(
                BUNDLE_REGISTRATIONS_MISSING,
                format!(
                    "the node's bundle registration directory {} does not exist",
                    node_dir.display()
                ),
            ));
        }
        // Bundles use one exact flat YAML namespace. Any directory, symlink,
        // special file, or non-YAML entry is refused.
        let files = scan_yaml_files(&node_dir, false).map_err(|error| {
            BundleRegistrationRefusal::error(
                BUNDLE_REGISTRATION_ENTRY_REFUSED,
                format!("{error:#}"),
            )
        })?;
        if files.is_empty() {
            return Err(BundleRegistrationRefusal::error(
                BUNDLE_REGISTRATIONS_EMPTY,
                format!(
                    "the node's bundle registration directory {} holds no registration",
                    node_dir.display()
                ),
            ));
        }
        // Registrations are node state, written only with the node key
        // (`bundle_transaction`). A registration signed by any other pinned
        // key, a publisher's included, is refused.
        let node_fingerprint = if files.is_empty() {
            String::new()
        } else {
            crate::identity::node_identity_fingerprint(self.app_root)?
        };
        for path in files {
            let verified = verify_and_parse(&path, &node_dir, "bundles", self.trust_store)?;
            ryeos_engine::trust::require_document_signer(
                verified.path(),
                verified.signer_fingerprint(),
                &node_fingerprint,
                "node",
                "bundle registrations are written only by the node's own `ryeos bundle install`: remove the file and reinstall the bundle",
            )
            .map_err(|error| {
                BundleRegistrationRefusal::error(
                    BUNDLE_REGISTRATION_NOT_NODE_SIGNED,
                    format!("{error:#}"),
                )
            })?;

            let mut record =
                parse_bundle(verified.context(), verified.body()).with_context(|| {
                    format!(
                        "failed to parse bundle record {}",
                        verified.path().display()
                    )
                })?;
            record.source_file = verified.path().to_path_buf();

            // Validate path: canonicalize, must exist as directory
            if !record.path.is_dir() {
                bail!(
                    "bundle '{}' path '{}' does not exist or is not a directory (declared in {})",
                    record.name,
                    record.path.display(),
                    record.source_file.display()
                );
            }

            let canonical = record.path.canonicalize().with_context(|| {
                format!(
                    "failed to canonicalize bundle '{}' path '{}'",
                    record.name,
                    record.path.display()
                )
            })?;
            record.path = canonical;

            records.push(record);
        }

        // Collision detection: by canonical path AND by name (fail-closed)
        check_bundle_collisions(&records).map_err(|error| {
            BundleRegistrationRefusal::error(BUNDLE_REGISTRATION_COLLISION, format!("{error:#}"))
        })?;
        Ok(records)
    }

    /// Check every laned record; leave out the ones that fail.
    fn audit_laned_bundles(
        &self,
        records: Vec<BundleRecord>,
    ) -> (Vec<BundleRecord>, Vec<SkippedLaneBundle>) {
        if records.iter().all(|record| record.lane.is_none()) {
            return (records, Vec::new());
        }
        let schema_roots: Vec<_> = records
            .iter()
            .filter(|record| record.lane.is_none())
            .map(|record| {
                record
                    .path
                    .join(ryeos_engine::AI_DIR)
                    .join(ryeos_engine::KIND_SCHEMAS_DIR)
            })
            .filter(|dir| dir.is_dir())
            .collect();
        let context = ryeos_engine::kind_registry::KindRegistry::load_base(
            &schema_roots,
            self.trust_store,
        )
        .map_err(|error| format!("the native kind schemas could not be loaded: {error}"))
        .and_then(|kinds| {
            let table = crate::node_policy::NodePolicyTable::new();
            crate::node_policy::load_snapshot(self.app_root, self.trust_store, &table)
                .and_then(|snapshot| {
                    snapshot
                        .require::<crate::node_policy::sections::publication_lane::PublicationLanePolicy>()
                        .cloned()
                })
                .map(|policy| (kinds, policy))
                .map_err(|error| format!("the publication_lane policy is unavailable: {error:#}"))
        });
        let mut kept = Vec::with_capacity(records.len());
        let mut skipped = Vec::new();
        for record in records {
            if record.lane.is_none() {
                kept.push(record);
                continue;
            }
            let (code, message) = match &context {
                Ok((kinds, policy)) => {
                    match ryeos_bundle::lane::audit_installed_lane_bundle(
                        &record.path,
                        &record.name,
                        kinds,
                        &policy.grants_for(&record.name),
                    ) {
                        Ok(()) => {
                            kept.push(record);
                            continue;
                        }
                        Err(refusal) => (refusal.code(), refusal.to_string()),
                    }
                }
                Err(detail) => ("publication_lane_policy_unavailable", detail.clone()),
            };
            tracing::error!(
                bundle = %record.name,
                code,
                error = %message,
                "a publication-lane bundle does not pass what this node admits; it is not loaded"
            );
            skipped.push(SkippedLaneBundle {
                name: record.name.clone(),
                code,
                message,
            });
        }
        (kept, skipped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A node root with an identity, a publisher key, and a trust store
    /// pinning both.
    struct Node {
        root: tempfile::TempDir,
        identity: crate::identity::NodeIdentity,
        publisher: lillux::crypto::SigningKey,
        trust: TrustStore,
    }

    impl Node {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join(".ai/node/identity")).unwrap();
            let identity = crate::identity::NodeIdentity::create(
                &root.path().join(".ai/node/identity/private_key.pem"),
            )
            .unwrap();
            let publisher = lillux::crypto::SigningKey::generate(&mut rand::rngs::OsRng);
            let trust = TrustStore::from_signers(vec![
                ryeos_engine::trust::TrustedSigner {
                    fingerprint: identity.fingerprint().to_owned(),
                    verifying_key: identity.signing_key().verifying_key(),
                    label: None,
                },
                ryeos_engine::trust::TrustedSigner {
                    fingerprint: lillux::crypto::fingerprint(&publisher.verifying_key()),
                    verifying_key: publisher.verifying_key(),
                    label: None,
                },
            ]);
            Self {
                root,
                identity,
                publisher,
                trust,
            }
        }

        fn registrations(&self) -> PathBuf {
            self.root.path().join(".ai/node/bundles")
        }

        fn bundle_dir(&self, name: &str) -> PathBuf {
            let dir = self.root.path().join(".ai/bundles").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn register_signed_by(&self, name: &str, bundle: &Path, key: &lillux::crypto::SigningKey) {
            std::fs::create_dir_all(self.registrations()).unwrap();
            let body = serde_json::json!({ "kind": "node", "path": bundle });
            let yaml = serde_yaml::to_string(&body).unwrap();
            std::fs::write(
                self.registrations().join(format!("{name}.yaml")),
                lillux::signature::sign_content(&yaml, key, "#", None),
            )
            .unwrap();
        }

        fn register(&self, name: &str) {
            let bundle = self.bundle_dir(name);
            self.register_signed_by(name, &bundle, self.identity.signing_key());
        }

        fn loader(&self) -> BundleBootstrapLoader<'_> {
            BundleBootstrapLoader {
                app_root: self.root.path(),
                trust_store: &self.trust,
            }
        }

        /// The refusal code each load path gives for the node as it is.
        fn refusal_codes(&self) -> [&'static str; 2] {
            let loader = self.loader();
            [
                loader.load_registrations().unwrap_err(),
                loader.load_bundle_section().unwrap_err(),
            ]
            .map(|error| {
                BundleRegistrationRefusal::find(&error)
                    .unwrap_or_else(|| panic!("no typed refusal in {error:#}"))
                    .code
            })
        }
    }

    #[test]
    fn a_node_signed_registration_loads() {
        let node = Node::new();
        node.register("core");
        let loaded = node.loader().load_bundle_section().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "core");
    }

    #[test]
    fn a_publisher_signed_registration_is_refused() {
        let node = Node::new();
        let bundle = node.bundle_dir("core");
        node.register_signed_by("core", &bundle, &node.publisher);
        assert_eq!(
            node.refusal_codes(),
            [BUNDLE_REGISTRATION_NOT_NODE_SIGNED; 2]
        );
    }

    #[test]
    fn a_non_yaml_entry_is_refused() {
        let node = Node::new();
        node.register("core");
        std::fs::write(node.registrations().join("notes.txt"), b"x").unwrap();
        assert_eq!(node.refusal_codes(), [BUNDLE_REGISTRATION_ENTRY_REFUSED; 2]);
    }

    #[test]
    fn a_directory_entry_is_refused() {
        let node = Node::new();
        node.register("core");
        std::fs::create_dir_all(node.registrations().join("nested")).unwrap();
        assert_eq!(node.refusal_codes(), [BUNDLE_REGISTRATION_ENTRY_REFUSED; 2]);
    }

    #[test]
    fn a_symlinked_registration_is_refused() {
        let node = Node::new();
        node.register("core");
        lillux::tree_entry::test_support::make_symlink(
            &node.registrations().join("core.yaml"),
            &node.registrations().join("alias.yaml"),
        )
        .unwrap();
        assert_eq!(node.refusal_codes(), [BUNDLE_REGISTRATION_ENTRY_REFUSED; 2]);
    }

    #[test]
    fn two_registrations_of_one_bundle_collide() {
        let node = Node::new();
        let bundle = node.bundle_dir("core");
        node.register_signed_by("core", &bundle, node.identity.signing_key());
        node.register_signed_by("alias", &bundle, node.identity.signing_key());
        assert_eq!(node.refusal_codes(), [BUNDLE_REGISTRATION_COLLISION; 2]);
    }

    #[test]
    fn a_missing_directory_is_refused() {
        let node = Node::new();
        assert_eq!(node.refusal_codes(), [BUNDLE_REGISTRATIONS_MISSING; 2]);
    }

    #[test]
    fn an_empty_directory_is_refused() {
        let node = Node::new();
        std::fs::create_dir_all(node.registrations()).unwrap();
        assert_eq!(node.refusal_codes(), [BUNDLE_REGISTRATIONS_EMPTY; 2]);
    }

    fn ctx() -> NodeItemContext {
        NodeItemContext {
            section: "bundles".to_string(),
            id: "pulled".to_string(),
            stem: "pulled".to_string(),
            rel_path: PathBuf::from("pulled.yaml"),
            source_file: PathBuf::from("/app/.ai/node/bundles/pulled.yaml"),
            signer_fingerprint: "f".repeat(64),
        }
    }

    fn lane_value() -> Value {
        serde_json::to_value(ryeos_engine::trust::BundleLane::publication(
            "a".repeat(64),
            "b".repeat(64),
            "c".repeat(64),
            "1.0.0",
        ))
        .unwrap()
    }

    #[test]
    fn registration_without_a_lane_parses_with_none() {
        let record =
            parse_bundle(&ctx(), &serde_json::json!({"kind": "node", "path": "/b"})).unwrap();
        assert!(record.lane.is_none());
    }

    #[test]
    fn registration_lane_is_carried_into_the_record() {
        let record = parse_bundle(
            &ctx(),
            &serde_json::json!({"kind": "node", "path": "/b", "lane": lane_value()}),
        )
        .unwrap();
        assert_eq!(record.lane.unwrap().name, "publication");
    }

    #[test]
    fn registration_with_a_malformed_lane_is_refused() {
        let mut lane = lane_value();
        lane["record_hash"] = serde_json::json!("not-hex");
        let error = parse_bundle(
            &ctx(),
            &serde_json::json!({"kind": "node", "path": "/b", "lane": lane}),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("record_hash"), "{error:#}");
    }
}
