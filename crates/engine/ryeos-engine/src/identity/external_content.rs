//! Signed external-content declarations and pure admission rules.
//!
//! This module deliberately performs no filesystem access, capture, CAS
//! writes, materialization, or node-policy lookup. Kinds own declaration and
//! composition semantics; state owns meaning-blind capture/storage mechanics;
//! daemon/executor orchestration supplies admitted named roots and policy.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::contracts::ItemSpace;

pub use ryeos_state::objects::{
    EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_CONTENT_TREE_SCHEMA,
    EXTERNAL_REALIZATIONS_DERIVED_KEY, ExternalContentKind,
    ExternalContentManifestEntry as ManifestEntry, ExternalContentManifestEntryKind,
    ExternalContentManifestObject, ExternalContentMode, ExternalContentMountRoot,
    FILE_REALIZATION_ENTRY_PATH,
};

pub const MAX_DECLARATIONS_PER_ITEM: usize = 8;
pub const MAX_EXCLUDES_PER_DECLARATION: usize = 32;
pub const MAX_ENTRY_PATH_BYTES: usize = ryeos_state::objects::MAX_EXTERNAL_CONTENT_PATH_BYTES;

pub use ryeos_state::external_content::products::composition::{
    EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY, ExternalProductSlotDeclaration,
    ResolvedExternalProductSelection, ResolvedExternalProductSelections,
    ResolvedProductConsumerSource,
};

/// Pure authored shape, not realization or product-selection authority.
/// Pending slots can name search/environment targets without inventing hashes.
#[derive(Debug, Clone)]
pub struct AuthoredExternalContentShape {
    pub literal_declarations: Vec<ExternalContentDeclaration>,
    pub product_slots: Vec<ExternalProductSlotDeclaration>,
}

impl AuthoredExternalContentShape {
    pub fn len(&self) -> usize {
        self.literal_declarations.len() + self.product_slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.literal_declarations
            .iter()
            .map(|entry| entry.id.as_str())
            .chain(self.product_slots.iter().map(|entry| entry.id.as_str()))
    }

    pub fn kind_for_id(&self, id: &str) -> Option<ExternalContentKind> {
        self.literal_declarations
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.kind)
            .or_else(|| {
                self.product_slots
                    .iter()
                    .find(|entry| entry.id == id)
                    .map(|entry| entry.kind)
            })
    }
}

/// Signed runtime-command selector for one executable member of an already
/// admitted external realization. This is deliberately a selector rather
/// than a pathname: daemon admission resolves it from the child's own exact
/// realization set and records the manifest/member coordinate in the admitted
/// direct-command closure before thread birth. After the complete tree is
/// materialized, isolation retains the exact member descriptor and overlays
/// it at that tree-relative path. It does not name a parent prepared-launch
/// dependency and must never be resolved through host `PATH`.
pub const REALIZATION_COMMAND_PREFIX: &str = "realization:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalRealizationCommandRef {
    pub realization_id: String,
    pub relative_path: String,
}

pub fn parse_realization_command_ref(
    value: &str,
) -> anyhow::Result<Option<ExternalRealizationCommandRef>> {
    let Some(remainder) = value.strip_prefix(REALIZATION_COMMAND_PREFIX) else {
        return Ok(None);
    };
    let (realization_id, relative_path) = remainder.split_once('/').ok_or_else(|| {
        anyhow::anyhow!(
            "realization command must be `{REALIZATION_COMMAND_PREFIX}<id>/<relative-path>`"
        )
    })?;
    validate_declaration_id(realization_id)?;
    validate_relative_path("realization command member", relative_path)?;
    Ok(Some(ExternalRealizationCommandRef {
        realization_id: realization_id.to_owned(),
        relative_path: relative_path.to_owned(),
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaringAuthority<'a> {
    Project,
    Node,
    Bundle(&'a str),
}

impl DeclaringAuthority<'_> {
    pub fn label(&self) -> String {
        match self {
            Self::Project => "project".to_owned(),
            Self::Node => "node".to_owned(),
            Self::Bundle(name) => format!("bundle:{name}"),
        }
    }
}

/// Signed locator classes. These are authority names, never host paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalContentRoot {
    ProjectFiles,
    NodeFiles,
    Bundle(String),
}

impl Serialize for ExternalContentRoot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.label())
    }
}

