//! Atomic node-owned operator policy generations.
//!
//! A source publisher may provide an init seed, but launch authority is always
//! the exact node-signed generation under `.ai/node/policies/`. Registered
//! section compilers validate every body. Publication requires the ordinary
//! stopped-node state lock and conditionally exchanges one pinned directory,
//! so readers cannot observe a mixed generation.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{NodePolicyContext, NodePolicyTable};
use crate::identity::NodeIdentity;

pub const POLICIES_DIRECTORY: &str = "policies";
const POLICY_STAGING_DIRECTORY: &str = ".policies.staging";
const MAX_POLICY_FILES: usize = 32;
const MAX_POLICY_DEPTH: usize = 1;

/// Publisher-authored initial policy generation. `exact_bundles` is the exact
/// bundle inventory selected by this node profile, not a minimum dependency
/// list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NodeInitProfile {
    pub schema: u32,
    pub exact_bundles: Vec<String>,
    pub policies: BTreeMap<String, Value>,
}

impl NodeInitProfile {
    pub fn validate(&self, policy_table: &NodePolicyTable, source_file: &Path) -> Result<()> {
        if self.schema != 1 {
            bail!("node policy set schema is not current");
        }
        if self.exact_bundles.is_empty() {
            bail!("node policy set exact_bundles is empty");
        }
        validate_sorted_unique_bundle_names(&self.exact_bundles)?;
        validate_policy_bodies(policy_table, &self.policies, source_file)
    }

    pub fn exact_bundles(&self) -> &[String] {
        &self.exact_bundles
    }

    pub fn policies(&self) -> &BTreeMap<String, Value> {
        &self.policies
    }

    pub fn validated_generation(
        &self,
        policy_table: &NodePolicyTable,
        source_file: &Path,
    ) -> Result<NodePolicyGeneration> {
        self.validate(policy_table, source_file)?;
        validate_policy_generation(policy_table, self.policies.clone(), source_file)
    }
}

/// One fully validated live policy generation.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePolicyGeneration {
    policies: BTreeMap<String, Value>,
    digest: String,
}

impl NodePolicyGeneration {
    pub fn policies(&self) -> &BTreeMap<String, Value> {
        &self.policies
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Prepare a complete replacement while retaining the exact observed
    /// generation as the compare-and-swap predecessor.
    pub fn prepare_replacement(
        &self,
        policy_table: &NodePolicyTable,
        policies: BTreeMap<String, Value>,
        source_file: &Path,
    ) -> Result<NodePolicyUpdate> {
        let generation = validate_policy_generation(policy_table, policies, source_file)?;
        Ok(NodePolicyUpdate {
            generation,
            expected: ExpectedPolicyGeneration::ExactDigest(self.digest.clone()),
        })
    }
}

/// Validated conditional publication request. Fields are private so callers
/// cannot bypass section compilation or pathname validation.
pub struct NodePolicyUpdate {
    generation: NodePolicyGeneration,
    expected: ExpectedPolicyGeneration,
}

enum ExpectedPolicyGeneration {
    Absent,
    ExactDigest(String),
    PresentSchemaCut,
}

impl NodePolicyUpdate {
    pub fn generation(&self) -> &NodePolicyGeneration {
        &self.generation
    }

    /// Prepare the first complete generation. Absence is represented only at
    /// this bootstrap CAS boundary and never as a valid empty policy.
    pub fn initial(generation: NodePolicyGeneration) -> Self {
        Self {
            generation,
            expected: ExpectedPolicyGeneration::Absent,
        }
    }

    /// Replace one pinned predecessor generation that the current registry
    /// cannot decode. Only an explicit stopped-node schema-cut operation may
    /// select this expectation.
    pub fn schema_cut(generation: NodePolicyGeneration) -> Self {
        Self {
            generation,
            expected: ExpectedPolicyGeneration::PresentSchemaCut,
        }
    }
}

pub fn validate_init_profile_name(value: &str) -> Result<()> {
    validate_policy_name("node init profile", value)
}

fn validate_sorted_unique_bundle_names(values: &[String]) -> Result<()> {
    let mut previous: Option<&str> = None;
    for value in values {
        ryeos_engine::protocol_vocabulary::validate_bundle_name(value)
            .map_err(|error| anyhow::anyhow!("invalid exact bundle name `{value}`: {error}"))?;
        if previous.is_some_and(|candidate| candidate >= value.as_str()) {
            bail!("node init profile exact bundle names must be sorted and unique");
        }
        previous = Some(value);
    }
    Ok(())
}

pub fn validate_policy_name(label: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
        && !value.ends_with(['_', '-'])
        && !value.contains("__")
        && !value.contains("--");
    if !valid {
        bail!("{label} name `{value}` is not canonical");
    }
    Ok(())
}

pub fn policy_directory(app_root: &Path) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join(POLICIES_DIRECTORY)
}

