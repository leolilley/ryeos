//! Trusted acquisition recipes for external content that is intentionally not
//! shipped in an installed bundle.
//!
//! The portable recipe says only how to obtain exact bytes and which existing
//! consumer declaration each member supplies. The consumer remains authority
//! for realization ID, file/tree kind, pinned manifest digest, and mount. Node
//! policy separately controls whether and within what limits acquisition may
//! run on this site.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::managed_external_content_operation::AcquisitionMode;
use crate::node_policy::sections::external_content::{
    ExternalContentImportLimits, ManagedExternalContentActivationPolicy,
};

pub const MANAGED_ACTIVATION_SCHEMA: &str = "ryeos.external_content_activation.v3";
pub const MANAGED_ACTIVATION_ARCHIVE_FORMAT: &str = "tar_gzip";
const MAX_PORTABLE_ARCHIVES: usize = 8;
const MAX_PORTABLE_MEMBERS: usize = 1024;
const MAX_PORTABLE_ARCHIVE_ENTRIES: usize = ryeos_state::objects::MAX_EXTERNAL_CONTENT_ENTRIES + 1;
const MAX_MAPPED_ACTIVATION_STAGING_ENTRIES: usize = MAX_PORTABLE_MEMBERS
    * (ryeos_state::external_content::MAX_CAPTURE_DEPTH + 1)
    + ryeos_state::objects::MAX_EXTERNAL_CONTENT_ACTIVATION_COMPONENTS;
const MAX_WHOLE_TREE_ACTIVATION_STAGING_ENTRIES: usize = MAX_PORTABLE_ARCHIVES
    * MAX_PORTABLE_ARCHIVE_ENTRIES
    + ryeos_state::objects::MAX_EXTERNAL_CONTENT_ACTIVATION_COMPONENTS;
// One portable document may use mapped and whole-tree shapes on different
// sources. Cleanup/reset must therefore cover their joint legal namespace,
// not merely the larger shape in isolation.
pub const MAX_MANAGED_ACTIVATION_STAGING_ENTRIES: usize =
    MAX_MAPPED_ACTIVATION_STAGING_ENTRIES + MAX_WHOLE_TREE_ACTIVATION_STAGING_ENTRIES;

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedMemberDisposition {
    Import,
    VerifyOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedComponentStorage {
    Content,
    LargeContent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationMember {
    pub path: String,
    pub disposition: ManagedMemberDisposition,
    pub sha256: String,
    pub maximum_bytes: u64,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationSource {
    pub id: String,
    pub url: String,
    pub archive_format: String,
    pub sha256: String,
    pub maximum_compressed_bytes: u64,
    pub maximum_expanded_bytes: u64,
    pub maximum_entries: usize,
    #[serde(default)]
    pub members: Vec<ManagedActivationMember>,
}

/// One selected archive member placed into a consumer realization. File-shaped
/// consumers require exactly one mapping with no target. Tree-shaped consumers
/// require a canonical target for every mapping; acquisition creates only
/// those regular files and their parent directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationComponentMember {
    pub source: String,
    pub member: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub target: Option<String>,
}

/// Signed capture ceilings for an exact publisher-produced archive subtree.
/// These are acquisition bounds, not manifest authority: the resolved
/// consumer still supplies the required manifest kind and digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationComponentBounds {
    pub maximum_entries: usize,
    pub maximum_depth: usize,
    pub maximum_file_bytes: u64,
    pub maximum_total_bytes: u64,
}

/// The two deliberately closed ways acquisition may materialize one existing
/// consumer declaration. `Mapped` selects exact regular members. A
/// `WholeArchiveTree` strips one canonical archive prefix and admits only its
/// already-final directory, regular-file, and internal-relative-symlink
/// entries. Neither shape can run a transform or author bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedActivationComponentShape {
    Mapped {
        members: Vec<ManagedActivationComponentMember>,
    },
    WholeArchiveTree {
        source: String,
        prefix: String,
        bounds: ManagedActivationComponentBounds,
    },
}

/// Selected acquisition members mapped to one existing consumer
/// external-content ID. Kind, pinned manifest digest, schema, and mount are
/// deliberately absent: admission derives them from the resolved consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationComponent {
    pub id: String,
    pub storage: ManagedComponentStorage,
    pub shape: ManagedActivationComponentShape,
}

impl ManagedActivationComponent {
    pub fn mapped_members(&self) -> Option<&[ManagedActivationComponentMember]> {
        match &self.shape {
            ManagedActivationComponentShape::Mapped { members } => Some(members),
            ManagedActivationComponentShape::WholeArchiveTree { .. } => None,
        }
    }

