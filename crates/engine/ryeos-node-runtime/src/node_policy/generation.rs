//! Atomic node-owned operator policy generations.
//!
//! A source publisher may provide an init seed, but launch authority is always
//! the exact node-signed generation under `.ai/node/policies/`. Registered
//! section compilers validate every body. Publication requires the ordinary
//! stopped-node state lock and conditionally exchanges one pinned directory,
//! so readers cannot observe a mixed generation.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{NodePolicyContext, NodePolicyTable, TypedNodePolicy};
use crate::identity::NodeIdentity;

pub const POLICIES_DIRECTORY: &str = "policies";
const POLICY_STAGING_DIRECTORY: &str = ".policies.staging";
const MAX_POLICY_FILES: usize = 32;
const MAX_POLICY_DEPTH: usize = 1;
/// The generation's signed record of which sections an operator applied. The
/// leading underscore is outside the canonical section-name grammar, so no
/// section can ever own this filename.
const OPERATOR_SECTIONS_FILE: &str = "_operator-sections.yaml";
const OPERATOR_SECTIONS_KEY: &str = "sections";

/// The installed generation lacks a section this release registers. Every
/// registered section is mandatory and has no code fallback, so the node
/// does not load. The cure is the stopped-node policy generation
/// replacement: it takes each newly required section from this release's
/// profile and carries operator-applied sections forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePolicySectionsMissing {
    pub directory: PathBuf,
    pub sections: Vec<String>,
}

impl std::fmt::Display for NodePolicySectionsMissing {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sections = self
            .sections
            .iter()
            .map(|section| format!("`{section}`"))
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            formatter,
            "the node policy generation at {} has no {sections} section, which this release \
             requires; with the daemon stopped, replace the generation from this release's \
             profile with `{POLICY_GENERATION_RESET_COMMAND}` (the new section comes from \
             the profile, and operator-applied sections carry forward)",
            self.directory.display(),
        )
    }
}

impl std::error::Error for NodePolicySectionsMissing {}

/// The stopped-node command that replaces a policy generation from the
/// release's profile, as an operator types it.
pub const POLICY_GENERATION_RESET_COMMAND: &str = "ryeos node reset policy-generation \
     --node-profile <profile> --host-class <host-class> --source <source> --confirm";

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
        for host_class in self.host_classes()? {
            let selected = self.for_host_class(&host_class)?;
            validate_policy_bodies(policy_table, &selected.policies, source_file)
                .with_context(|| format!("validate node profile host class `{host_class}`"))?;
        }
        Ok(())
    }

    /// Host classes this profile declares in its isolation policy.
    pub fn host_classes(&self) -> Result<Vec<String>> {
        let body = self
            .policies
            .get(super::sections::isolation::SECTION_NAME)
            .context("node profile has no isolation policy")?;
        super::sections::isolation::declared_host_classes(body)
            .context("read the host classes of the node profile")
    }

    /// This profile with its isolation policy narrowed to the one host
    /// class the deployment names. A class the profile does not declare is
    /// refused; no other class stands in for it.
    pub fn for_host_class(&self, host_class: &str) -> Result<Self> {
        let declared = self.host_classes()?;
        if !declared.iter().any(|name| name == host_class) {
            bail!(
                "node profile does not declare host class `{host_class}`; it declares {}",
                declared.join(", ")
            );
        }
        let mut selected = self.clone();
        let body = selected
            .policies
            .get_mut(super::sections::isolation::SECTION_NAME)
            .context("node profile has no isolation policy")?;
        *body = super::sections::isolation::select_host_class(body, host_class)?;
        Ok(selected)
    }

    /// This profile with the declared external-content import roots measured
    /// against `app_root` and recorded with their filesystem identities. A
    /// profile without that section is returned unchanged.
    pub fn admit_import_roots(&self, app_root: &Path) -> Result<Self> {
        let mut admitted = self.clone();
        if let Some(body) = admitted
            .policies
            .get_mut(super::sections::external_content::SECTION_NAME)
        {
            *body = super::sections::external_content::admit_declared_roots(body, app_root)?;
        }
        Ok(admitted)
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
        validate_policy_generation(
            policy_table,
            self.policies.clone(),
            Some(BTreeSet::new()),
            source_file,
        )
    }

    /// This profile's member for policy `T`, compiled by its registered
    /// section exactly as a node generation compiles it.
    pub fn typed_policy<T: TypedNodePolicy + Clone>(
        &self,
        policy_table: &NodePolicyTable,
        source_file: &Path,
    ) -> Result<T> {
        let body = self
            .policies
            .get(T::SECTION_NAME)
            .with_context(|| format!("node profile has no `{}` policy", T::SECTION_NAME))?;
        let section = policy_table
            .get(T::SECTION_NAME)
            .with_context(|| format!("unknown node policy section `{}`", T::SECTION_NAME))?;
        let context = NodePolicyContext {
            section: T::SECTION_NAME.to_owned(),
            source_file: source_file.with_file_name(format!("{}.yaml", T::SECTION_NAME)),
            signer_fingerprint: String::new(),
        };
        let record = section
            .parse(&context, body)
            .with_context(|| format!("validate `{}` node policy", T::SECTION_NAME))?;
        record
            .as_any()
            .downcast_ref::<T>()
            .cloned()
            .with_context(|| {
                format!(
                    "node policy compiler `{}` returned the wrong typed record",
                    T::SECTION_NAME
                )
            })
    }
}