impl<'de> Deserialize<'de> for ExternalContentRoot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl ExternalContentRoot {
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "project_files" => Ok(Self::ProjectFiles),
            "node_files" => Ok(Self::NodeFiles),
            other => match other.strip_prefix("bundle:") {
                Some(name) if valid_bundle_name(name) => Ok(Self::Bundle(name.to_owned())),
                _ => Err(format!("unsupported external content root: {other}")),
            },
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::ProjectFiles => "project_files".to_owned(),
            Self::NodeFiles => "node_files".to_owned(),
            Self::Bundle(name) => format!("bundle:{name}"),
        }
    }

    pub fn contract_class(&self) -> &'static str {
        match self {
            Self::ProjectFiles => "project_files",
            Self::NodeFiles => "node_files",
            Self::Bundle(_) => "bundle:own",
        }
    }

    pub fn declarable_from(&self, declarer: DeclaringAuthority<'_>) -> bool {
        match (self, declarer) {
            (Self::ProjectFiles, DeclaringAuthority::Project | DeclaringAuthority::Node) => true,
            (Self::NodeFiles, DeclaringAuthority::Node) => true,
            (Self::Bundle(named), DeclaringAuthority::Bundle(own)) => named == own,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentLocator {
    pub root: ExternalContentRoot,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalContentDeclaration {
    pub id: String,
    pub kind: ExternalContentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<ExternalContentLocator>,
    pub mode: ExternalContentMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_hint: Option<String>,
    pub mount_root: ExternalContentMountRoot,
    pub mount: String,
}

impl ExternalContentDeclaration {
    pub fn validate(&self, declarer: DeclaringAuthority<'_>) -> anyhow::Result<()> {
        self.validate_with_draft_state(declarer, false)
    }

    /// Validate unsigned authoring input for the dedicated pin-completion
    /// transaction. This is not an admission/preview dialect: the only relaxed
    /// state is a locator-backed `pinned` declaration whose digest is absent.
    pub fn validate_for_pin_authoring_draft(
        &self,
        declarer: DeclaringAuthority<'_>,
    ) -> anyhow::Result<()> {
        self.validate_with_draft_state(declarer, true)
    }

    fn validate_with_draft_state(
        &self,
        declarer: DeclaringAuthority<'_>,
        allow_missing_pinned_digest: bool,
    ) -> anyhow::Result<()> {
        validate_declaration_id(&self.id)?;
        validate_relative_path("external content mount target", &self.mount)?;
        match &self.locator {
            Some(locator) => {
                validate_relative_path("external content locator path", &locator.path)?;
                if !locator.root.declarable_from(declarer) {
                    anyhow::bail!(
                        "an item admitted from {} may not declare external content under {}",
                        declarer.label(),
                        locator.root.label()
                    );
                }
            }
            None => {
                if self.mode != ExternalContentMode::Pinned || self.digest.is_none() {
                    anyhow::bail!(
                        "external content `{}` may omit its locator only when pinned to a digest",
                        self.id
                    );
                }
                if !self.exclude.is_empty() {
                    anyhow::bail!("locator-free external content cannot declare exclusions");
                }
            }
        }
        match (self.mode, self.digest.as_deref()) {
            (ExternalContentMode::Pinned, Some(digest)) if lillux::cas::valid_hash(digest) => {}
            (ExternalContentMode::Pinned, Some(_)) => {
                anyhow::bail!("pinned external content digest is not canonical")
            }
            (ExternalContentMode::Pinned, None)
                if allow_missing_pinned_digest && self.locator.is_some() => {}
            (ExternalContentMode::Pinned, None) => {
                anyhow::bail!("pinned external content must carry its expected digest")
            }
            (ExternalContentMode::Captured, None) => {}
            (ExternalContentMode::Captured, Some(_)) => {
                anyhow::bail!("captured external content must not carry a digest")
            }
        }
        if self.exclude.len() > MAX_EXCLUDES_PER_DECLARATION {
            anyhow::bail!("external content has too many authored exclusions");
        }
        for exclusion in &self.exclude {
            validate_exclude_pattern(exclusion)?;
        }
        if let Some(hint) = &self.metadata_hint {
            if hint.is_empty() || hint.len() > 255 || hint.chars().any(char::is_control) {
                anyhow::bail!("external content metadata hint is not canonical");
            }
        }
        Ok(())
    }
}

pub fn declarations_from_authored_pin_draft(
    authored: &serde_json::Value,
    contract: Option<&crate::kind_registry::KindExternalContentDecl>,
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<Vec<ExternalContentDeclaration>> {
    let Some(contract) = contract else {
        if authored.get("external_content").is_some() {
            anyhow::bail!(
                "item declares `external_content` but its signed kind has no external-content contract"
            );
        }
        anyhow::bail!("item has no kind-owned external-content contract");
    };
    let value = authored
        .get("external_content")
        .ok_or_else(|| anyhow::anyhow!("item has no root-authored external_content declaration"))?;
    if value.is_null() {
        anyhow::bail!("external_content must be an array");
    }
    let declarations: Vec<ExternalContentDeclaration> = serde_json::from_value(value.clone())
        .map_err(|error| anyhow::anyhow!("invalid external_content declaration: {error}"))?;
    validate_declaration_collection(&declarations, declarer, true)?;
    validate_kind_contract(&declarations, contract)?;
    Ok(declarations)
}

pub fn validate_declarations(
    declarations: &[ExternalContentDeclaration],
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<()> {
    validate_declaration_collection(declarations, declarer, false)
}

fn validate_declaration_collection(
    declarations: &[ExternalContentDeclaration],
    declarer: DeclaringAuthority<'_>,
    allow_missing_pinned_digest: bool,
) -> anyhow::Result<()> {
    if declarations.len() > MAX_DECLARATIONS_PER_ITEM {
        anyhow::bail!("item declares too many external content entries");
    }
    for declaration in declarations {
        if allow_missing_pinned_digest {
            declaration.validate_for_pin_authoring_draft(declarer)?;
        } else {
            declaration.validate(declarer)?;
        }
    }
    validate_content_locations(
        declarations
            .iter()
            .map(|entry| (entry.id.as_str(), entry.mount_root, entry.mount.as_str())),
    )
}

fn validate_content_locations<'a>(
    entries: impl IntoIterator<Item = (&'a str, ExternalContentMountRoot, &'a str)>,
) -> anyhow::Result<()> {
    let mut ids = BTreeSet::new();
    let mut mounts = BTreeSet::new();
    for (id, mount_root, mount) in entries {
        if !ids.insert(id) {
            anyhow::bail!("external content id `{id}` is duplicated");
        }
        if !mounts.insert((mount_root, mount)) {
            anyhow::bail!("external content mount `{mount}` is duplicated");
        }
    }
    let mounts = mounts.into_iter().collect::<Vec<_>>();
    for (index, left) in mounts.iter().enumerate() {
        for right in mounts.iter().skip(index + 1) {
            if left.0 == right.0
                && (path_contains(left.1, right.1) || path_contains(right.1, left.1))
            {
                anyhow::bail!("external content mounts `{left:?}` and `{right:?}` overlap");
            }
        }
    }
    Ok(())
}

/// Parse the signed literal/slot union without claiming that a pending slot
/// can execute. Effective declaration derivation below requires exact selection.
pub fn authored_external_content_shape(
    composed: &serde_json::Value,
    contract: Option<&crate::kind_registry::KindExternalContentDecl>,
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<Option<AuthoredExternalContentShape>> {
    let literal_declarations = declarations_from_composed(composed, contract, declarer)?;
    let Some(value) = composed.get("external_product_slots") else {
        return Ok(
            literal_declarations.map(|literal_declarations| AuthoredExternalContentShape {
                literal_declarations,
                product_slots: Vec::new(),
            }),
        );
    };
    let contract = contract.ok_or_else(|| {
        anyhow::anyhow!("product slots require a signed external-content kind contract")
    })?;
    let product_slots: Vec<ExternalProductSlotDeclaration> = serde_json::from_value(value.clone())
        .map_err(|error| anyhow::anyhow!("invalid external_product_slots: {error}"))?;
    if !product_slots.is_empty() && declarer == DeclaringAuthority::Node {
        anyhow::bail!("product slots require an exact pinned-project or installed-bundle consumer");
    }
    let shape = AuthoredExternalContentShape {
        literal_declarations: literal_declarations.unwrap_or_default(),
        product_slots,
    };
    if shape.len() > MAX_DECLARATIONS_PER_ITEM || shape.len() > contract.max_declarations {
        anyhow::bail!("combined literal and product declarations exceed the signed ceiling");
    }
    for slot in &shape.product_slots {
        slot.validate()?;
        if !contract.allowed_mount_roots.contains(&slot.mount_root) {
            anyhow::bail!("product slot mount root is outside its signed kind contract");
        }
    }
    validate_content_locations(
        shape
            .literal_declarations
            .iter()
            .map(|entry| (entry.id.as_str(), entry.mount_root, entry.mount.as_str()))
            .chain(
                shape
                    .product_slots
                    .iter()
                    .map(|entry| (entry.id.as_str(), entry.mount_root, entry.mount.as_str())),
            ),
    )?;
    Ok(Some(shape))
}

/// Read an application-verified selection without treating arbitrary derived
/// JSON as authority. This enforces its closed type and exact signed-source
/// relationship; node/witness verification remains the admission owner's duty.
pub fn effective_external_content_declarations(
    resolution: &crate::resolution::ResolutionOutput,
    contract: Option<&crate::kind_registry::KindExternalContentDecl>,
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<Option<Vec<ExternalContentDeclaration>>> {
    let shape = authored_external_content_shape(&resolution.composed.composed, contract, declarer)?;
    let selections = resolved_external_product_selections(resolution)?;
    let Some(shape) = shape else {
        if selections.is_some() {
            anyhow::bail!("product selection has no signed content declaration");
        }
        return Ok(None);
    };
    if shape.product_slots.is_empty() {
        if selections.is_some() {
            anyhow::bail!("product selection has no signed product slot");
        }
        return Ok(Some(shape.literal_declarations));
    }
    let selections =
        selections.ok_or_else(|| anyhow::anyhow!("product slot has no admitted selection"))?;
    if selections.len() != shape.product_slots.len() {
        anyhow::bail!("product selections do not exactly cover the signed slots");
    }
    let mut before = resolution.clone();
    before
        .composed
        .derived
        .remove(EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY);
    before
        .composed
        .derived
        .remove(EXTERNAL_REALIZATIONS_DERIVED_KEY);
    let expected_base = pre_product_selection_consumer_digest(&before)?;
    let mut declarations = shape.literal_declarations;
    for slot in shape.product_slots {
        let selection = selections
            .get(&slot.id)
            .ok_or_else(|| anyhow::anyhow!("signed product slot is unresolved"))?;
        selection.validate()?;
        if selection.declaration_id != slot.id
            || selection.relationship_ref != slot.relationship_ref
            || selection.relationship.name != slot.relationship
            || selection.consumer_source.consumer_ref() != resolution.root.resolved_ref
            || Some(selection.consumer_source.publisher_fingerprint())
                != resolution.root.signer_fingerprint.as_deref()
            || selection.pre_selection_effective_definition_digest != expected_base
            || selection.declaration.id != slot.id
            || selection.declaration.kind != slot.kind
            || selection.declaration.mount_root != slot.mount_root
            || selection.declaration.mount != slot.mount
            || selection.declaration.manifest_hash != selection.manifest_hash
        {
            anyhow::bail!("product selection contradicts its exact signed consumer slot");
        }
        validate_resolved_product_consumer_source(resolution, &selection.consumer_source)?;
        declarations.push(ExternalContentDeclaration {
            id: slot.id,
            kind: slot.kind,
            mode: ExternalContentMode::Pinned,
            locator: None,
            digest: Some(selection.manifest_hash.clone()),
            exclude: Vec::new(),
            metadata_hint: None,
            mount_root: slot.mount_root,
            mount: slot.mount,
        });
    }
    validate_declarations(&declarations, declarer)?;
    validate_kind_contract(
        &declarations,
        contract.expect("shape requires its kind contract"),
    )?;
    if let Some(value) = resolution
        .composed
        .derived
        .get(EXTERNAL_REALIZATIONS_DERIVED_KEY)
    {
        let realized = crate::external_realization::RealizedExternalContentSet::from_value(value)?;
        if realized.iter().len() != declarations.len()
            || declarations.iter().any(|declaration| {
                !realized.iter().any(|entry| {
                    entry.id == declaration.id
                        && entry.kind == declaration.kind
                        && entry.mode == declaration.mode
                        && declaration
                            .digest
                            .as_deref()
                            .is_none_or(|digest| digest == entry.manifest_hash)
                        && entry.mount_root == declaration.mount_root
                        && entry.mount == declaration.mount
                })
            })
        {
            anyhow::bail!("retained product realizations contradict the selected declarations");
        }
    }
    Ok(Some(declarations))
}

/// Resolve declarations which may authorize an operator-owned content bind.
///
/// A literal pinned declaration is already complete in the signed D0 and can
/// be bound while unrelated product slots are still pending. Once a selection
/// projection is present, however, it must be the exact complete D1 accepted
/// by execution admission; malformed or partial selected state is never
/// reinterpreted as an unselected consumer.
pub fn external_content_declarations_for_binding(
    resolution: &crate::resolution::ResolutionOutput,
    contract: Option<&crate::kind_registry::KindExternalContentDecl>,
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<Option<Vec<ExternalContentDeclaration>>> {
    let shape = authored_external_content_shape(&resolution.composed.composed, contract, declarer)?;
    if resolved_external_product_selections(resolution)?.is_some() {
        return effective_external_content_declarations(resolution, contract, declarer);
    }
    Ok(shape.map(|shape| shape.literal_declarations))
}

/// Closed decoding, not node testimony authentication. Fresh admission must
/// verify the witness before insertion; recovery must bind this value to its
/// admitted selector, owner and project authority.
pub fn resolved_external_product_selections(
    resolution: &crate::resolution::ResolutionOutput,
) -> anyhow::Result<Option<ResolvedExternalProductSelections>> {
    let Some(value) = resolution
        .composed
        .derived
        .get(EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY)
    else {
        return Ok(None);
    };
    let map = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("product selections must be a keyed object"))?;
    if map.is_empty() || map.len() > MAX_DECLARATIONS_PER_ITEM
        || lillux::canonical_json(value)?.len()
            > ryeos_state::external_content::products::composition::MAX_RESOLVED_PRODUCT_SELECTIONS_BYTES
    {
        anyhow::bail!("product selections exceed the bounded declaration contract");
    }
    let selections: ryeos_state::external_content::products::composition::ResolvedExternalProductSelections =
        serde_json::from_value(value.clone()).map_err(|error| anyhow::anyhow!("invalid product selections: {error}"))?;
    selections.validate()?;
    Ok(Some(selections))
}

/// Atomically install one target's complete selection set after application
/// witness/relationship verification. Every entry is derived from the same
/// unselected D0; no partially selected identity is ever published.
pub fn insert_resolved_product_selections(
    resolution: &mut crate::resolution::ResolutionOutput,
    selections: ResolvedExternalProductSelections,
    contract: &crate::kind_registry::KindExternalContentDecl,
) -> anyhow::Result<()> {
    let expected_base = pre_product_selection_consumer_digest(resolution)?;
    selections.validate()?;
    if selections.len() > MAX_DECLARATIONS_PER_ITEM
        || selections.iter().any(|(_, selection)| {
            selection.pre_selection_effective_definition_digest != expected_base
        })
    {
        anyhow::bail!("product selections do not form one bounded complete D0 projection");
    }
    let mut candidate = resolution.clone();
    candidate.composed.derived.insert(
        EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY.to_owned(),
        serde_json::to_value(&selections)?,
    );
    effective_external_content_declarations(
        &candidate,
        Some(contract),
        declaring_authority(&candidate)?,
    )?;
    *resolution = candidate;
    Ok(())
}

fn validate_resolved_product_consumer_source(
    resolution: &crate::resolution::ResolutionOutput,
    source: &ResolvedProductConsumerSource,
) -> anyhow::Result<()> {
    source.validate()?;
    let declarer = declaring_authority(resolution)?;
    match (source, declarer) {
        (ResolvedProductConsumerSource::InstalledBundle { .. }, DeclaringAuthority::Bundle(_)) => {}
        (
            ResolvedProductConsumerSource::PinnedProject { source_closure, .. },
            DeclaringAuthority::Project,
        ) => {
            let admitted_source = resolution
                .composed
                .derived
                .get(ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY)
                .map(ryeos_state::objects::EffectiveSourceClosureProjection::from_value)
                .transpose()?;
            if admitted_source.as_ref() != source_closure.as_ref() {
                anyhow::bail!(
                    "resolved product consumer source closure differs from its admitted definition"
                );
            }
        }
        _ => anyhow::bail!(
            "resolved product consumer source authority differs from its admitted definition"
        ),
    }
    Ok(())
}

/// Consumer D0 before any product selection or external realization. A caller
/// cannot provide an already-augmented resolution as a new authored generation.
pub fn pre_product_selection_consumer_digest(
    resolution: &crate::resolution::ResolutionOutput,
) -> anyhow::Result<String> {
    if resolution
        .composed
        .derived
        .contains_key(EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY)
    {
        anyhow::bail!("product consumer identity must be derived before product selection");
    }
    pre_external_realization_consumer_digest(resolution)
}

pub fn declarations_from_composed(
    composed: &serde_json::Value,
    contract: Option<&crate::kind_registry::KindExternalContentDecl>,
    declarer: DeclaringAuthority<'_>,
) -> anyhow::Result<Option<Vec<ExternalContentDeclaration>>> {
    let authored = composed.get("external_content");
    let Some(contract) = contract else {
        if authored.is_some() {
            anyhow::bail!(
                "item declares `external_content` but its signed kind has no external-content contract"
            );
        }
        return Ok(None);
    };
    let Some(value) = authored else {
        return Ok(None);
    };
    if value.is_null() {
        anyhow::bail!("external_content must be an array");
    }
    let declarations: Vec<ExternalContentDeclaration> = serde_json::from_value(value.clone())
        .map_err(|error| anyhow::anyhow!("invalid external_content declaration: {error}"))?;
    if declarations.len() > contract.max_declarations {
        anyhow::bail!(
            "item declares {} external content entries; its signed kind permits {}",
            declarations.len(),
            contract.max_declarations
        );
    }
    validate_declarations(&declarations, declarer)?;
    validate_kind_contract(&declarations, contract)?;
    Ok(Some(declarations))
}

fn validate_kind_contract(
    declarations: &[ExternalContentDeclaration],
    contract: &crate::kind_registry::KindExternalContentDecl,
) -> anyhow::Result<()> {
    if declarations.len() > contract.max_declarations {
        anyhow::bail!(
            "item declares {} external content entries; its signed kind permits {}",
            declarations.len(),
            contract.max_declarations
        );
    }
    for declaration in declarations {
        if !contract
            .allowed_mount_roots
            .contains(&declaration.mount_root)
        {
            anyhow::bail!(
                "external content `{}` names mount root {:?} which its signed kind does not permit",
                declaration.id,
                declaration.mount_root,
            );
        }
        if let Some(locator) = &declaration.locator
            && !contract
                .allowed_roots
                .iter()
                .any(|allowed| allowed == locator.root.contract_class())
        {
            anyhow::bail!(
                "external content `{}` names root `{}` which its signed kind does not permit",
                declaration.id,
                locator.root.label()
            );
        }
    }
    Ok(())
}

/// Derive declaration authority from verified resolution provenance. This is
/// pure admission logic: it does not resolve or open a host path.
pub fn declaring_authority(
    resolution: &crate::resolution::ResolutionOutput,
) -> anyhow::Result<DeclaringAuthority<'_>> {
    use crate::contracts::ItemSourceRoot;

    match (&resolution.root.source_root, resolution.root.source_space) {
        (ItemSourceRoot::Project, ItemSpace::Project) => Ok(DeclaringAuthority::Project),
        (ItemSourceRoot::Node, ItemSpace::Node) => Ok(DeclaringAuthority::Node),
        (ItemSourceRoot::Bundle { name }, ItemSpace::Bundle) => {
            Ok(DeclaringAuthority::Bundle(name))
        }
        (identity, space) => anyhow::bail!(
            "external-content declarer has non-authoritative or incoherent source root {identity:?} for {} space",
            space.as_str()
        ),
    }
}

/// Identity of a fully resolved consumer immediately before external
/// realizations are inserted into its effective view.
///
/// Project external-content bindings and launch admission both use this
/// helper. Keeping the derivation here prevents either caller from inventing
/// a parallel projection. A post-realization input is rejected because the
/// resulting digest would recursively depend on the binding being selected.
pub fn pre_external_realization_consumer_digest(
    resolution: &crate::resolution::ResolutionOutput,
) -> anyhow::Result<String> {
    if resolution
        .composed
        .derived
        .contains_key(EXTERNAL_REALIZATIONS_DERIVED_KEY)
    {
        anyhow::bail!(
            "external-content consumer identity must be derived before realization admission"
        );
    }
    resolution
        .effective_definition_digest()
        .map(|digest| digest.as_str().to_owned())
        .map_err(|error| {
            anyhow::anyhow!("derive pre-realization external-content consumer identity: {error}")
        })
}

fn validate_relative_path(label: &str, value: &str) -> anyhow::Result<()> {
    if value.len() > MAX_ENTRY_PATH_BYTES {
        anyhow::bail!("{label} exceeds {MAX_ENTRY_PATH_BYTES} bytes");
    }
    ryeos_state::objects::validate_canonical_project_relative_path(value)
        .map_err(|error| anyhow::anyhow!("{label}: {error}"))
}

pub(crate) fn validate_declaration_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
    {
        anyhow::bail!("external content id has a non-canonical value: {id:?}");
    }
    Ok(())
}

fn validate_exclude_pattern(pattern: &str) -> anyhow::Result<()> {
    if pattern.is_empty() || pattern.len() > 128 || pattern.contains('/') {
        anyhow::bail!("external content exclusion is not a canonical basename pattern");
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        if !suffix.starts_with('.') || suffix.len() < 2 || suffix.contains('*') {
            anyhow::bail!("external content suffix exclusion must use `*.ext`");
        }
    } else if pattern.contains('*') {
        anyhow::bail!("external content exclusion supports one leading suffix wildcard only");
    }
    Ok(())
}

fn valid_bundle_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn path_contains(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .is_some_and(|suffix| suffix.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(roots: &[&str], max: usize) -> crate::kind_registry::KindExternalContentDecl {
        crate::kind_registry::KindExternalContentDecl {
            realization_derived: EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),
            allowed_roots: roots.iter().map(|value| (*value).to_owned()).collect(),
            allowed_mount_roots: vec![ExternalContentMountRoot::Project],
            max_declarations: max,
            large_content: None,
        }
    }

    fn product_slot() -> serde_json::Value {
        serde_json::json!({
            "id":"runtime", "relationship_ref":"config:fixture/recipe",
            "relationship":"runtime_to_worker", "kind":"tree",
            "mount_root":"project", "mount":"environment/runtime"
        })
    }

    // Pure identity fixtures: no node signature or capsule admission is mocked.
    fn product_resolution() -> crate::resolution::ResolutionOutput {
        use crate::resolution::{
            KindComposedView, ResolutionOutput, ResolutionStepName, ResolvedAncestor, TrustClass,
        };
        ResolutionOutput {
            root: ResolvedAncestor {
                requested_id: "config:fixture/environment".to_owned(),
                resolved_ref: "config:fixture/environment".to_owned(),
                source_path: "/diagnostic/environment.yaml".into(),
                source_space: ItemSpace::Project,
                source_root: crate::contracts::ItemSourceRoot::Project,
                trust_class: TrustClass::TrustedProject,
                signer_fingerprint: Some("a".repeat(64)),
                alias_resolution: None,
                added_by: ResolutionStepName::PipelineInit,
                raw_content: "fixture".to_owned(),
                source_content_digest: "b".repeat(64),
                raw_content_digest: "c".repeat(64),
            },
            ancestors: Vec::new(),
            references_edges: Vec::new(),
            referenced_items: Vec::new(),
            step_outputs: Default::default(),
            effective_trust_class: TrustClass::TrustedProject,
            composed: KindComposedView {
                composed: serde_json::json!({"external_content":[], "external_product_slots":[product_slot()]}),
                derived: Default::default(),
                policy_facts: Default::default(),
            },
        }
    }

    fn product_selection(
        resolution: &crate::resolution::ResolutionOutput,
    ) -> ResolvedExternalProductSelection {
        serde_json::from_value(serde_json::json!({
            "schema":ryeos_state::external_content::products::composition::RESOLVED_EXTERNAL_PRODUCT_SELECTION_SCHEMA,
            "declaration_id":"runtime", "relationship_name":"runtime_to_worker",
            "relationship_ref":"config:fixture/recipe", "relationship_raw_content_digest":"d".repeat(64),
            "relationship":{
                "name":"runtime_to_worker",
                "producer":{"canonical_ref":"graph:fixture/build","recipe_binding":"product_recipe","product_name":"runtime","parameters":{}},
                "consumer":{"canonical_ref":"config:fixture/environment","declaration_id":"runtime"},
                "required_product":{"shape":"tree","storage":"content","bounds":{"maximum_entries":8,"maximum_depth":4,"maximum_file_bytes":1024,"maximum_total_bytes":4096}},
                "qualification":{"policy_ref":null,"required_claims":[]}
            },
            "witness_hash":"e".repeat(64),
            "witness_source":{"kind":"local_capture"},
            "qualification":null,
            "witness_coordinate":{"owner_principal":format!("fp:{}","f".repeat(64)),"chain_root_id":"T-root","thread_id":"T-terminal","recipe_binding":"product_recipe","product_name":"runtime"},
            "producer":{"canonical_ref":"graph:fixture/build","effective_definition_digest":"1".repeat(64),"exact_program_hash":"2".repeat(64),"producer_project_snapshot_hash":"3".repeat(64),"launch_authority_digest":"4".repeat(64),"admitted_parameters_digest":ryeos_state::objects::canonical_value_digest(&serde_json::json!({})).unwrap()},
            "owner_principal":format!("fp:{}","f".repeat(64)),
            "consumer_source":{"kind":"pinned_project","consumer_ref":"config:fixture/environment",
                "publisher_fingerprint":"a".repeat(64),"project_snapshot_hash":"5".repeat(64),
                "source_closure":null},
            "pre_selection_effective_definition_digest":pre_product_selection_consumer_digest(resolution).unwrap(),
            "manifest_hash":"6".repeat(64),"manifest_kind":EXTERNAL_CONTENT_MANIFEST_KIND,
            "declaration":{"id":"runtime","kind":"tree","manifest_hash":"6".repeat(64),"mount_root":"project","mount":"environment/runtime"}
        })).unwrap()
    }

    fn product_selections(
        selection: ResolvedExternalProductSelection,
    ) -> ResolvedExternalProductSelections {
        ResolvedExternalProductSelections::new(std::collections::BTreeMap::from([(
            selection.declaration_id.clone(),
            selection,
        )]))
        .unwrap()
    }

    fn second_product_selection(
        mut selection: ResolvedExternalProductSelection,
    ) -> ResolvedExternalProductSelection {
        selection.declaration_id = "support".to_owned();
        selection.relationship_name = "support_to_worker".to_owned();
        selection.relationship.name = "support_to_worker".to_owned();
        selection.relationship.consumer.declaration_id = "support".to_owned();
        selection.witness_hash = "7".repeat(64);
        selection.manifest_hash = "8".repeat(64);
        selection.declaration.id = "support".to_owned();
        selection.declaration.manifest_hash = "8".repeat(64);
        selection.declaration.mount = "environment/support".to_owned();
        selection
    }

    #[test]
    fn product_shapes_are_pending_not_placeholder_declarations() {
        let contract = contract(&[], 2);
        let resolution = product_resolution();
        let shape = authored_external_content_shape(
            &resolution.composed.composed,
            Some(&contract),
            DeclaringAuthority::Project,
        )
        .unwrap()
        .unwrap();
        assert!(shape.literal_declarations.is_empty());
        assert_eq!(shape.ids().collect::<Vec<_>>(), vec!["runtime"]);
        assert_eq!(
            shape.kind_for_id("runtime"),
            Some(ExternalContentKind::Tree)
        );
        assert!(
            effective_external_content_declarations(
                &resolution,
                Some(&contract),
                DeclaringAuthority::Project
            )
            .is_err()
        );
        let mut collision = resolution.composed.composed.clone();
        collision["external_content"] = serde_json::json!([{
            "id":"runtime","kind":"tree","mode":"pinned","digest":"a".repeat(64),
            "mount_root":"project","mount":"other"
        }]);
        assert!(
            authored_external_content_shape(
                &collision,
                Some(&contract),
                DeclaringAuthority::Project
            )
            .is_err()
        );
        collision["external_content"][0]["id"] = serde_json::json!("other");
        collision["external_content"][0]["mount"] = serde_json::json!("environment");
        assert!(
            authored_external_content_shape(
                &collision,
                Some(&contract),
                DeclaringAuthority::Project
            )
            .is_err()
        );
        authored_external_content_shape(
            &resolution.composed.composed,
            Some(&contract),
            DeclaringAuthority::Bundle("fixture"),
        )
        .unwrap();
    }

    #[test]
    fn literal_binding_projection_does_not_require_unrelated_pending_slots() {
        let kind_contract = contract(&[], 2);
        let mut resolution = product_resolution();
        resolution.composed.composed["external_content"] = serde_json::json!([{
            "id":"producer_python", "kind":"tree", "mode":"pinned",
            "digest":"9".repeat(64), "mount_root":"project", "mount":"producer-python"
        }]);

        let declarations = external_content_declarations_for_binding(
            &resolution,
            Some(&kind_contract),
            DeclaringAuthority::Project,
        )
        .unwrap()
        .unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].id, "producer_python");
        let literal_digest = "9".repeat(64);
        assert_eq!(
            declarations[0].digest.as_deref(),
            Some(literal_digest.as_str())
        );
        assert!(declarations.iter().all(|entry| entry.id != "runtime"));

        let mut selected = resolution.clone();
        insert_resolved_product_selections(
            &mut selected,
            product_selections(product_selection(&resolution)),
            &kind_contract,
        )
        .unwrap();
        assert_eq!(
            external_content_declarations_for_binding(
                &selected,
                Some(&kind_contract),
                DeclaringAuthority::Project,
            )
            .unwrap()
            .unwrap()
            .len(),
            2
        );
        let d0 = pre_external_realization_consumer_digest(&resolution).unwrap();
        let d1 = pre_external_realization_consumer_digest(&selected).unwrap();
        assert_ne!(d0, d1);
        let d0_authority = ryeos_state::objects::ExternalContentConsumerAuthority::pinned_project(
            resolution.root.resolved_ref.clone(),
            resolution.root.signer_fingerprint.clone().unwrap(),
            "5".repeat(64),
            d0,
            None,
        )
        .unwrap();
        let d1_authority = ryeos_state::objects::ExternalContentConsumerAuthority::pinned_project(
            selected.root.resolved_ref.clone(),
            selected.root.signer_fingerprint.clone().unwrap(),
            "5".repeat(64),
            d1,
            None,
        )
        .unwrap();
        assert_ne!(
            ryeos_state::objects::ExternalContentBinding::derive_binding_subject_id(
                literal_digest.as_str(),
                EXTERNAL_CONTENT_MANIFEST_KIND,
                &d0_authority,
                &"8".repeat(64),
            )
            .unwrap(),
            ryeos_state::objects::ExternalContentBinding::derive_binding_subject_id(
                literal_digest.as_str(),
                EXTERNAL_CONTENT_MANIFEST_KIND,
                &d1_authority,
                &"8".repeat(64),
            )
            .unwrap()
        );

        let selections = selected
            .composed
            .derived
            .get_mut(EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY)
            .unwrap()
            .as_object_mut()
            .unwrap();
        selections.get_mut("runtime").unwrap()["declaration"]["mount"] =
            serde_json::json!("different");
        assert!(
            external_content_declarations_for_binding(
                &selected,
                Some(&kind_contract),
                DeclaringAuthority::Project,
            )
            .is_err()
        );

        let mut partial = resolution.clone();
        partial.composed.composed["external_product_slots"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id":"support", "relationship_ref":"config:fixture/recipe",
                "relationship":"support_to_worker", "kind":"tree",
                "mount_root":"project", "mount":"environment/support"
            }));
        let partial_selection = product_selection(&partial);
        partial.composed.derived.insert(
            EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY.to_owned(),
            serde_json::to_value(product_selections(partial_selection)).unwrap(),
        );
        assert!(
            external_content_declarations_for_binding(
                &partial,
                Some(&contract(&[], 3)),
                DeclaringAuthority::Project,
            )
            .is_err()
        );

        let mut malformed = resolution;
        malformed.composed.derived.insert(
            EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY.to_owned(),
            serde_json::Value::Null,
        );
        assert!(
            external_content_declarations_for_binding(
                &malformed,
                Some(&kind_contract),
                DeclaringAuthority::Project,
            )
            .is_err()
        );
    }

    #[test]
    fn product_selection_commits_effective_not_authored_identity_and_survives_retention() {
        let contract = contract(&[], 2);
        let mut resolution = product_resolution();
        let authored = resolution.authored_definition_digest().unwrap();
        let d0 = pre_product_selection_consumer_digest(&resolution).unwrap();
        let selection = product_selection(&resolution);
        insert_resolved_product_selections(
            &mut resolution,
            product_selections(selection.clone()),
            &contract,
        )
        .unwrap();
        assert!(
            declarations_from_composed(
                &resolution.composed.composed,
                Some(&contract),
                DeclaringAuthority::Project,
            )
            .unwrap()
            .is_some_and(|declarations| declarations.is_empty()),
            "the selected product is not an authored literal declaration"
        );
        assert_eq!(resolution.authored_definition_digest().unwrap(), authored);
        assert_ne!(
            pre_external_realization_consumer_digest(&resolution).unwrap(),
            d0
        );
        assert!(pre_product_selection_consumer_digest(&resolution).is_err());
        assert!(
            insert_resolved_product_selections(
                &mut resolution,
                product_selections(selection),
                &contract,
            )
            .is_err()
        );
        let retained = crate::resolution::RetainedResolutionOutput::capture(&resolution);
        let restored = retained.restore();
        let declarations = effective_external_content_declarations(
            &restored,
            Some(&contract),
            DeclaringAuthority::Project,
        )
        .unwrap()
        .unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(
            declarations[0].digest.as_deref(),
            Some("6".repeat(64).as_str())
        );
        assert_eq!(declarations[0].mode, ExternalContentMode::Pinned);
        assert!(declarations[0].locator.is_none());
        assert!(declarations[0].exclude.is_empty());
        assert_eq!(
            resolution.effective_definition_digest().unwrap(),
            restored.effective_definition_digest().unwrap()
        );
        let selected_digest = resolution.effective_definition_digest().unwrap();
        let mut received = resolution.clone();
        received
            .composed
            .derived
            .get_mut(EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY)
            .unwrap()["runtime"]["witness_source"] =
            serde_json::json!({"kind":"received","acceptance_hash":"7".repeat(64)});
        assert_ne!(
            received.composed.derived[EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY],
            resolution.composed.derived[EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY],
            "retained selection must preserve the exact redemption source"
        );
        assert_eq!(
            received.effective_definition_digest().unwrap(),
            selected_digest,
            "receiver-local witness redemption is not executable identity"
        );
        let mut changed_witness = resolution.clone();
        changed_witness
            .composed
            .derived
            .get_mut(EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY)
            .unwrap()["runtime"]["witness_hash"] = serde_json::json!("8".repeat(64));
        assert_ne!(
            changed_witness.effective_definition_digest().unwrap(),
            selected_digest,
            "the immutable product witness remains executable identity"
        );
        let mut realized = restored;
        realized.composed.derived.insert(
            EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),
            serde_json::json!([{
                "id":"runtime","kind":"tree","mode":"pinned","manifest_hash":"6".repeat(64),
                "entry_count":1,"total_bytes":1,"mount_root":"project","mount":"environment/runtime"
            }]),
        );
        effective_external_content_declarations(
            &realized,
            Some(&contract),
            DeclaringAuthority::Project,
        )
        .unwrap();
        realized
            .composed
            .derived
            .get_mut(EXTERNAL_REALIZATIONS_DERIVED_KEY)
            .unwrap()[0]["manifest_hash"] = serde_json::json!("7".repeat(64));
        assert!(
            effective_external_content_declarations(
                &realized,
                Some(&contract),
                DeclaringAuthority::Project
            )
            .is_err()
        );
    }

    #[test]
    fn selected_child_slot_is_owned_while_parent_only_content_is_not() {
        let contract = contract(&[], 2);
        let mut parent_only = product_resolution();
        parent_only.composed.composed = serde_json::json!({});
        assert!(
            effective_external_content_declarations(
                &parent_only,
                Some(&contract),
                DeclaringAuthority::Project,
            )
            .unwrap()
            .is_none()
        );

        let mut selected = product_resolution();
        let selection = product_selection(&selected);
        insert_resolved_product_selections(&mut selected, product_selections(selection), &contract)
            .unwrap();
        selected.composed.derived.insert(
            EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),
            serde_json::json!([
                {"id":"runtime","kind":"tree","mode":"pinned",
                 "manifest_hash":"6".repeat(64),"entry_count":1,"total_bytes":1,
                 "mount_root":"project","mount":"environment/runtime"},
                {"id":"parent-only","kind":"tree","mode":"pinned",
                 "manifest_hash":"9".repeat(64),"entry_count":1,"total_bytes":1,
                 "mount_root":"project","mount":"environment/parent"}
            ]),
        );
        assert!(
            effective_external_content_declarations(
                &selected,
                Some(&contract),
                DeclaringAuthority::Project,
            )
            .is_err(),
            "a merged inherited realization is not part of the child's declaration union"
        );
        selected
            .composed
            .derived
            .remove(EXTERNAL_REALIZATIONS_DERIVED_KEY);
        let declarations = effective_external_content_declarations(
            &selected,
            Some(&contract),
            DeclaringAuthority::Project,
        )
        .unwrap()
        .unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].id, "runtime");
        assert_eq!(
            declarations[0].digest.as_deref(),
            Some("6".repeat(64).as_str())
        );
    }

    #[test]
    fn product_selection_rejects_source_mismatch_unknown_fields_and_partial_mutation() {
        let contract = contract(&[], 2);
        let original = product_resolution();
        let correct = product_selection(&original);
        for mutate in [
            |selection: &mut ResolvedExternalProductSelection| match &mut selection.consumer_source
            {
                ResolvedProductConsumerSource::PinnedProject {
                    publisher_fingerprint,
                    ..
                } => *publisher_fingerprint = "0".repeat(64),
                ResolvedProductConsumerSource::InstalledBundle { .. } => unreachable!(),
            },
            |selection: &mut ResolvedExternalProductSelection| {
                selection.pre_selection_effective_definition_digest = "0".repeat(64)
            },
            |selection: &mut ResolvedExternalProductSelection| {
                selection.relationship_ref = "config:other/recipe".to_owned()
            },
            |selection: &mut ResolvedExternalProductSelection| {
                selection.declaration.mount = "other".to_owned()
            },
        ] {
            let mut resolution = original.clone();
            let mut selection = correct.clone();
            mutate(&mut selection);
            assert!(
                insert_resolved_product_selections(
                    &mut resolution,
                    product_selections(selection),
                    &contract,
                )
                .is_err()
            );
            assert_eq!(
                resolution.effective_definition_digest().unwrap(),
                original.effective_definition_digest().unwrap()
            );
        }
        let mut resolution = original.clone();
        insert_resolved_product_selections(&mut resolution, product_selections(correct), &contract)
            .unwrap();
        resolution
            .composed
            .derived
            .get_mut(EXTERNAL_PRODUCT_SELECTIONS_DERIVED_KEY)
            .unwrap()["runtime"]["unchecked"] = serde_json::json!(true);
        assert!(
            effective_external_content_declarations(
                &resolution,
                Some(&contract),
                DeclaringAuthority::Project
            )
            .is_err()
        );
    }

    #[test]
    fn product_selection_batch_requires_complete_same_d0_projection() {
        let contract = contract(&[], 2);
        let mut original = product_resolution();
        original.composed.composed["external_product_slots"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id":"support", "relationship_ref":"config:fixture/recipe",
                "relationship":"support_to_worker", "kind":"tree",
                "mount_root":"project", "mount":"environment/support"
            }));
        let first = product_selection(&original);
        let second = second_product_selection(first.clone());
        let complete = ResolvedExternalProductSelections::new(std::collections::BTreeMap::from([
            (first.declaration_id.clone(), first.clone()),
            (second.declaration_id.clone(), second.clone()),
        ]))
        .unwrap();

        let mut selected = original.clone();
        insert_resolved_product_selections(&mut selected, complete, &contract).unwrap();
        assert_eq!(
            effective_external_content_declarations(
                &selected,
                Some(&contract),
                DeclaringAuthority::Project,
            )
            .unwrap()
            .unwrap()
            .len(),
            2
        );

        let mut partial = original.clone();
        assert!(
            insert_resolved_product_selections(&mut partial, product_selections(first), &contract,)
                .is_err()
        );
        assert_eq!(
            partial.effective_definition_digest().unwrap(),
            original.effective_definition_digest().unwrap()
        );

        let mut mismatched = second;
        mismatched.pre_selection_effective_definition_digest = "9".repeat(64);
        let invalid: ResolvedExternalProductSelections = serde_json::from_value(
            serde_json::to_value(std::collections::BTreeMap::from([
                ("runtime".to_owned(), product_selection(&original)),
                ("support".to_owned(), mismatched),
            ]))
            .unwrap(),
        )
        .unwrap();
        assert!(insert_resolved_product_selections(&mut original, invalid, &contract).is_err());
    }

    #[test]
    fn selected_source_authority_matches_bundle_or_exact_project_closure() {
        let contract = contract(&[], 2);
        let mut bundle = product_resolution();
        bundle.root.resolved_ref = "tool:fixture/verifier".to_owned();
        bundle.root.source_space = ItemSpace::Bundle;
        bundle.root.source_root = crate::contracts::ItemSourceRoot::Bundle {
            name: "fixture".to_owned(),
        };
        let mut bundle_selection = product_selection(&bundle);
        bundle_selection.relationship.consumer.canonical_ref = bundle.root.resolved_ref.clone();
        bundle_selection.consumer_source = ResolvedProductConsumerSource::InstalledBundle {
            consumer_ref: bundle.root.resolved_ref.clone(),
            publisher_fingerprint: "a".repeat(64),
        };
        insert_resolved_product_selections(
            &mut bundle,
            product_selections(bundle_selection),
            &contract,
        )
        .unwrap();

        let source_closure = ryeos_state::objects::EffectiveSourceClosureProjection {
            schema: ryeos_state::objects::EFFECTIVE_SOURCE_BINDING_SCHEMA,
            binding_hash: "1".repeat(64),
            content_manifest_hash: "2".repeat(64),
            owner_key: "3".repeat(64),
            file_count: 1,
            total_bytes: 7,
        };
        let mut project = product_resolution();
        project.composed.derived.insert(
            ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY.to_owned(),
            source_closure.to_value().unwrap(),
        );
        let selection = product_selection(&project);
        let mut rejected = project.clone();
        assert!(
            insert_resolved_product_selections(
                &mut rejected,
                product_selections(selection.clone()),
                &contract,
            )
            .is_err()
        );
        let mut exact = selection;
        let ResolvedProductConsumerSource::PinnedProject {
            source_closure: admitted,
            ..
        } = &mut exact.consumer_source
        else {
            unreachable!()
        };
        *admitted = Some(source_closure);
        insert_resolved_product_selections(&mut project, product_selections(exact), &contract)
            .unwrap();
    }

    #[test]
    fn enclosing_content_identity_tracks_product_and_literal_consumers() {
        use crate::content_dependencies::{
            EffectiveContentDependencyIdentities, EffectiveContentDependencyIdentity,
            bind_effective_content_dependency_identities,
        };
        let project = product_resolution();
        let mut product = project.clone();
        let selected = product_selection(&project);
        insert_resolved_product_selections(
            &mut product,
            product_selections(selected.clone()),
            &contract(&[], 2),
        )
        .unwrap();
        let mut another_product = project.clone();
        let mut another_selection = selected;
        another_selection.witness_hash = "7".repeat(64);
        insert_resolved_product_selections(
            &mut another_product,
            product_selections(another_selection),
            &contract(&[], 2),
        )
        .unwrap();

        let mut literal = project.clone();
        literal.composed.composed = serde_json::json!({"external_content":[{
            "id":"runtime","kind":"tree","mode":"pinned","digest":"8".repeat(64),
            "mount_root":"project","mount":"environment/runtime"
        }]});
        let mut another_literal = literal.clone();
        another_literal.composed.composed["external_content"][0]["digest"] =
            serde_json::json!("9".repeat(64));
        // Admission supplies these typed realized projections. This test proves
        // propagation of their identity, not CAS availability or operator grant.
        for consumer in [
            &mut product,
            &mut another_product,
            &mut literal,
            &mut another_literal,
        ] {
            let declarations = effective_external_content_declarations(
                consumer,
                Some(&contract(&[], 2)),
                DeclaringAuthority::Project,
            )
            .unwrap()
            .unwrap();
            consumer.composed.derived.insert(EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),serde_json::json!([{
                "id":"runtime","kind":"tree","mode":"pinned","manifest_hash":declarations[0].digest,
                "entry_count":1,"total_bytes":1,"mount_root":"project","mount":"environment/runtime"
            }]));
        }
        let identity = |consumer: &crate::resolution::ResolutionOutput| {
            EffectiveContentDependencyIdentities::from([(
                "environment".to_owned(),
                EffectiveContentDependencyIdentity {
                    canonical_ref: consumer.root.resolved_ref.clone(),
                    effective_definition_digest: consumer
                        .effective_definition_digest()
                        .unwrap()
                        .as_str()
                        .to_owned(),
                    targets: vec!["session_worker".to_owned()],
                    executable_search: vec![
                        ryeos_handler_protocol::ExecutableSearchPathEntryWire {
                            realization_id: "runtime".to_owned(),
                            relative_directory: "bin".to_owned(),
                        },
                        ryeos_handler_protocol::ExecutableSearchPathEntryWire {
                            realization_id: "runtime".to_owned(),
                            relative_directory: "libexec".to_owned(),
                        },
                    ],
                },
            )])
        };
        let mut outer = project.clone();
        outer.composed.composed = serde_json::json!({"config":{"fixture":"outer-worker"}});
        let authored = outer.authored_definition_digest().unwrap();
        for (left, right) in [(&product, &another_product), (&literal, &another_literal)] {
            let mut a = outer.clone();
            let mut b = outer.clone();
            bind_effective_content_dependency_identities(&mut a, identity(left), false).unwrap();
            bind_effective_content_dependency_identities(&mut b, identity(right), false).unwrap();
            assert_ne!(
                a.effective_definition_digest().unwrap(),
                b.effective_definition_digest().unwrap()
            );
            assert_eq!(a.authored_definition_digest().unwrap(), authored);
            assert_eq!(b.authored_definition_digest().unwrap(), authored);
            let mut recovered = crate::resolution::RetainedResolutionOutput::capture(&a).restore();
            bind_effective_content_dependency_identities(&mut recovered, identity(left), true)
                .unwrap();
            assert!(
                bind_effective_content_dependency_identities(&mut recovered, identity(right), true)
                    .is_err()
            );
            assert!(
                bind_effective_content_dependency_identities(&mut recovered, identity(left), false)
                    .is_err()
            );
            let mut changed_search = identity(left);
            changed_search
                .get_mut("environment")
                .unwrap()
                .executable_search[0]
                .relative_directory = "other".to_owned();
            assert!(
                bind_effective_content_dependency_identities(&mut recovered, changed_search, true)
                    .is_err()
            );
            let mut reordered_search = identity(left);
            reordered_search
                .get_mut("environment")
                .unwrap()
                .executable_search
                .reverse();
            assert!(
                bind_effective_content_dependency_identities(
                    &mut recovered,
                    reordered_search.clone(),
                    true,
                )
                .is_err()
            );
            let mut reordered = outer.clone();
            bind_effective_content_dependency_identities(&mut reordered, reordered_search, false)
                .unwrap();
            assert_ne!(
                reordered.effective_definition_digest().unwrap(),
                a.effective_definition_digest().unwrap()
            );
            assert_eq!(reordered.authored_definition_digest().unwrap(), authored);
            assert!(
                bind_effective_content_dependency_identities(
                    &mut recovered,
                    EffectiveContentDependencyIdentities::new(),
                    true,
                )
                .is_err()
            );
        }
        let original_digest = outer.effective_definition_digest().unwrap();
        bind_effective_content_dependency_identities(
            &mut outer,
            EffectiveContentDependencyIdentities::new(),
            false,
        )
        .unwrap();
        assert_eq!(
            outer.effective_definition_digest().unwrap(),
            original_digest
        );
        bind_effective_content_dependency_identities(
            &mut outer,
            EffectiveContentDependencyIdentities::new(),
            true,
        )
        .unwrap();
        assert!(
            bind_effective_content_dependency_identities(&mut outer, identity(&literal), true)
                .is_err()
        );
        outer.composed.derived.insert(
            crate::content_dependencies::EFFECTIVE_CONTENT_DEPENDENCIES_DERIVED_KEY.to_owned(),
            serde_json::json!({}),
        );
        // No dependencies canonically means no key, not an authored empty
        // projection. Neither fresh nor recovered admission accepts it.
        assert!(
            bind_effective_content_dependency_identities(
                &mut outer,
                EffectiveContentDependencyIdentities::new(),
                false,
            )
            .is_err()
        );
        assert!(
            bind_effective_content_dependency_identities(
                &mut outer,
                EffectiveContentDependencyIdentities::new(),
                true,
            )
            .is_err()
        );
    }

    #[test]
    fn realization_command_ref_is_a_canonical_declaration_member_selector() {
        let parsed = parse_realization_command_ref("realization:toolchain/bin/rustc")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.realization_id, "toolchain");
        assert_eq!(parsed.relative_path, "bin/rustc");
        assert!(
            parse_realization_command_ref("/usr/bin/rustc")
                .unwrap()
                .is_none()
        );
        assert!(parse_realization_command_ref("realization:toolchain/../rustc").is_err());
        assert!(parse_realization_command_ref("realization:toolchain").is_err());
        assert!(parse_realization_command_ref("realization:/bin/rustc").is_err());
    }

    #[test]
    fn absence_empty_and_null_are_distinct() {
        let contract = contract(&["project_files"], 2);
        assert!(
            declarations_from_composed(
                &serde_json::json!({}),
                Some(&contract),
                DeclaringAuthority::Project
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(
            declarations_from_composed(
                &serde_json::json!({"external_content": []}),
                Some(&contract),
                DeclaringAuthority::Project
            )
            .unwrap(),
            Some(Vec::new())
        );
        assert!(
            declarations_from_composed(
                &serde_json::json!({"external_content": null}),
                Some(&contract),
                DeclaringAuthority::Project
            )
            .is_err()
        );
    }

    #[test]
    fn declarer_and_kind_contract_both_constrain_roots() {
        let value = serde_json::json!({"external_content": [{
            "id": "fixture",
            "kind": "tree",
            "locator": {"root": "node_files", "path": "fixture"},
            "mode": "captured",
            "mount_root": "project",
            "mount": "fixture"
        }]});
        assert!(
            declarations_from_composed(
                &value,
                Some(&contract(&["node_files"], 1)),
                DeclaringAuthority::Project
            )
            .is_err()
        );
    }

    #[test]
    fn runtime_mount_requires_explicit_signed_kind_permission() {
        let mut value = serde_json::json!({"external_content": [{
            "id": "platform", "kind": "tree", "mode": "pinned",
            "digest": "a".repeat(64), "mount_root": "execution_runtime", "mount": "platform"
        }]});
        let mut policy = contract(&[], 1);
        assert!(
            declarations_from_composed(&value, Some(&policy), DeclaringAuthority::Project).is_err()
        );
        policy
            .allowed_mount_roots
            .push(ExternalContentMountRoot::ExecutionRuntime);
        assert!(
            declarations_from_composed(&value, Some(&policy), DeclaringAuthority::Project).is_ok()
        );
        value["external_content"][0]
            .as_object_mut()
            .unwrap()
            .remove("mount_root");
        assert!(
            declarations_from_composed(&value, Some(&policy), DeclaringAuthority::Project).is_err()
        );
    }

    #[test]
    fn empty_allowed_roots_accepts_only_locator_free_pins() {
        let digest = "a".repeat(64);
        let locator_free = serde_json::json!({"external_content": [{
            "id": "fixture",
            "kind": "file",
            "mode": "pinned",
            "digest": digest,
            "mount_root": "project",
            "mount": "bin/fixture"
        }]});
        assert!(
            declarations_from_composed(
                &locator_free,
                Some(&contract(&[], 1)),
                DeclaringAuthority::Bundle("fixture")
            )
            .is_ok()
        );

        let locator_backed = serde_json::json!({"external_content": [{
            "id": "fixture",
            "kind": "file",
            "locator": {"root": "bundle:fixture", "path": "bin/fixture"},
            "mode": "pinned",
            "digest": "a".repeat(64),
            "mount_root": "project",
            "mount": "bin/fixture"
        }]});
        assert!(
            declarations_from_composed(
                &locator_backed,
                Some(&contract(&[], 1)),
                DeclaringAuthority::Bundle("fixture")
            )
            .is_err()
        );
    }

    #[test]
    fn pending_pin_tokens_are_not_a_declaration_state() {
        let value = serde_json::json!({"external_content": [{
            "id": "fixture",
            "kind": "tree",
            "locator": {"root": "project_files", "path": "vendor/fixture"},
            "mode": "pinned",
            "digest": "PENDING_FIXTURE_DIGEST",
            "mount_root": "project",
            "mount": "vendor/fixture"
        }]});
        assert!(
            declarations_from_composed(
                &value,
                Some(&contract(&["project_files"], 1)),
                DeclaringAuthority::Project
            )
            .is_err()
        );
    }

    #[test]
    fn missing_pinned_digest_exists_only_in_the_unsigned_authoring_contract() {
        let value = serde_json::json!({"external_content": [{
            "id": "fixture",
            "kind": "tree",
            "locator": {"root": "project_files", "path": "vendor/fixture"},
            "mode": "pinned",
            "mount_root": "project",
            "mount": "vendor/fixture"
        }]});
        assert!(
            declarations_from_composed(
                &value,
                Some(&contract(&["project_files"], 1)),
                DeclaringAuthority::Project,
            )
            .is_err()
        );
        let draft = declarations_from_authored_pin_draft(
            &value,
            Some(&contract(&["project_files"], 1)),
            DeclaringAuthority::Project,
        )
        .unwrap();
        assert_eq!(draft.len(), 1);
        assert_eq!(draft[0].digest, None);
    }
}