    pub fn whole_archive_tree(&self) -> Option<(&str, &str, &ManagedActivationComponentBounds)> {
        match &self.shape {
            ManagedActivationComponentShape::WholeArchiveTree {
                source,
                prefix,
                bounds,
            } => Some((source, prefix, bounds)),
            ManagedActivationComponentShape::Mapped { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedExternalContentActivation {
    pub schema: String,
    pub consumer_ref: String,
    pub sources: Vec<ManagedActivationSource>,
    pub components: Vec<ManagedActivationComponent>,
}

#[derive(Debug, Clone)]
pub struct ResolvedManagedActivationComponent {
    pub recipe: ManagedActivationComponent,
    pub expected_manifest_hash: String,
    pub expected_manifest_kind: String,
    pub declaration_kind: ryeos_engine::external_content::ExternalContentKind,
    pub capture_bounds: ManagedActivationComponentBounds,
    pub expected_file_sha256: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedManagedExternalContentActivation {
    pub activation_ref: String,
    pub activation_program_digest: String,
    pub publisher_fingerprint: String,
    pub document: ManagedExternalContentActivation,
    pub components: Vec<ResolvedManagedActivationComponent>,
}

impl ManagedExternalContentActivation {
    /// Compile the portable signed recipe without consulting this node.
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let mut payload = value.clone();
        if let Some(category) = payload
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("managed external-content config must be an object"))?
            .remove("category")
            && !category.is_string()
        {
            bail!("managed external-content config category must be a string");
        }
        let document: Self = serde_json::from_value(payload)
            .context("parse managed external-content acquisition config")?;
        document.validate_portable()?;
        Ok(document)
    }

    pub fn validate_portable(&self) -> anyhow::Result<()> {
        if self.schema != MANAGED_ACTIVATION_SCHEMA {
            bail!("managed external-content activation schema is not current");
        }
        validate_canonical_ref("activation consumer ref", &self.consumer_ref)?;
        if self.sources.is_empty() || self.sources.len() > MAX_PORTABLE_ARCHIVES {
            bail!("managed activation source count exceeds the portable contract");
        }
        if self.components.is_empty()
            || self.components.len()
                > ryeos_state::objects::MAX_EXTERNAL_CONTENT_ACTIVATION_COMPONENTS
        {
            bail!("managed activation component count is outside the supported range");
        }

        let mut source_ids = BTreeSet::new();
        let mut source_members = BTreeMap::<(&str, &str), &ManagedActivationMember>::new();
        let mut imported_members = BTreeSet::new();
        let mut total_members = 0usize;
        for source in &self.sources {
            validate_id("activation source id", &source.id)?;
            if !source_ids.insert(source.id.as_str()) {
                bail!("managed activation repeats a source id");
            }
            validate_portable_source_url(&source.url)?;
            if source.archive_format != MANAGED_ACTIVATION_ARCHIVE_FORMAT {
                bail!("managed activation source archive format is unsupported");
            }
            validate_hash("activation archive digest", &source.sha256)?;
            if source.maximum_compressed_bytes == 0
                || source.maximum_expanded_bytes == 0
                || source.maximum_compressed_bytes
                    > ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES
                || source.maximum_expanded_bytes
                    > ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES
            {
                bail!("managed activation archive bounds exceed the portable contract");
            }
            if source.maximum_entries == 0 || source.maximum_entries > MAX_PORTABLE_ARCHIVE_ENTRIES
            {
                bail!("managed activation archive entry bound exceeds the portable contract");
            }
            total_members = total_members
                .checked_add(source.members.len())
                .ok_or_else(|| anyhow::anyhow!("activation member ceiling overflow"))?;
            for member in &source.members {
                validate_member_path(&member.path)?;
                validate_hash("activation member digest", &member.sha256)?;
                if member.maximum_bytes == 0
                    || member.maximum_bytes > ryeos_state::objects::MAX_LARGE_CONTENT_FILE_BYTES
                {
                    bail!("managed activation member bound exceeds the portable contract");
                }
                if source_members
                    .insert((source.id.as_str(), member.path.as_str()), member)
                    .is_some()
                {
                    bail!("managed activation repeats a source member");
                }
                if member.disposition == ManagedMemberDisposition::Import {
                    imported_members.insert((source.id.as_str(), member.path.as_str()));
                }
            }
        }
        if total_members > MAX_PORTABLE_MEMBERS {
            bail!("managed activation selected-member count exceeds the portable contract");
        }

        let mut component_ids = BTreeSet::new();
        let mut consumed_imports = BTreeSet::new();
        let mut mapped_sources = BTreeSet::new();
        let mut whole_tree_sources = BTreeSet::new();
        for component in &self.components {
            validate_id("activation component id", &component.id)?;
            if !component_ids.insert(component.id.as_str()) {
                bail!("managed activation repeats a component id");
            }
            match &component.shape {
                ManagedActivationComponentShape::Mapped { members } => {
                    validate_mapped_component(
                        component,
                        members,
                        &source_members,
                        &mut consumed_imports,
                        &mut mapped_sources,
                    )?;
                }
                ManagedActivationComponentShape::WholeArchiveTree {
                    source,
                    prefix,
                    bounds,
                } => {
                    validate_id("whole-tree activation source", source)?;
                    validate_member_path(prefix)?;
                    if prefix.len() > ryeos_state::objects::MAX_EXTERNAL_CONTENT_PATH_BYTES {
                        bail!("whole-tree activation prefix exceeds the portable path bound");
                    }
                    validate_component_bounds(bounds, component.storage)?;
                    let source_recipe = self.source_by_id(source)?;
                    if !source_recipe.members.is_empty() {
                        bail!("whole-tree activation source cannot also select members");
                    }
                    if source_recipe.maximum_entries < bounds.maximum_entries.saturating_add(1) {
                        bail!("whole-tree activation source entry bound cannot contain its tree");
                    }
                    if !whole_tree_sources.insert(source.as_str()) {
                        bail!("whole-tree activation source is consumed more than once");
                    }
                }
            }
        }
        if !mapped_sources.is_disjoint(&whole_tree_sources) {
            bail!("managed activation source cannot mix mapped and whole-tree consumption");
        }
        for source in &self.sources {
            if source.members.is_empty() && !whole_tree_sources.contains(source.id.as_str()) {
                bail!("managed activation source has neither selected members nor a whole tree");
            }
        }
        if consumed_imports != imported_members {
            bail!("every imported activation member must map to exactly one component");
        }
        Ok(())
    }

    /// Admit the portable recipe against this node and the already-resolved
    /// consumer. Repeated facts are derived here and retained only as compiled
    /// assertions for the import/bind operation.
    pub fn admit(
        &self,
        acquisition_mode: AcquisitionMode,
        policy: &ManagedExternalContentActivationPolicy,
        import_limits: &ExternalContentImportLimits,
        declarations: &[ryeos_engine::external_content::ExternalContentDeclaration],
        large_content_supported: bool,
    ) -> anyhow::Result<Vec<ResolvedManagedActivationComponent>> {
        self.validate_portable()?;
        policy.validate()?;
        import_limits.validate()?;
        if self.sources.len() > policy.max_archives {
            bail!("managed activation archive count exceeds node policy");
        }
        let mut total_compressed = 0u64;
        let mut total_expanded = 0u64;
        let mut total_entries = 0usize;
        for source in &self.sources {
            if acquisition_mode == AcquisitionMode::Online {
                if !policy.allow_online {
                    bail!(
                        "node policy does not permit online managed external-content acquisition"
                    );
                }
                admit_source_url(&source.url, policy)?;
            }
            if source.maximum_compressed_bytes > policy.max_compressed_bytes
                || source.maximum_expanded_bytes > policy.max_expanded_bytes
            {
                bail!("managed activation archive bounds exceed node policy");
            }
            total_compressed = total_compressed
                .checked_add(source.maximum_compressed_bytes)
                .ok_or_else(|| anyhow::anyhow!("activation compressed byte ceiling overflow"))?;
            total_expanded = total_expanded
                .checked_add(source.maximum_expanded_bytes)
                .ok_or_else(|| anyhow::anyhow!("activation expanded byte ceiling overflow"))?;
            total_entries = total_entries
                .checked_add(source.maximum_entries)
                .ok_or_else(|| anyhow::anyhow!("activation archive entry ceiling overflow"))?;
            if source
                .members
                .iter()
                .any(|member| member.maximum_bytes > policy.max_member_bytes)
            {
                bail!("managed activation member bound exceeds node policy");
            }
        }
        if total_compressed > policy.max_compressed_bytes
            || total_expanded > policy.max_expanded_bytes
            || total_entries > policy.max_members
        {
            bail!("managed activation aggregate bounds exceed node policy");
        }

        let required = declarations
            .iter()
            .filter(|declaration| {
                declaration.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
                    && declaration.locator.is_none()
            })
            .map(|declaration| (declaration.id.as_str(), declaration))
            .collect::<BTreeMap<_, _>>();
        if required.len() != self.components.len() {
            bail!(
                "managed activation must supply every locator-free pinned consumer realization exactly once"
            );
        }
        let mut resolved = Vec::with_capacity(self.components.len());
        for component in &self.components {
            let declaration = required.get(component.id.as_str()).ok_or_else(|| {
                anyhow::anyhow!(
                    "managed activation component {} is not a pinned consumer realization",
                    component.id
                )
            })?;
            let (capture_bounds, expected_file_sha256) = match &component.shape {
                ManagedActivationComponentShape::Mapped { members } => {
                    derive_mapped_capture_bounds(self, members, declaration.kind)?
                }
                ManagedActivationComponentShape::WholeArchiveTree { bounds, .. } => {
                    if declaration.kind != ryeos_engine::external_content::ExternalContentKind::Tree
                    {
                        bail!("whole-archive-tree activation requires a tree consumer");
                    }
                    (bounds.clone(), None)
                }
            };
            if capture_bounds.maximum_file_bytes > policy.max_member_bytes {
                bail!("managed activation component file bound exceeds node policy");
            }
            if capture_bounds.maximum_depth > import_limits.max_depth
                || capture_bounds.maximum_entries > import_limits.max_entries
                || capture_bounds.maximum_file_bytes > import_limits.max_file_bytes
                || capture_bounds.maximum_total_bytes > import_limits.max_total_bytes
            {
                bail!("managed activation component exceeds node import policy");
            }
            let expected_manifest_hash = declaration
                .digest
                .clone()
                .ok_or_else(|| anyhow::anyhow!("pinned consumer realization has no digest"))?;
            let expected_manifest_kind = match component.storage {
                ManagedComponentStorage::Content => {
                    ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND
                }
                ManagedComponentStorage::LargeContent if large_content_supported => {
                    ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
                }
                ManagedComponentStorage::LargeContent => {
                    bail!("consumer kind has no signed large-content grant")
                }
            };
            resolved.push(ResolvedManagedActivationComponent {
                recipe: component.clone(),
                expected_manifest_hash,
                expected_manifest_kind: expected_manifest_kind.to_owned(),
                declaration_kind: declaration.kind,
                capture_bounds,
                expected_file_sha256,
            });
        }
        resolved.sort_by(|left, right| left.recipe.id.cmp(&right.recipe.id));
        Ok(resolved)
    }

    fn source_by_id(&self, id: &str) -> anyhow::Result<&ManagedActivationSource> {
        self.sources
            .iter()
            .find(|source| source.id == id)
            .ok_or_else(|| anyhow::anyhow!("managed activation component names an absent source"))
    }
}

fn validate_mapped_component<'a>(
    component: &'a ManagedActivationComponent,
    members: &'a [ManagedActivationComponentMember],
    source_members: &BTreeMap<(&'a str, &'a str), &'a ManagedActivationMember>,
    consumed_imports: &mut BTreeSet<(&'a str, &'a str)>,
    mapped_sources: &mut BTreeSet<&'a str>,
) -> anyhow::Result<()> {
    if members.is_empty() || members.len() > MAX_PORTABLE_MEMBERS {
        bail!("managed activation component member count is outside the portable contract");
    }
    let tree_candidate = members.iter().any(|mapping| mapping.target.is_some());
    if tree_candidate && members.iter().any(|mapping| mapping.target.is_none()) {
        bail!("tree-shaped activation component has a member without a target");
    }
    if !tree_candidate && members.len() != 1 {
        bail!("file-shaped activation component candidate must select exactly one member");
    }
    let mut targets = BTreeSet::new();
    let mut tree_entries = BTreeSet::new();
    let mut component_maximum_bytes = 0u64;
    for mapping in members {
        validate_id("activation component source", &mapping.source)?;
        validate_member_path(&mapping.member)?;
        mapped_sources.insert(mapping.source.as_str());
        if let Some(target) = &mapping.target {
            validate_member_path(target)?;
            if target.len() > ryeos_state::objects::MAX_EXTERNAL_CONTENT_PATH_BYTES {
                bail!("activation tree target exceeds the portable path bound");
            }
            if target.split('/').count() > ryeos_state::external_content::MAX_CAPTURE_DEPTH {
                bail!("activation tree target exceeds the portable depth bound");
            }
            if !targets.insert(target.as_str()) {
                bail!("activation tree component repeats a target path");
            }
            insert_tree_namespace(target, &mut tree_entries);
            if tree_entries.len() > ryeos_state::objects::MAX_EXTERNAL_CONTENT_ENTRIES {
                bail!("activation tree component exceeds the portable entry bound");
            }
        }
        let Some(member) = source_members.get(&(mapping.source.as_str(), mapping.member.as_str()))
        else {
            bail!("activation component names an absent source member");
        };
        if member.disposition != ManagedMemberDisposition::Import {
            bail!("activation component does not name an imported source member");
        }
        component_maximum_bytes = component_maximum_bytes
            .checked_add(member.maximum_bytes)
            .ok_or_else(|| anyhow::anyhow!("activation component byte bound overflow"))?;
        if component.storage == ManagedComponentStorage::Content
            && member.maximum_bytes > ryeos_state::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES
        {
            bail!("ordinary-content activation member has a large-content byte bound");
        }
        if !consumed_imports.insert((mapping.source.as_str(), mapping.member.as_str())) {
            bail!("an imported activation member is consumed more than once");
        }
    }
    if component.storage == ManagedComponentStorage::Content
        && component_maximum_bytes > ryeos_state::objects::MAX_EXTERNAL_CONTENT_TOTAL_BYTES
    {
        bail!("ordinary-content activation component has an excessive aggregate byte bound");
    }
    Ok(())
}

fn validate_component_bounds(
    bounds: &ManagedActivationComponentBounds,
    storage: ManagedComponentStorage,
) -> anyhow::Result<()> {
    let (maximum_entries, maximum_file_bytes, maximum_total_bytes) = match storage {
        ManagedComponentStorage::Content => (
            ryeos_state::objects::MAX_EXTERNAL_CONTENT_ENTRIES,
            ryeos_state::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES,
            ryeos_state::objects::MAX_EXTERNAL_CONTENT_TOTAL_BYTES,
        ),
        ManagedComponentStorage::LargeContent => (
            ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_ENTRIES,
            ryeos_state::objects::MAX_LARGE_CONTENT_FILE_BYTES,
            ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES,
        ),
    };
    if bounds.maximum_entries == 0
        || bounds.maximum_entries > maximum_entries
        || bounds.maximum_depth == 0
        || bounds.maximum_depth > ryeos_state::external_content::MAX_CAPTURE_DEPTH
        || bounds.maximum_file_bytes == 0
        || bounds.maximum_file_bytes > maximum_file_bytes
        || bounds.maximum_total_bytes == 0
        || bounds.maximum_total_bytes > maximum_total_bytes
        || bounds.maximum_file_bytes > bounds.maximum_total_bytes
    {
        bail!("whole-tree activation bounds exceed the portable storage contract");
    }
    Ok(())
}

fn derive_mapped_capture_bounds(
    document: &ManagedExternalContentActivation,
    members: &[ManagedActivationComponentMember],
    declaration_kind: ryeos_engine::external_content::ExternalContentKind,
) -> anyhow::Result<(ManagedActivationComponentBounds, Option<String>)> {
    let mut maximum_total_bytes = 0u64;
    let mut maximum_file_bytes = 0u64;
    let mut maximum_depth = 1usize;
    let mut tree_entries = BTreeSet::new();
    for mapping in members {
        let member = document.member(&mapping.source, &mapping.member)?;
        maximum_total_bytes = maximum_total_bytes
            .checked_add(member.maximum_bytes)
            .ok_or_else(|| anyhow::anyhow!("activation component byte bound overflow"))?;
        maximum_file_bytes = maximum_file_bytes.max(member.maximum_bytes);
        if let Some(target) = mapping.target.as_deref() {
            maximum_depth = maximum_depth.max(target.split('/').count());
            insert_tree_namespace(target, &mut tree_entries);
        }
    }
    let (maximum_entries, expected_file_sha256) = match declaration_kind {
        ryeos_engine::external_content::ExternalContentKind::File => {
            if members.len() != 1 || members[0].target.is_some() {
                bail!("managed activation file realization requires one untargeted member");
            }
            let member = document.member(&members[0].source, &members[0].member)?;
            (1, Some(member.sha256.clone()))
        }
        ryeos_engine::external_content::ExternalContentKind::Tree => {
            if members.iter().any(|mapping| mapping.target.is_none()) {
                bail!("managed activation tree realization requires targeted members");
            }
            let mut targets = members
                .iter()
                .map(|mapping| mapping.target.as_deref().expect("checked above"))
                .collect::<Vec<_>>();
            targets.sort_unstable();
            for (index, left) in targets.iter().enumerate() {
                for right in targets.iter().skip(index + 1) {
                    if path_contains(left, right) || path_contains(right, left) {
                        bail!("managed activation tree targets overlap");
                    }
                }
            }
            (tree_entries.len(), None)
        }
    };
    Ok((
        ManagedActivationComponentBounds {
            maximum_entries,
            maximum_depth,
            maximum_file_bytes,
            maximum_total_bytes,
        },
        expected_file_sha256,
    ))
}

impl ResolvedManagedExternalContentActivation {
    pub fn component(&self, id: &str) -> anyhow::Result<&ResolvedManagedActivationComponent> {
        self.components
            .iter()
            .find(|component| component.recipe.id == id)
            .ok_or_else(|| anyhow::anyhow!("managed activation component {id} is absent"))
    }

    pub fn source(&self, id: &str) -> anyhow::Result<&ManagedActivationSource> {
        self.document
            .sources
            .iter()
            .find(|source| source.id == id)
            .ok_or_else(|| anyhow::anyhow!("managed activation source {id} is absent"))
    }

    pub fn member(&self, source: &str, member: &str) -> anyhow::Result<&ManagedActivationMember> {
        self.source(source)?
            .members
            .iter()
            .find(|candidate| candidate.path == member)
            .ok_or_else(|| anyhow::anyhow!("managed activation component member is absent"))
    }
}

impl ManagedExternalContentActivation {
    fn member(&self, source: &str, member: &str) -> anyhow::Result<&ManagedActivationMember> {
        self.sources
            .iter()
            .find(|candidate| candidate.id == source)
            .and_then(|candidate| {
                candidate
                    .members
                    .iter()
                    .find(|candidate| candidate.path == member)
            })
            .ok_or_else(|| anyhow::anyhow!("managed activation component member is absent"))
    }
}

pub fn resolve_activation(
    state: &crate::state::AppState,
    activation_ref: &str,
    acquisition_mode: AcquisitionMode,
) -> anyhow::Result<ResolvedManagedExternalContentActivation> {
    let import_policy = state.node_policy.require::<
        crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord,
    >()?;
    let policy = import_policy
        .managed_activation
        .require_enabled()?;
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(activation_ref)
        .map_err(|error| anyhow::anyhow!("invalid activation ref: {error}"))?;
    if canonical.to_string() != activation_ref || canonical.kind != "config" {
        bail!("managed activation requires one canonical config ref");
    }
    let effective = state.engine.with_checked_bundle_generation(|generation| {
        generation.effective_item(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: canonical,
            expected_kind: Some("config".to_owned()),
            project_root: None,
            subject_resolution_authority:
                ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
        })
    })?;
    require_trusted_bundle_item(&effective, "managed activation config")?;
    let publisher_fingerprint = item_publisher(&effective, "managed activation config")?;
    let document = ManagedExternalContentActivation::from_value(&effective.composed_value)?;

    let consumer_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(&document.consumer_ref)
        .map_err(|error| anyhow::anyhow!("invalid activation consumer ref: {error}"))?;
    let consumer = state.engine.with_checked_bundle_generation(|generation| {
        generation.effective_item(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: consumer_ref,
            expected_kind: None,
            project_root: None,
            subject_resolution_authority:
                ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
        })
    })?;
    require_trusted_bundle_item(&consumer, "managed activation consumer")?;
    let consumer_publisher = item_publisher(&consumer, "managed activation consumer")?;
    if consumer_publisher != publisher_fingerprint {
        bail!("managed activation and consumer must share one trusted bundle publisher");
    }
    let external_contract = state
        .engine
        .kinds
        .get(&consumer.kind)
        .and_then(|kind| kind.external_content_contract())
        .ok_or_else(|| {
            anyhow::anyhow!("managed activation consumer kind has no external-content contract")
        })?;
    let declarations: Vec<ryeos_engine::external_content::ExternalContentDeclaration> =
        serde_json::from_value(
            consumer
                .composed_value
                .get("external_content")
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("managed activation consumer declares no external content")
                })?,
        )
        .context("parse resolved consumer external-content declarations")?;
    let components = document.admit(
        acquisition_mode,
        policy,
        &import_policy.limits,
        &declarations,
        external_contract.large_content.is_some(),
    )?;
    let activation_program_digest =
        ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "activation": {
                "canonical_ref": effective.canonical_ref,
                "kind": effective.kind,
                "publisher_fingerprint": publisher_fingerprint,
                "trust_class": effective.trust_class,
                "composed_value": effective.composed_value,
            },
            "consumer": {
                "canonical_ref": consumer.canonical_ref,
                "kind": consumer.kind,
                "publisher_fingerprint": consumer_publisher,
                "trust_class": consumer.trust_class,
                "external_content": declarations,
                "large_content_supported": external_contract.large_content.is_some(),
            }
        }))?;
    Ok(ResolvedManagedExternalContentActivation {
        activation_ref: effective.canonical_ref,
        activation_program_digest,
        publisher_fingerprint,
        document,
        components,
    })
}