/// One fully validated live policy generation.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePolicyGeneration {
    policies: BTreeMap<String, Value>,
    /// Sections an operator applied with `policy-apply`, which a policy
    /// generation replacement carries forward. `None` only for a generation
    /// published before this record existed: nothing is known about which of
    /// its sections an operator chose, so a replacement asks rather than
    /// guessing.
    operator_sections: Option<BTreeSet<String>>,
    digest: String,
}

impl NodePolicyGeneration {
    pub fn policies(&self) -> &BTreeMap<String, Value> {
        &self.policies
    }

    pub fn operator_sections(&self) -> Option<&BTreeSet<String>> {
        self.operator_sections.as_ref()
    }

    /// This generation's policies with the operator-applied members of
    /// `current` in place of its own, for comparing a profile against a node
    /// whose operator changed some sections.
    pub fn with_operator_sections_of(
        &self,
        current: &NodePolicyGeneration,
    ) -> BTreeMap<String, Value> {
        let mut policies = self.policies.clone();
        for section in current.operator_sections.iter().flatten() {
            if let Some(body) = current.policies.get(section) {
                policies.insert(section.clone(), body.clone());
            }
        }
        policies
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Refuse unless this generation's isolation policy is the one for
    /// `host_class`, the class the node's signed substrate identity binds.
    /// A node's host class is fixed at initialization: a policy for another
    /// class would run the node under a floor and lane chosen for a
    /// different kind of host.
    pub fn require_host_class(&self, host_class: &str) -> Result<()> {
        let body = self
            .policies
            .get(super::sections::isolation::SECTION_NAME)
            .context("node policy generation has no isolation policy")?;
        let declared = super::sections::isolation::declared_host_classes(body)?;
        if declared != [host_class] {
            bail!(
                "the isolation policy is for host class {}, but this node was initialized \
                 under host class `{host_class}`; a node's host class does not change, so \
                 apply an isolation policy for `{host_class}`",
                declared
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(())
    }

    /// Prepare a complete replacement while retaining the exact observed
    /// generation as the compare-and-swap predecessor.
    pub fn prepare_replacement(
        &self,
        policy_table: &NodePolicyTable,
        policies: BTreeMap<String, Value>,
        source_file: &Path,
    ) -> Result<NodePolicyUpdate> {
        let generation = validate_policy_generation(
            policy_table,
            policies,
            self.operator_sections.clone(),
            source_file,
        )?;
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
    operator_sections: Option<BTreeSet<String>>,
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
    for section in operator_sections.iter().flatten() {
        if !policies.contains_key(section) {
            bail!(
                "node policy generation records operator section `{section}` it does not contain"
            );
        }
    }
    let digest = generation_digest(&policies, operator_sections.as_ref())?;
    Ok(NodePolicyGeneration {
        policies,
        operator_sections,
        digest,
    })
}

/// A generation without an operator record keeps the bodies-only digest it
/// was published under; one with a record binds the record too.
fn generation_digest(
    policies: &BTreeMap<String, Value>,
    operator_sections: Option<&BTreeSet<String>>,
) -> Result<String> {
    let value = match operator_sections {
        None => serde_json::to_value(policies),
        Some(sections) => serde_json::to_value(serde_json::json!({
            "policies": policies,
            "operator_sections": sections,
        })),
    }
    .context("serialize node policy generation")?;
    ryeos_state::objects::canonical_value_digest(&value)
}

fn operator_sections_body(sections: &BTreeSet<String>) -> Value {
    serde_json::json!({ OPERATOR_SECTIONS_KEY: sections })
}

fn parse_operator_sections(body: &Value) -> Result<BTreeSet<String>> {
    let map = body
        .as_object()
        .context("node policy operator record must be a mapping")?;
    if map.len() != 1 {
        bail!("node policy operator record declares fields other than `{OPERATOR_SECTIONS_KEY}`");
    }
    let names = map
        .get(OPERATOR_SECTIONS_KEY)
        .and_then(Value::as_array)
        .context("node policy operator record has no section list")?;
    let mut sections = BTreeSet::new();
    let mut previous: Option<&str> = None;
    for name in names {
        let name = name
            .as_str()
            .context("node policy operator record names a non-string section")?;
        validate_policy_name("node policy operator section", name)?;
        if previous.is_some_and(|candidate| candidate >= name) {
            bail!("node policy operator record sections must be sorted and unique");
        }
        previous = Some(name);
        sections.insert(name.to_owned());
    }
    Ok(sections)
}

/// Explicit replacement does not need to interpret the retired member's
/// schema. Verify the complete predecessor's signatures, replace exactly the
/// requested member, then compile every resulting member before publication.
/// Runtime loaders remain strict; this is not a predecessor reader or fallback.
/// `host_class` is the class the node's signed substrate identity binds; a
/// resulting generation whose isolation policy is for any other class is
/// refused.
pub fn prepare_policy_member_replacement(
    app_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
    policy_table: &NodePolicyTable,
    host_class: &str,
    section: &str,
    body: Value,
    source_file: &Path,
) -> Result<NodePolicyUpdate> {
    policy_table
        .get(section)
        .context("unknown replacement policy section")?;
    let directory = lillux::PinnedDirectory::open(&policy_directory(app_root))?
        .context("node has no explicit signed policy generation")?;
    let SignedPolicyBodies {
        mut policies,
        operator_sections,
    } = read_signed_policy_bodies(app_root, &directory, trust_store)?;
    if !policies.contains_key(section) {
        bail!("node policy generation has no `{section}` member to replace");
    }
    let expected = ExpectedPolicyGeneration::ExactDigest(generation_digest(
        &policies,
        operator_sections.as_ref(),
    )?);
    policies.insert(section.to_owned(), body);
    // A generation published before the record existed stays unrecorded: its
    // earlier operator changes are unknown, and recording only this one would
    // let the next replacement silently drop them. That replacement asks.
    let operator_sections = operator_sections.map(|mut sections| {
        sections.insert(section.to_owned());
        sections
    });
    let generation =
        validate_policy_generation(policy_table, policies, operator_sections, source_file)?;
    generation.require_host_class(host_class)?;
    Ok(NodePolicyUpdate {
        generation,
        expected,
    })
}

/// The operator's explicit choices for a policy generation replacement:
/// sections to keep from the installed generation and sections to take from
/// the new profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyCarryChoices {
    #[serde(default)]
    pub keep: BTreeSet<String>,
    #[serde(default)]
    pub reset: BTreeSet<String>,
}

impl PolicyCarryChoices {
    pub fn is_empty(&self) -> bool {
        self.keep.is_empty() && self.reset.is_empty()
    }
}

/// Replace the node's policy generation with `profile`, carrying every
/// section an operator applied. Each carried body is compiled under the
/// current table; the replacement is refused, naming every section that no
/// longer validates, before anything changes. A generation published before
/// the operator record existed cannot say which sections were chosen, so
/// every member that differs from the profile must be named in
/// `choices.keep` or `choices.reset`. Nothing is reset silently.
///
/// The installed generation is read meaning-blind, as the schema cut already
/// proves it, and is unchanged until publication: a rerun after a crash
/// computes the same generation, including after the exchange, when the
/// installed generation is the one this produced.
pub fn prepare_schema_cut_carrying_operator_policy(
    app_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
    policy_table: &NodePolicyTable,
    host_class: &str,
    profile: NodePolicyGeneration,
    choices: &PolicyCarryChoices,
    source_file: &Path,
) -> Result<NodePolicyUpdate> {
    let directory = lillux::PinnedDirectory::open(&policy_directory(app_root))?
        .context("node has no existing policy generation to replace")?;
    validate_schema_cut_directory_occupant(app_root, &directory, trust_store)?;
    let installed = read_signed_policy_bodies(app_root, &directory, trust_store)?;
    for name in choices.keep.iter().chain(&choices.reset) {
        validate_policy_name("policy carry section", name)?;
    }
    if let Some(both) = choices.keep.intersection(&choices.reset).next() {
        bail!("section `{both}` is named both to keep and to reset");
    }
    for name in &choices.keep {
        if !installed.policies.contains_key(name) {
            bail!("the installed policy generation has no `{name}` section to keep");
        }
    }
    let carried: BTreeSet<String> = match &installed.operator_sections {
        Some(recorded) => recorded
            .union(&choices.keep)
            .filter(|section| !choices.reset.contains(*section))
            .cloned()
            .collect(),
        None => {
            let undecided = installed
                .policies
                .iter()
                .filter(|(section, body)| {
                    profile
                        .policies
                        .get(*section)
                        .is_some_and(|new| new != *body)
                        && !choices.keep.contains(*section)
                        && !choices.reset.contains(*section)
                })
                .map(|(section, _)| section.as_str())
                .collect::<Vec<_>>();
            if !undecided.is_empty() {
                bail!(
                    "this node's policy generation predates the operator record, so it cannot \
                     say which sections an operator chose; these differ from the new profile: \
                     {}. Name each with --keep-section (carry the installed policy) or \
                     --reset-section (take the profile's)",
                    undecided.join(", ")
                );
            }
            choices.keep.clone()
        }
    };
    let mut refused = Vec::new();
    let mut policies = profile.policies.clone();
    for section in &carried {
        let Some(body) = installed.policies.get(section) else {
            refused.push(format!(
                "`{section}`: recorded as operator-applied but absent from the installed generation"
            ));
            continue;
        };
        let single = BTreeMap::from([(section.clone(), body.clone())]);
        if let Err(error) = validate_policy_bodies(policy_table, &single, source_file) {
            refused.push(format!("`{section}`: {error:#}"));
            continue;
        }
        policies.insert(section.clone(), body.clone());
    }
    if !refused.is_empty() {
        bail!(
            "operator-applied policy no longer validates under this release; nothing was \
             changed. Fix and re-apply it after the replacement, or pass --reset-section \
             to take the profile's: {}",
            refused.join("; ")
        );
    }
    let generation =
        validate_policy_generation(policy_table, policies, Some(carried), source_file)?;
    generation
        .require_host_class(host_class)
        .context("check the replacement generation against this node's host class")?;
    Ok(NodePolicyUpdate::schema_cut(generation))
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
    let node_fingerprint = crate::identity::node_identity_fingerprint(app_root)?;
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
        if entry.name != OPERATOR_SECTIONS_FILE {
            validate_policy_name("existing node policy section", name)?;
        }
        let file = directory
            .open_pinned_regular(&entry.name, false)?
            .context("existing node policy entry disappeared")?;
        crate::node_document::verify_pinned_signed_yaml(&file, trust_store, &node_fingerprint)?;
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
    let SignedPolicyBodies {
        policies,
        operator_sections,
    } = read_signed_policy_bodies(app_root, directory, trust_store)?;
    let missing = policy_table
        .sections()
        .map(|section| section.name())
        .filter(|name| !policies.contains_key(*name))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(NodePolicySectionsMissing {
            directory: directory.path().to_path_buf(),
            sections: missing,
        }
        .into());
    }
    validate_policy_generation(policy_table, policies, operator_sections, directory.path())
}

/// A generation's node-signed members, read without compiling any section.
struct SignedPolicyBodies {
    policies: BTreeMap<String, Value>,
    operator_sections: Option<BTreeSet<String>>,
}

/// Signature/shape verification shared by strict loading and explicit cuts.
/// Raw bodies never grant launch authority without full section compilation.
fn read_signed_policy_bodies(
    app_root: &Path,
    directory: &lillux::PinnedDirectory,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<SignedPolicyBodies> {
    let node_fingerprint = crate::identity::node_identity_fingerprint(app_root)?;
    let entries = directory.entries_no_follow_bounded(MAX_POLICY_FILES)?;
    let mut policies = BTreeMap::new();
    let mut operator_sections = None;
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
        let is_operator_record = entry.name == OPERATOR_SECTIONS_FILE;
        if !is_operator_record {
            validate_policy_name("node policy section", &section_name)?;
        }
        let file = directory
            .open_pinned_regular(&entry.name, false)?
            .with_context(|| format!("node policy disappeared: {}", path.display()))?;
        let verified =
            crate::node_document::verify_pinned_signed_yaml(&file, trust_store, &node_fingerprint)?;
        if is_operator_record {
            operator_sections = Some(
                parse_operator_sections(&verified.body)
                    .with_context(|| format!("read {}", path.display()))?,
            );
            continue;
        }
        if policies
            .insert(section_name.clone(), verified.body)
            .is_some()
        {
            bail!("duplicate node policy section `{section_name}`");
        }
    }
    Ok(SignedPolicyBodies {
        policies,
        operator_sections,
    })
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
    let current_node_fingerprint = crate::identity::node_identity_fingerprint(app_root)?;
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
            let observed = read_signed_policy_bodies(app_root, current, trust_store)?;
            let found = generation_digest(&observed.policies, observed.operator_sections.as_ref())?;
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
        if !node_root
            .remove_empty_child_if_same(&staging_name, &retired)
            .map(|receipt| receipt.is_some())?
        {
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
            staging.atomic_write_pinned_if_same(
                &filename,
                None,
                &bytes,
                0o600,
                crate::NODE_RUNTIME_TEMP_PURPOSE,
            )?;
        }
        if let Some(sections) = &update.generation.operator_sections {
            let bytes = crate::node_document::render_signed_item(
                "operator_sections",
                "policy",
                &operator_sections_body(sections),
                identity,
            )?;
            staging.atomic_write_pinned_if_same(
                OsStr::new(OPERATOR_SECTIONS_FILE),
                None,
                &bytes,
                0o600,
                crate::NODE_RUNTIME_TEMP_PURPOSE,
            )?;
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
                        .map(|receipt| receipt.is_some())
                        .and_then(|removed| {
                            if removed {
                                Ok(())
                            } else {
                                bail!("retired node policy generation remained non-empty")
                            }
                        })
                });
            if let Err(error) = cleanup {
                tracing::warn!(error = %format!("{error:#}"), "node policy generation committed with retired staging cleanup pending");
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
            let _ = node_root
                .remove_empty_child_if_same(&staging_name, &candidate)
                .map(|receipt| receipt.is_some());
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
            ryeos_engine::test_support::workspace_root().join("bundles/.ai/node/init/profiles");
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
                .validate(&table, &path)
                .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
            let host_classes = profile.host_classes().unwrap();
            assert!(!host_classes.is_empty(), "{}", path.display());
            for host_class in host_classes {
                profile
                    .for_host_class(&host_class)
                    .and_then(|selected| selected.validated_generation(&table, &path))
                    .unwrap_or_else(|error| panic!("{} {host_class}: {error:#}", path.display()));
            }
        }
    }