pub fn validate_policy_bodies(
    policy_table: &NodePolicyTable,
    policies: &BTreeMap<String, Value>,
    source_file: &Path,
) -> Result<()> {
    if policies.len() > MAX_POLICY_FILES {
        bail!("node policy generation exceeds {MAX_POLICY_FILES} sections");
    }
    for (section_name, body) in policies {
        validate_policy_name("node policy section", section_name)?;
        let section = policy_table
            .get(section_name)
            .with_context(|| format!("unknown node policy section `{section_name}`"))?;
        if !body.is_object() {
            bail!("node policy section `{section_name}` must contain a YAML mapping");
        }
        let body_bytes = serde_yaml::to_string(body)
            .with_context(|| format!("serialize `{section_name}` node policy"))?
            .len() as u64;
        let maximum_body_bytes = crate::node_document::MAX_ITEM_BYTES
            .saturating_sub(crate::node_document::MAX_SIGNATURE_OVERHEAD_BYTES);
        if body_bytes > maximum_body_bytes {
            bail!(
                "node policy section `{section_name}` exceeds {maximum_body_bytes} portable body bytes"
            );
        }
        for forbidden in ["category", "section"] {
            if body.get(forbidden).is_some() {
                bail!(
                    "node policy section `{section_name}` declares path-owned field `{forbidden}`"
                );
            }
        }
        let filename = format!("{section_name}.yaml");
        let context = NodePolicyContext {
            section: section_name.clone(),
            source_file: source_file.with_file_name(filename),
            signer_fingerprint: String::new(),
        };
        let record = section
            .parse(&context, body)
            .with_context(|| format!("validate `{section_name}` node policy"))?;
        if record.section_name() != section_name {
            bail!("node policy compiler `{section_name}` returned the wrong typed record");
        }
    }
    Ok(())
}

fn validate_policy_generation(
    policy_table: &NodePolicyTable,
    policies: BTreeMap<String, Value>,
    source_file: &Path,
) -> Result<NodePolicyGeneration> {
    validate_policy_bodies(policy_table, &policies, source_file)?;
    for section in policy_table.sections() {
        if !policies.contains_key(section.name()) {
            bail!(
                "node policy generation requires exactly one `{}` policy",
                section.name()
            );
        }
    }
    let digest = ryeos_state::objects::canonical_value_digest(
        &serde_json::to_value(&policies).context("serialize node policy generation")?,
    )?;
    Ok(NodePolicyGeneration { policies, digest })
}

/// Load and validate the exact node-signed policy generation.
pub fn load_policy_generation(
    app_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
    policy_table: &NodePolicyTable,
) -> Result<NodePolicyGeneration> {
    let directory = lillux::PinnedDirectory::open(&policy_directory(app_root))?
        .context("node has no explicit signed policy generation")?;
    load_policy_generation_from_directory(app_root, &directory, trust_store, policy_table)
}

/// Observe whether first publication is still required. Runtime callers must
/// use [`load_policy_generation`] and can never receive implicit defaults.
pub fn load_optional_policy_generation(
    app_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
    policy_table: &NodePolicyTable,
) -> Result<Option<NodePolicyGeneration>> {
    let Some(directory) = lillux::PinnedDirectory::open(&policy_directory(app_root))? else {
        return Ok(None);
    };
    load_policy_generation_from_directory(app_root, &directory, trust_store, policy_table).map(Some)
}