fn require_trusted_bundle_item(
    item: &ryeos_engine::engine::EffectiveItem,
    label: &str,
) -> anyhow::Result<()> {
    if !item.trusted
        || item.trust_class != ryeos_engine::resolution::TrustClass::TrustedBundle
        || item.source.bundle_root.is_none()
    {
        bail!("{label} must be a trusted installed-bundle item");
    }
    Ok(())
}

fn item_publisher(
    item: &ryeos_engine::engine::EffectiveItem,
    label: &str,
) -> anyhow::Result<String> {
    item.provenance
        .root
        .signer_fingerprint
        .clone()
        .ok_or_else(|| anyhow::anyhow!("{label} has no publisher"))
}

fn validate_portable_source_url(value: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(value).context("parse managed activation source URL")?;
    if parsed.as_str() != value
        || parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.port().is_some()
    {
        bail!("managed activation source must be a canonical HTTPS URL");
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("managed activation source has no HTTPS host"))?;
    if host != host.to_ascii_lowercase() {
        bail!("managed activation source host is not canonical");
    }
    Ok(())
}

fn admit_source_url(
    value: &str,
    policy: &ManagedExternalContentActivationPolicy,
) -> anyhow::Result<()> {
    validate_portable_source_url(value)?;
    let parsed = url::Url::parse(value)?;
    let host = parsed
        .host_str()
        .expect("portable URL validation checked host");
    if !policy
        .allowed_https_hosts
        .iter()
        .any(|allowed| allowed == host)
    {
        bail!("managed activation source host is not admitted by node policy");
    }
    Ok(())
}