    fn authored_profile(name: &str) -> NodeInitProfile {
        let raw = std::fs::read_to_string(
            ryeos_engine::test_support::workspace_root()
                .join(format!("bundles/.ai/node/init/profiles/{name}.yaml")),
        )
        .unwrap();
        serde_yaml::from_str(&lillux::signature::strip_signature_lines(&raw)).unwrap()
    }

    #[test]
    fn declared_import_roots_are_admitted_into_the_generation() {
        let table = NodePolicyTable::new();
        let app_root = tempfile::tempdir().unwrap();
        let mut profile = authored_profile("core").for_host_class("local").unwrap();
        let mut body = serde_json::json!({
            "schema": 1,
            "roots": {},
            "declared_roots": {"inline": {"path": "{app_root}/imports/inline"}},
            "limits": {
                "max_depth": 8,
                "max_entries": 8,
                "max_file_bytes": 1024,
                "max_total_bytes": 1024,
                "store_budget_bytes": 2048,
                "minimum_free_bytes": 1024
            },
            "managed_activation": {"enabled": false, "limits": null}
        });
        profile.policies.insert(
            super::super::sections::external_content::SECTION_NAME.to_owned(),
            body.take(),
        );
        let admitted = profile.admit_import_roots(app_root.path()).unwrap();
        let generation = admitted
            .validated_generation(&table, Path::new("inline.yaml"))
            .unwrap();
        let roots = &generation.policies()["external_content"]["roots"];
        assert_eq!(
            roots["inline"]["path"],
            serde_json::json!(app_root.path().join("imports/inline").to_string_lossy())
        );
        assert!(roots["inline"]["root_identity"]["inode"].as_u64().unwrap() != 0);
    }