/// Prove that an explicit schema cut is replacing a bounded, node-signed
/// policy generation without interpreting its predecessor section schemas.
/// This is intentionally meaning-blind: the current registry may be unable to
/// decode the very generation the operator is retiring.
pub fn validate_schema_cut_policy_occupant(
    app_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<()> {
    let directory = lillux::PinnedDirectory::open(&policy_directory(app_root))?
        .context("node has no existing policy generation to replace")?;
    validate_schema_cut_directory_occupant(app_root, &directory, trust_store)
}

fn validate_schema_cut_directory_occupant(
    app_root: &Path,
    directory: &lillux::PinnedDirectory,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<()> {
    let node_fingerprint = crate::node_config::loader::node_identity_fingerprint(app_root)?;
    let entries = directory.entries_no_follow_bounded(MAX_POLICY_FILES)?;
    if entries.is_empty() {
        bail!("existing node policy generation is empty");
    }
    let mut names = BTreeMap::new();
    for entry in entries {
        if entry.entry_type != lillux::secure_fs::PinnedEntryType::Regular {
            bail!("existing node policy generation contains a non-regular entry");
        }
        let path = Path::new(&entry.name);
        if path.extension().and_then(OsStr::to_str) != Some("yaml") {
            bail!("existing node policy generation contains an unsupported filename");
        }
        let name = path
            .file_stem()
            .and_then(OsStr::to_str)
            .context("existing node policy filename is not UTF-8")?;
        validate_policy_name("existing node policy section", name)?;
        let file = directory
            .open_pinned_regular(&entry.name, false)?
            .context("existing node policy entry disappeared")?;
        let verified = crate::node_document::verify_pinned_signed_yaml(&file, trust_store)?;
        if verified.signer_fingerprint != node_fingerprint {
            bail!("existing policy generation is not signed by the current node identity");
        }
        if names.insert(name.to_owned(), ()).is_some() {
            bail!("existing node policy generation contains duplicate section names");
        }
    }
    Ok(())
}

fn load_policy_generation_from_directory(
    app_root: &Path,
    directory: &lillux::PinnedDirectory,
    trust_store: &ryeos_engine::trust::TrustStore,
    policy_table: &NodePolicyTable,
) -> Result<NodePolicyGeneration> {
    let node_fingerprint = crate::node_config::loader::node_identity_fingerprint(app_root)?;
    let entries = directory.entries_no_follow_bounded(MAX_POLICY_FILES)?;
    let mut policies = BTreeMap::new();
    for entry in entries {
        if entry.entry_type != lillux::secure_fs::PinnedEntryType::Regular {
            bail!(
                "node policies directory contains unsupported entry {}",
                directory.path().join(&entry.name).display()
            );
        }
        let path = directory.path().join(&entry.name);
        if path.extension().and_then(|extension| extension.to_str()) != Some("yaml") {
            bail!(
                "node policies directory contains unsupported entry {}",
                path.display()
            );
        }
        let section_name = Path::new(&entry.name)
            .file_stem()
            .and_then(OsStr::to_str)
            .context("node policy filename is not UTF-8")?
            .to_owned();
        validate_policy_name("node policy section", &section_name)?;
        let file = directory
            .open_pinned_regular(&entry.name, false)?
            .with_context(|| format!("node policy disappeared: {}", path.display()))?;
        let verified = crate::node_document::verify_pinned_signed_yaml(&file, trust_store)?;
        if verified.signer_fingerprint != node_fingerprint {
            bail!(
                "node policy {} is signed by {}, expected current node {}",
                path.display(),
                verified.signer_fingerprint,
                node_fingerprint
            );
        }
        if policies
            .insert(section_name.clone(), verified.body)
            .is_some()
        {
            bail!("duplicate node policy section `{section_name}`");
        }
    }
    validate_policy_generation(policy_table, policies, directory.path())
}

/// Publish one complete validated generation. The caller must hold the same
/// state lock used by the daemon and every offline node mutation.
pub fn publish_policy_update(
    app_root: &Path,
    update: &NodePolicyUpdate,
    identity: &NodeIdentity,
    trust_store: &ryeos_engine::trust::TrustStore,
    state_lock: &crate::state_lock::StateLock,
) -> Result<PathBuf> {
    state_lock
        .ensure_protects_app_root(app_root)
        .context("node policy publication requires this app root's state lock")?;
    let policy_table = NodePolicyTable::new();
    let current_node_fingerprint = crate::node_config::loader::node_identity_fingerprint(app_root)?;
    if identity.fingerprint() != current_node_fingerprint {
        bail!(
            "node policy signer {} is not the current node identity {}",
            identity.fingerprint(),
            current_node_fingerprint
        );
    }
    if !trust_store.is_trusted(&current_node_fingerprint) {
        bail!("current node identity is absent from the supplied trust store");
    }
    let node_root_path = app_root.join(ryeos_engine::AI_DIR).join("node");
    let node_root = lillux::PinnedDirectory::open_or_create(&node_root_path)
        .context("pin node root for policy publication")?;
    let target_name = OsStr::new(POLICIES_DIRECTORY);
    let current = node_root.open_child_directory(target_name)?;
    match &update.expected {
        ExpectedPolicyGeneration::Absent if current.is_some() => {
            bail!("node policy generation appeared before initial publication")
        }
        ExpectedPolicyGeneration::Absent => {}
        ExpectedPolicyGeneration::ExactDigest(expected) => {
            let current = current
                .as_ref()
                .context("node policy generation disappeared before replacement")?;
            let found = load_policy_generation_from_directory(
                app_root,
                current,
                trust_store,
                &policy_table,
            )?
            .digest;
            if &found != expected {
                bail!(
                    "node policy generation changed before publication: expected {expected}, found {found}"
                );
            }
        }
        ExpectedPolicyGeneration::PresentSchemaCut if current.is_none() => {
            bail!("node policy generation disappeared before explicit schema cut")
        }
        ExpectedPolicyGeneration::PresentSchemaCut => {
            validate_schema_cut_directory_occupant(
                app_root,
                current.as_ref().expect("presence checked above"),
                trust_store,
            )?;
        }
    }

    let staging_name = OsString::from(POLICY_STAGING_DIRECTORY);
    if let Some(retired) = node_root.open_child_directory(&staging_name)? {
        retired.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(
            MAX_POLICY_FILES,
            MAX_POLICY_DEPTH,
        ))?;
        if !node_root.remove_empty_child_if_same(&staging_name, &retired)? {
            bail!("stale node policy staging directory remained non-empty");
        }
    } else if node_root.entry_no_follow(&staging_name)?.is_some() {
        bail!("node policy staging name is occupied by a non-directory entry");
    }
    let staging = node_root
        .create_child(&staging_name, 0o700)
        .context("create node policy staging directory")?;
    let mut committed = false;
    let result = (|| {
        for (section_name, body) in &update.generation.policies {
            let filename = OsString::from(format!("{section_name}.yaml"));
            let bytes =
                crate::node_document::render_signed_item(section_name, "policy", body, identity)?;
            staging.atomic_write_pinned_if_same(&filename, None, &bytes, 0o600)?;
        }
        staging.sync_tree_bounded(lillux::DirectoryTraversalBudget::new(
            MAX_POLICY_FILES,
            MAX_POLICY_DEPTH,
        ))?;
        let staged_generation =
            load_policy_generation_from_directory(app_root, &staging, trust_store, &policy_table)
                .context("verify complete staged node policy generation")?;
        if staged_generation.digest != update.generation.digest {
            bail!(
                "staged node policy generation digest {} does not match requested {}",
                staged_generation.digest,
                update.generation.digest
            );
        }

        if let Some(current) = current.as_ref() {
            match node_root.exchange_child_directories_if_same(
                target_name,
                current,
                &staging_name,
                &staging,
            ) {
                Ok(()) => {}
                Err(error) if error.namespace_committed() => {
                    tracing::warn!(%error, "node policy generation committed before durability warning");
                }
                Err(error) => return Err(error.into()),
            }
            committed = true;
            let cleanup = current
                .remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(
                    MAX_POLICY_FILES,
                    MAX_POLICY_DEPTH,
                ))
                .and_then(|()| {
                    node_root
                        .remove_empty_child_if_same(&staging_name, current)
                        .and_then(|removed| {
                            if removed {
                                Ok(())
                            } else {
                                bail!("retired node policy generation remained non-empty")
                            }
                        })
                });
            if let Err(error) = cleanup {
                tracing::warn!(%error, "node policy generation committed with retired staging cleanup pending");
            }
        } else {
            match node_root.rename_child_directory_noreplace(&staging_name, target_name, &staging) {
                Ok(()) => {}
                Err(error) if error.namespace_committed() => {
                    tracing::warn!(%error, "node policy generation published before durability warning");
                }
                Err(error) => return Err(error.into()),
            }
            committed = true;
        }
        Ok::<(), anyhow::Error>(())
    })();
    if result.is_err()
        && !committed
        && let Some(candidate) = node_root.open_child_directory(&staging_name)?
    {
        let is_unpublished_stage = candidate.is_same_directory(&staging)?;
        let is_proven_retired = if let Some(current) = current.as_ref() {
            let target_is_published_stage = match node_root.open_child_directory(target_name)? {
                Some(target) => target.is_same_directory(&staging)?,
                None => false,
            };
            candidate.is_same_directory(current)? && target_is_published_stage
        } else {
            false
        };
        if is_unpublished_stage || is_proven_retired {
            let _ = candidate.remove_contents_recursive_bounded(
                lillux::DirectoryTraversalBudget::new(MAX_POLICY_FILES, MAX_POLICY_DEPTH),
            );
            let _ = node_root.remove_empty_child_if_same(&staging_name, &candidate);
        }
    }
    result?;
    Ok(policy_directory(app_root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_authored_init_profile_compiles_as_one_complete_generation() {
        let profile_directory =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../bundles/.ai/node/init/profiles");
        let mut profiles = std::fs::read_dir(&profile_directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        profiles.sort();
        assert!(!profiles.is_empty(), "init profile inventory is empty");

        let table = NodePolicyTable::new();
        for path in profiles {
            assert_eq!(
                path.extension().and_then(|value| value.to_str()),
                Some("yaml")
            );
            let raw = std::fs::read_to_string(&path).unwrap();
            let body = lillux::signature::strip_signature_lines(&raw);
            let profile: NodeInitProfile = serde_yaml::from_str(&body).unwrap();
            profile
                .validated_generation(&table, &path)
                .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        }
    }
}