fn validate_member_path(value: &str) -> anyhow::Result<()> {
    ryeos_state::objects::validate_canonical_project_relative_path(value)
}

fn validate_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if !lillux::valid_hash(value) || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        bail!("{label} is not a canonical sha256 digest");
    }
    Ok(())
}

fn validate_id(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        bail!("{label} is not canonical");
    }
    Ok(())
}

fn path_contains(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

fn insert_tree_namespace(target: &str, entries: &mut BTreeSet<String>) {
    let mut path = String::new();
    for part in target.split('/') {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(part);
        entries.insert(path.clone());
    }
}

fn validate_canonical_ref(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 512
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        bail!("{label} is empty, unbounded, or non-canonical");
    }
    let parsed = ryeos_engine::canonical_ref::CanonicalRef::parse(value)
        .map_err(|error| anyhow::anyhow!("invalid {label}: {error}"))?;
    if parsed.to_string() != value {
        bail!("{label} is not canonical");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ManagedExternalContentActivationPolicy {
        ManagedExternalContentActivationPolicy {
            allow_online: true,
            allowed_https_hosts: vec!["releases.example.test".to_owned()],
            max_redirects: 0,
            max_archives: 2,
            max_compressed_bytes: 4096,
            max_expanded_bytes: 8192,
            max_members: 8,
            max_member_bytes: 4096,
            max_concurrent_activations: 1,
            cache_budget_bytes: 16384,
            store_budget_bytes: 32768,
            minimum_free_bytes: 4096,
            max_attempts: 3,
        }
    }

    fn import_limits() -> ExternalContentImportLimits {
        ExternalContentImportLimits {
            max_depth: 8,
            max_entries: 64,
            max_file_bytes: 8192,
            max_total_bytes: 16384,
            store_budget_bytes: 32768,
            minimum_free_bytes: 4096,
        }
    }

    fn document_value(host: &str) -> Value {
        serde_json::json!({
            "schema":MANAGED_ACTIVATION_SCHEMA,
            "consumer_ref":"worker:fixture/hosted",
            "sources":[{
                "id":"package",
                "url":format!("https://{host}/fixture.tar.gz"),
                "archive_format":MANAGED_ACTIVATION_ARCHIVE_FORMAT,
                "sha256":"a".repeat(64),
                "maximum_compressed_bytes":4096,
                "maximum_expanded_bytes":8192,
                "maximum_entries":8,
                "members":[{
                    "path":"bin/runtime",
                    "disposition":"import",
                    "sha256":"b".repeat(64),
                    "maximum_bytes":4096,
                    "executable":true
                }]
            }],
            "components":[{
                "id":"runtime",
                "storage":"large_content",
                "shape":{
                    "kind":"mapped",
                    "members":[{
                        "source":"package",
                        "member":"bin/runtime",
                        "target":null
                    }]
                }
            }]
        })
    }

    fn declarations() -> Vec<ryeos_engine::external_content::ExternalContentDeclaration> {
        vec![ryeos_engine::external_content::ExternalContentDeclaration {
            id: "runtime".to_owned(),
            kind: ryeos_engine::external_content::ExternalContentKind::File,
            locator: None,
            mode: ryeos_engine::external_content::ExternalContentMode::Pinned,
            digest: Some("c".repeat(64)),
            exclude: Vec::new(),
            metadata_hint: None,
            mount: "bin/runtime".to_owned(),
        }]
    }

    fn whole_tree_document_value() -> Value {
        serde_json::json!({
            "schema":MANAGED_ACTIVATION_SCHEMA,
            "consumer_ref":"worker:fixture/hosted",
            "sources":[{
                "id":"runtime-package",
                "url":"https://releases.example.test/runtime.tar.gz",
                "archive_format":MANAGED_ACTIVATION_ARCHIVE_FORMAT,
                "sha256":"a".repeat(64),
                "maximum_compressed_bytes":4096,
                "maximum_expanded_bytes":8192,
                "maximum_entries":8
            }],
            "components":[{
                "id":"runtime",
                "storage":"content",
                "shape":{
                    "kind":"whole_archive_tree",
                    "source":"runtime-package",
                    "prefix":"runtime-root",
                    "bounds":{
                        "maximum_entries":7,
                        "maximum_depth":4,
                        "maximum_file_bytes":4096,
                        "maximum_total_bytes":8192
                    }
                }
            }]
        })
    }

    #[test]
    fn acquisition_mode_selects_transport_policy_without_weakening_portable_validation() {
        let document =
            ManagedExternalContentActivation::from_value(&document_value("foreign.example.test"))
                .unwrap();
        assert!(
            document
                .admit(
                    AcquisitionMode::Online,
                    &policy(),
                    &import_limits(),
                    &declarations(),
                    true,
                )
                .is_err()
        );

        let mut offline_policy = policy();
        offline_policy.allow_online = false;
        offline_policy.allowed_https_hosts.clear();
        document
            .admit(
                AcquisitionMode::Offline,
                &offline_policy,
                &import_limits(),
                &declarations(),
                true,
            )
            .unwrap();

        let mut invalid = document_value("foreign.example.test");
        invalid["sources"][0]["url"] = Value::String("file:///tmp/fixture.tar.gz".to_owned());
        assert!(ManagedExternalContentActivation::from_value(&invalid).is_err());
    }

    #[test]
    fn portable_compilation_accepts_the_generic_config_category_envelope() {
        let mut value = document_value("releases.example.test");
        value["category"] = Value::String("fixture".to_owned());
        ManagedExternalContentActivation::from_value(&value).unwrap();

        value["category"] = Value::Bool(true);
        assert!(ManagedExternalContentActivation::from_value(&value).is_err());
    }

    #[test]
    fn compressed_and_expanded_archive_bounds_are_independent() {
        let mut value = document_value("releases.example.test");
        value["sources"][0]["maximum_compressed_bytes"] = Value::from(4096u64);
        value["sources"][0]["maximum_expanded_bytes"] = Value::from(2048u64);
        ManagedExternalContentActivation::from_value(&value).unwrap();
    }

    #[test]
    fn admission_derives_consumer_manifest_authority() {
        let document =
            ManagedExternalContentActivation::from_value(&document_value("releases.example.test"))
                .unwrap();
        let resolved = document
            .admit(
                AcquisitionMode::Online,
                &policy(),
                &import_limits(),
                &declarations(),
                true,
            )
            .unwrap();
        assert_eq!(resolved[0].expected_manifest_hash, "c".repeat(64));
        assert_eq!(
            resolved[0].expected_manifest_kind,
            ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
        );
    }

    #[test]
    fn recipe_cannot_establish_a_second_consumer_realization() {
        let mut value = document_value("releases.example.test");
        value["components"][0]["id"] = Value::String("other".to_owned());
        let document = ManagedExternalContentActivation::from_value(&value).unwrap();
        assert!(
            document
                .admit(
                    AcquisitionMode::Online,
                    &policy(),
                    &import_limits(),
                    &declarations(),
                    true,
                )
                .is_err()
        );
        value["expected_manifest_hash"] = Value::String("d".repeat(64));
        assert!(ManagedExternalContentActivation::from_value(&value).is_err());
    }

    #[test]
    fn tree_realization_is_derived_from_consumer_kind() {
        let mut value = document_value("releases.example.test");
        value["components"][0]["shape"]["members"][0]["target"] =
            Value::String("bin/runtime".to_owned());
        let document = ManagedExternalContentActivation::from_value(&value).unwrap();
        let mut declarations = declarations();
        declarations[0].kind = ryeos_engine::external_content::ExternalContentKind::Tree;
        let resolved = document
            .admit(
                AcquisitionMode::Online,
                &policy(),
                &import_limits(),
                &declarations,
                true,
            )
            .unwrap();
        assert_eq!(
            resolved[0].declaration_kind,
            ryeos_engine::external_content::ExternalContentKind::Tree
        );
    }

    #[test]
    fn file_and_tree_member_shapes_cannot_be_confused() {
        let mut value = document_value("releases.example.test");
        value["components"][0]["shape"]["members"][0]["target"] =
            Value::String("bin/runtime".to_owned());
        let document = ManagedExternalContentActivation::from_value(&value).unwrap();
        assert!(
            document
                .admit(
                    AcquisitionMode::Online,
                    &policy(),
                    &import_limits(),
                    &declarations(),
                    true,
                )
                .is_err()
        );
    }

    #[test]
    fn target_is_required_even_when_explicitly_null() {
        let mut value = document_value("releases.example.test");
        value["components"][0]["shape"]["members"][0]
            .as_object_mut()
            .unwrap()
            .remove("target");
        assert!(ManagedExternalContentActivation::from_value(&value).is_err());
    }

    #[test]
    fn node_import_limits_fence_managed_tree_capture() {
        let mut value = document_value("releases.example.test");
        value["components"][0]["shape"]["members"][0]["target"] =
            Value::String("bin/runtime".to_owned());
        let document = ManagedExternalContentActivation::from_value(&value).unwrap();
        let mut declarations = declarations();
        declarations[0].kind = ryeos_engine::external_content::ExternalContentKind::Tree;
        let mut limits = import_limits();
        limits.max_depth = 1;
        assert!(
            document
                .admit(
                    AcquisitionMode::Online,
                    &policy(),
                    &limits,
                    &declarations,
                    true,
                )
                .is_err()
        );
    }

    #[test]
    fn whole_archive_tree_is_compact_and_consumer_derived() {
        let value = whole_tree_document_value();
        assert!(serde_json::to_vec(&value).unwrap().len() < 1024);
        let document = ManagedExternalContentActivation::from_value(&value).unwrap();
        let mut declarations = declarations();
        declarations[0].kind = ryeos_engine::external_content::ExternalContentKind::Tree;
        let resolved = document
            .admit(
                AcquisitionMode::Online,
                &policy(),
                &import_limits(),
                &declarations,
                true,
            )
            .unwrap();
        assert_eq!(resolved[0].capture_bounds.maximum_entries, 7);
        assert_eq!(resolved[0].expected_manifest_hash, "c".repeat(64));
        assert!(resolved[0].expected_file_sha256.is_none());
    }

    #[test]
    fn whole_archive_tree_cannot_supply_a_file_or_mix_selected_members() {
        let value = whole_tree_document_value();
        let document = ManagedExternalContentActivation::from_value(&value).unwrap();
        assert!(
            document
                .admit(
                    AcquisitionMode::Online,
                    &policy(),
                    &import_limits(),
                    &declarations(),
                    true,
                )
                .is_err()
        );

        let mut mixed = value;
        mixed["sources"][0]["members"] = serde_json::json!([{
            "path":"runtime-root/bin/runtime",
            "disposition":"verify_only",
            "sha256":"b".repeat(64),
            "maximum_bytes":4096,
            "executable":true
        }]);
        assert!(ManagedExternalContentActivation::from_value(&mixed).is_err());
    }

    #[test]
    fn whole_archive_tree_refuses_transform_vocabulary_and_incoherent_bounds() {
        let mut value = whole_tree_document_value();
        value["components"][0]["shape"]["command"] = Value::String("strip".to_owned());
        assert!(ManagedExternalContentActivation::from_value(&value).is_err());

        let mut value = whole_tree_document_value();
        value["sources"][0]["maximum_entries"] = Value::from(7);
        assert!(ManagedExternalContentActivation::from_value(&value).is_err());
    }

    #[test]
    fn staging_cleanup_budget_covers_every_portable_whole_tree_entry() {
        assert!(
            MAX_MANAGED_ACTIVATION_STAGING_ENTRIES
                >= MAX_PORTABLE_ARCHIVES * MAX_PORTABLE_ARCHIVE_ENTRIES
                    + ryeos_state::objects::MAX_EXTERNAL_CONTENT_ACTIVATION_COMPONENTS
        );
    }

    #[test]
    fn staging_cleanup_budget_covers_mixed_mapped_and_whole_tree_shapes() {
        assert!(
            MAX_MANAGED_ACTIVATION_STAGING_ENTRIES
                >= MAX_MAPPED_ACTIVATION_STAGING_ENTRIES
                    + MAX_WHOLE_TREE_ACTIVATION_STAGING_ENTRIES
        );
    }
}