    #[test]
    fn f5r_controller_declares_products_root_admitted_at_init() {
        let table = NodePolicyTable::new();
        let app_root = tempfile::tempdir().unwrap();
        let selected = authored_profile("f5r-controller")
            .for_host_class("hosted")
            .unwrap();
        let admitted = selected.admit_import_roots(app_root.path()).unwrap();
        let generation = admitted
            .validated_generation(&table, Path::new("f5r-controller.yaml"))
            .unwrap();
        let roots = &generation.policies()["external_content"]["roots"];
        assert_eq!(
            roots["products"]["path"],
            serde_json::json!(app_root.path().join("imports/products").to_string_lossy())
        );
        assert!(
            roots["products"]["root_identity"]["inode"]
                .as_u64()
                .unwrap()
                != 0
        );
    }

    #[test]
    fn composition_profiles_declare_a_local_and_a_hosted_class_and_init_selects_one() {
        let table = NodePolicyTable::new();
        let path = Path::new("profile.yaml");
        let declared: [(&str, &[&str]); 4] = [
            ("core", &["hosted", "local"]),
            ("core-scheduling", &["hosted", "local"]),
            ("f5r-controller", &["hosted", "hosted_enforced", "local"]),
            ("standard", &["hosted", "local"]),
        ];
        for (name, classes) in declared {
            let profile = authored_profile(name);
            assert_eq!(profile.host_classes().unwrap(), classes, "{name}");
            let listed = classes.join(", ");
            // The unselected profile is not a node generation.
            let error = format!(
                "{:#}",
                profile.validated_generation(&table, path).unwrap_err()
            );
            assert!(error.contains("exactly one host class"), "{name}: {error}");
            let refused = format!("{:#}", profile.for_host_class("edge").unwrap_err());
            assert!(
                refused.contains("does not declare host class `edge`") && refused.contains(&listed),
                "{name}: {refused}"
            );
            for host_class in classes.iter().copied() {
                let selected = profile.for_host_class(host_class).unwrap();
                assert_eq!(selected.host_classes().unwrap(), [host_class]);
                let policy: ryeos_engine::isolation::IsolationPolicy =
                    selected.typed_policy(&table, path).unwrap();
                assert_eq!(policy.host_class, host_class);
                // Every policy other than isolation is the profile's own.
                for (section, body) in profile.policies() {
                    if section != "isolation" {
                        assert_eq!(&selected.policies()[section], body);
                    }
                }
            }
        }
        // The hosted floor names the weaker grades a hosted container can
        // prove and never the per-scope guarantees only a qualified host
        // supervisor provides.
        let hosted: ryeos_engine::isolation::IsolationPolicy = authored_profile("core")
            .for_host_class("hosted")
            .unwrap()
            .typed_policy(&table, path)
            .unwrap();
        let names = hosted
            .requires_capabilities
            .iter()
            .map(|entry| entry.label())
            .collect::<Vec<_>>();
        assert!(
            names.contains(&"any_of(accounting, residency:swap_absent)".to_owned()),
            "{names:?}"
        );
        assert!(
            names
                .iter()
                .any(|label| label.contains("termination:enclosure_trusted_init")),
            "{names:?}"
        );
        assert!(
            hosted.required_process_scope_capabilities().is_empty(),
            "{names:?}"
        );
        let local: ryeos_engine::isolation::IsolationPolicy = authored_profile("core")
            .for_host_class("local")
            .unwrap()
            .typed_policy(&table, path)
            .unwrap();
        assert!(!local.required_process_scope_capabilities().is_empty());
        // A profile without a hosted variant declares only its local class.
        assert_eq!(authored_profile("full").host_classes().unwrap(), ["local"]);
        assert_eq!(
            authored_profile("hosted-node").host_classes().unwrap(),
            ["hosted"]
        );
    }

    #[test]
    fn explicit_member_cut_preserves_other_policies_and_checks_exact_predecessor() {
        use ryeos_engine::trust::{TrustStore, TrustedSigner};
        let root = tempfile::tempdir().unwrap();
        let node = root.path().join(ryeos_engine::AI_DIR).join("node");
        let identity_path = node.join("identity/private_key.pem");
        std::fs::create_dir_all(identity_path.parent().unwrap()).unwrap();
        let identity = NodeIdentity::create(&identity_path).unwrap();
        let trust = TrustStore::from_signers(vec![TrustedSigner {
            fingerprint: identity.fingerprint().to_owned(),
            verifying_key: *identity.verifying_key(),
            label: None,
        }]);
        let directory = policy_directory(root.path());
        std::fs::create_dir_all(&directory).unwrap();
        let profile_path = ryeos_engine::test_support::workspace_root()
            .join("bundles/.ai/node/init/profiles/full.yaml");
        let profile: NodeInitProfile =
            serde_yaml::from_str(&std::fs::read_to_string(profile_path).unwrap()).unwrap();
        let mut policies = profile.policies;
        let replacement = policies["isolation"].clone();
        policies.insert(
            "isolation".into(),
            serde_json::json!({"retired_shape": true}),
        );
        for (section, body) in &policies {
            std::fs::write(
                directory.join(format!("{section}.yaml")),
                crate::node_document::render_signed_item(section, "policy", body, &identity)
                    .unwrap(),
            )
            .unwrap();
        }
        let table = NodePolicyTable::new();
        assert!(load_policy_generation(root.path(), &trust, &table).is_err());
        let source = root.path().join("replacement.yaml");
        let update = prepare_policy_member_replacement(
            root.path(),
            &trust,
            &table,
            "local",
            "isolation",
            replacement.clone(),
            &source,
        )
        .unwrap();
        for (section, body) in &policies {
            if section != "isolation" {
                assert_eq!(&update.generation.policies[section], body);
            }
        }
        // The node's host class is fixed: a replacement isolation policy for
        // another class is refused, and so is any replacement on a node whose
        // binding names a class the resulting policy is not for.
        let mut reclassed = replacement.clone();
        let classes = reclassed["policy"]["host_classes"].as_object_mut().unwrap();
        let local = classes.remove("local").unwrap();
        classes.insert("hosted".to_owned(), local);
        let refused = format!(
            "{:#}",
            prepare_policy_member_replacement(
                root.path(),
                &trust,
                &table,
                "local",
                "isolation",
                reclassed,
                &source,
            )
            .err()
            .expect("a replacement for another host class is refused")
        );
        assert!(
            refused.contains("for host class `hosted`")
                && refused.contains("initialized under host class `local`"),
            "{refused}"
        );
        let refused = format!(
            "{:#}",
            prepare_policy_member_replacement(
                root.path(),
                &trust,
                &table,
                "hosted",
                "isolation",
                replacement.clone(),
                &source,
            )
            .err()
            .expect("a replacement for another host class is refused")
        );
        assert!(
            refused.contains("a node's host class does not change"),
            "{refused}"
        );
        // A different obsolete body is still a different predecessor. Exact
        // byte authority matters even though the retired schema is opaque.
        std::fs::write(
            directory.join("isolation.yaml"),
            crate::node_document::render_signed_item(
                "isolation",
                "policy",
                &serde_json::json!({"different_retired_shape": true}),
                &identity,
            )
            .unwrap(),
        )
        .unwrap();
        let lock = crate::state_lock::StateLock::acquire(&crate::state_lock::default_lock_path(
            root.path(),
        ))
        .unwrap();
        let error =
            publish_policy_update(root.path(), &update, &identity, &trust, &lock).unwrap_err();
        assert!(
            error.to_string().contains("changed before publication"),
            "{error:#}"
        );
        let update = prepare_policy_member_replacement(
            root.path(),
            &trust,
            &table,
            "local",
            "isolation",
            replacement,
            &source,
        )
        .unwrap();
        publish_policy_update(root.path(), &update, &identity, &trust, &lock).unwrap();
        assert_eq!(
            load_policy_generation(root.path(), &trust, &table).unwrap(),
            update.generation
        );
    }

    struct CarryFixture {
        root: tempfile::TempDir,
        identity: NodeIdentity,
        trust: ryeos_engine::trust::TrustStore,
        table: NodePolicyTable,
        profile: NodePolicyGeneration,
    }

    impl CarryFixture {
        fn new() -> Self {
            use ryeos_engine::trust::{TrustStore, TrustedSigner};
            let root = tempfile::tempdir().unwrap();
            let identity_path = root
                .path()
                .join(ryeos_engine::AI_DIR)
                .join("node/identity/private_key.pem");
            std::fs::create_dir_all(identity_path.parent().unwrap()).unwrap();
            let identity = NodeIdentity::create(&identity_path).unwrap();
            let trust = TrustStore::from_signers(vec![TrustedSigner {
                fingerprint: identity.fingerprint().to_owned(),
                verifying_key: *identity.verifying_key(),
                label: None,
            }]);
            let table = NodePolicyTable::new();
            let profile = authored_profile("full")
                .for_host_class("local")
                .unwrap()
                .validated_generation(&table, Path::new("profile.yaml"))
                .unwrap();
            Self {
                root,
                identity,
                trust,
                table,
                profile,
            }
        }

        /// Install a generation as it lies on disk, with or without a record.
        fn install(&self, policies: &BTreeMap<String, Value>, record: Option<&[&str]>) {
            let directory = policy_directory(self.root.path());
            if directory.exists() {
                std::fs::remove_dir_all(&directory).unwrap();
            }
            std::fs::create_dir_all(&directory).unwrap();
            for (section, body) in policies {
                std::fs::write(
                    directory.join(format!("{section}.yaml")),
                    crate::node_document::render_signed_item(
                        section,
                        "policy",
                        body,
                        &self.identity,
                    )
                    .unwrap(),
                )
                .unwrap();
            }
            if let Some(record) = record {
                let sections = record.iter().map(|name| (*name).to_owned()).collect();
                std::fs::write(
                    directory.join(OPERATOR_SECTIONS_FILE),
                    crate::node_document::render_signed_item(
                        "operator_sections",
                        "policy",
                        &operator_sections_body(&sections),
                        &self.identity,
                    )
                    .unwrap(),
                )
                .unwrap();
            }
        }

        fn cut(&self, choices: &PolicyCarryChoices) -> Result<NodePolicyGeneration> {
            prepare_schema_cut_carrying_operator_policy(
                self.root.path(),
                &self.trust,
                &self.table,
                "local",
                self.profile.clone(),
                choices,
                Path::new("profile.yaml"),
            )
            .map(|update| update.generation)
        }
    }

    fn operator_ingest_ignore() -> Value {
        serde_json::json!({"schema": 2, "patterns": ["operator-only/", "target/"]})
    }

    fn sections(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_replacement_carries_recorded_operator_sections_and_retakes_the_rest() {
        let fixture = CarryFixture::new();
        let mut installed = fixture.profile.policies().clone();
        installed.insert("ingest_ignore".into(), operator_ingest_ignore());
        // A profile section in a retired shape is retaken, not interpreted.
        installed.insert(
            "isolation".into(),
            serde_json::json!({"retired_shape": true}),
        );
        fixture.install(&installed, Some(&["ingest_ignore"]));

        let carried = fixture.cut(&PolicyCarryChoices::default()).unwrap();
        assert_eq!(
            carried.policies()["ingest_ignore"],
            operator_ingest_ignore()
        );
        assert_eq!(
            carried.policies()["isolation"],
            fixture.profile.policies()["isolation"]
        );
        assert_eq!(
            carried.operator_sections(),
            Some(&sections(&["ingest_ignore"]))
        );

        // A rerun after the exchange computes the same generation.
        fixture.install(carried.policies(), Some(&["ingest_ignore"]));
        assert_eq!(
            fixture
                .cut(&PolicyCarryChoices::default())
                .unwrap()
                .digest(),
            carried.digest()
        );
        assert_eq!(
            load_policy_generation(fixture.root.path(), &fixture.trust, &fixture.table)
                .unwrap()
                .digest(),
            carried.digest()
        );

        // Resetting a recorded section takes the profile's and forgets it.
        let reset = fixture
            .cut(&PolicyCarryChoices {
                reset: sections(&["ingest_ignore"]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            reset.policies()["ingest_ignore"],
            fixture.profile.policies()["ingest_ignore"]
        );
        assert_eq!(reset.operator_sections(), Some(&BTreeSet::new()));
    }

    #[test]
    fn a_replacement_names_every_operator_section_that_no_longer_validates() {
        let fixture = CarryFixture::new();
        let mut installed = fixture.profile.policies().clone();
        installed.insert(
            "ingest_ignore".into(),
            serde_json::json!({"retired_shape": true}),
        );
        fixture.install(&installed, Some(&["ingest_ignore"]));
        let refused = format!(
            "{:#}",
            fixture
                .cut(&PolicyCarryChoices::default())
                .expect_err("a carried section must validate")
        );
        assert!(
            refused.contains("`ingest_ignore`") && refused.contains("nothing was changed"),
            "{refused}"
        );
        // The explicit reset takes the profile's instead.
        assert!(
            fixture
                .cut(&PolicyCarryChoices {
                    reset: sections(&["ingest_ignore"]),
                    ..Default::default()
                })
                .is_ok()
        );
    }

    #[test]
    fn an_unrecorded_generation_asks_about_every_section_that_differs() {
        let fixture = CarryFixture::new();
        let mut installed = fixture.profile.policies().clone();
        installed.insert("ingest_ignore".into(), operator_ingest_ignore());
        fixture.install(&installed, None);
        let installed_generation =
            load_policy_generation(fixture.root.path(), &fixture.trust, &fixture.table).unwrap();
        assert_eq!(installed_generation.operator_sections(), None);

        let refused = format!(
            "{:#}",
            fixture
                .cut(&PolicyCarryChoices::default())
                .expect_err("an unrecorded difference is never reset silently")
        );
        assert!(
            refused.contains("ingest_ignore") && refused.contains("--keep-section"),
            "{refused}"
        );

        let kept = fixture
            .cut(&PolicyCarryChoices {
                keep: sections(&["ingest_ignore"]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(kept.policies()["ingest_ignore"], operator_ingest_ignore());
        assert_eq!(
            kept.operator_sections(),
            Some(&sections(&["ingest_ignore"]))
        );

        let reset = fixture
            .cut(&PolicyCarryChoices {
                reset: sections(&["ingest_ignore"]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(reset.digest(), fixture.profile.digest());

        assert!(
            fixture
                .cut(&PolicyCarryChoices {
                    keep: sections(&["ingest_ignore"]),
                    reset: sections(&["ingest_ignore"]),
                })
                .is_err()
        );
    }

    #[test]
    fn a_generation_without_a_record_keeps_its_bodies_only_digest() {
        let fixture = CarryFixture::new();
        let policies = fixture.profile.policies().clone();
        fixture.install(&policies, None);
        let unrecorded =
            load_policy_generation(fixture.root.path(), &fixture.trust, &fixture.table).unwrap();
        let bodies_only =
            ryeos_state::objects::canonical_value_digest(&serde_json::to_value(&policies).unwrap())
                .unwrap();
        assert_eq!(unrecorded.digest(), bodies_only);
        fixture.install(&policies, Some(&[]));
        let recorded =
            load_policy_generation(fixture.root.path(), &fixture.trust, &fixture.table).unwrap();
        assert_ne!(recorded.digest(), bodies_only);
        assert_eq!(recorded.digest(), fixture.profile.digest());
    }

    #[test]
    fn policy_apply_records_its_section_only_where_a_record_exists() {
        let fixture = CarryFixture::new();
        let policies = fixture.profile.policies().clone();
        let apply = || {
            prepare_policy_member_replacement(
                fixture.root.path(),
                &fixture.trust,
                &fixture.table,
                "local",
                "ingest_ignore",
                operator_ingest_ignore(),
                Path::new("policy.yaml"),
            )
            .unwrap()
            .generation
        };
        fixture.install(&policies, Some(&[]));
        assert_eq!(
            apply().operator_sections(),
            Some(&sections(&["ingest_ignore"]))
        );
        // Recording only this section on an unrecorded generation would let
        // the next replacement drop earlier, unknown operator changes.
        fixture.install(&policies, None);
        assert_eq!(apply().operator_sections(), None);
    }

    #[test]
    fn a_generation_missing_a_registered_section_refuses_with_the_cure() {
        let fixture = CarryFixture::new();
        let section = crate::node_policy::sections::event_storage::SECTION_NAME;
        let mut installed = fixture.profile.policies().clone();
        installed.remove(section);
        fixture.install(&installed, Some(&[]));
        let error = load_policy_generation(fixture.root.path(), &fixture.trust, &fixture.table)
            .expect_err("a generation without a registered section does not load");
        let missing = error
            .downcast_ref::<NodePolicySectionsMissing>()
            .expect("the refusal is typed");
        assert_eq!(missing.sections, vec![section.to_owned()]);
        let message = error.to_string();
        assert!(
            message.contains(POLICY_GENERATION_RESET_COMMAND) && message.contains(section),
            "{message}"
        );
    }

    #[test]
    fn a_replacement_takes_a_newly_required_section_from_the_profile() {
        let fixture = CarryFixture::new();
        let section = crate::node_policy::sections::event_storage::SECTION_NAME;
        let mut installed = fixture.profile.policies().clone();
        installed.remove(section);
        installed.insert("ingest_ignore".into(), operator_ingest_ignore());

        // Recorded generation: the operator's section carries forward and the
        // new section comes from the profile, with or without naming it.
        fixture.install(&installed, Some(&["ingest_ignore"]));
        for choices in [
            PolicyCarryChoices::default(),
            PolicyCarryChoices {
                reset: sections(&[section]),
                ..Default::default()
            },
        ] {
            let cut = fixture.cut(&choices).unwrap();
            assert_eq!(cut.policies()[section], fixture.profile.policies()[section]);
            assert_eq!(cut.policies()["ingest_ignore"], operator_ingest_ignore());
            assert_eq!(cut.operator_sections(), Some(&sections(&["ingest_ignore"])));
        }
        // A section the installed generation lacks cannot be kept.
        let refused = fixture
            .cut(&PolicyCarryChoices {
                keep: sections(&[section]),
                ..Default::default()
            })
            .expect_err("an absent section cannot be kept")
            .to_string();
        assert!(refused.contains("section to keep"), "{refused}");

        // Unrecorded generation: only sections it holds that differ from the
        // profile need a choice; the new section is not one of them.
        fixture.install(&installed, None);
        let refused = format!(
            "{:#}",
            fixture
                .cut(&PolicyCarryChoices::default())
                .expect_err("the unrecorded difference still needs a choice")
        );
        assert!(
            refused.contains("ingest_ignore") && !refused.contains(section),
            "{refused}"
        );
        let cut = fixture
            .cut(&PolicyCarryChoices {
                keep: sections(&["ingest_ignore"]),
                reset: sections(&[section]),
            })
            .unwrap();
        assert_eq!(cut.policies()[section], fixture.profile.policies()[section]);
        assert_eq!(cut.policies()["ingest_ignore"], operator_ingest_ignore());
    }
}
