//! Canonical executable-definition identity.
//!
//! The document serialized here is both the digest input and the structural
//! value consumed by comparison. Keeping those uses on one owned type makes a
//! second, drift-prone reconstruction impossible.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use super::types::{
    EXTERNAL_REALIZATIONS_DERIVED_KEY, EffectiveDefinitionDigest, EffectiveDefinitionDigestError,
    KindComposedView, ResolutionEdge, ResolutionOutput, ResolutionStepName, ResolvedAncestor,
    TrustClass,
};
use crate::contracts::{ItemSourceRoot, ItemSpace};

pub const MAX_IDENTITY_DIFF_VISITS: usize = 32_768;
pub const MAX_IDENTITY_DIFF_ROWS: usize = 512;
pub const MAX_IDENTITY_COORDINATE_BYTES: usize = 512;
pub const MAX_PUBLIC_SCALAR_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionChangeCategory {
    Root,
    Ancestor,
    ReferencedItem,
    ReferenceEdge,
    EffectiveTrust,
    HookPlan,
    Policy,
    ExternalRealization,
    SourceClosure,
    ComposedProgram,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionChangeKind {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionValueType {
    Null,
    Boolean,
    Number,
    String,
    Array,
    Object,
}

/// Bounded public projection of one changed value.
///
/// `public_scalar` is populated only for an engine-owned public identity
/// field. Generic composed values remain type-only even when they happen to
/// look like a hash or canonical ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionValueSummary {
    pub value_type: DefinitionValueType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_scalar: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionIdentityChange {
    pub category: DefinitionChangeCategory,
    pub coordinate: String,
    pub change: DefinitionChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub left: Option<DefinitionValueSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub right: Option<DefinitionValueSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionIdentityDiff {
    pub left_digest: EffectiveDefinitionDigest,
    pub right_digest: EffectiveDefinitionDigest,
    pub complete: bool,
    pub changes: Vec<DefinitionIdentityChange>,
    pub omitted_changes: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
struct DefinitionDiffLimits {
    visits: usize,
    rows: usize,
    coordinate_bytes: usize,
    public_scalar_bytes: usize,
}

impl Default for DefinitionDiffLimits {
    fn default() -> Self {
        Self {
            visits: MAX_IDENTITY_DIFF_VISITS,
            rows: MAX_IDENTITY_DIFF_ROWS,
            coordinate_bytes: MAX_IDENTITY_COORDINATE_BYTES,
            public_scalar_bytes: MAX_PUBLIC_SCALAR_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefinitionIdentityScope {
    Effective,
    Authored,
}

impl DefinitionIdentityScope {
    fn schema_tag(self) -> &'static str {
        match self {
            Self::Effective => "ryeos.effective_definition.v2",
            Self::Authored => "ryeos.authored_definition.v2",
        }
    }

    fn composed_view(self, composed: &KindComposedView) -> KindComposedView {
        let mut view = composed.clone();
        if matches!(self, Self::Authored) {
            view.derived.retain(|key, _| {
                !matches!(
                    realization_derived_kind(key),
                    Some(RealizationDerivedKind::External | RealizationDerivedKind::Source)
                )
            });
        }
        view
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RealizationDerivedKind {
    External,
    Source,
}

fn realization_derived_kind(key: &str) -> Option<RealizationDerivedKind> {
    match key {
        EXTERNAL_REALIZATIONS_DERIVED_KEY => Some(RealizationDerivedKind::External),
        ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY => Some(RealizationDerivedKind::Source),
        _ => None,
    }
}

/// Path-free canonical document that names one executable definition.
///
/// Fields remain private so callers cannot manufacture a lookalike document.
/// Construction validates the exact finalized resolution contributors and
/// preserves the current v2 canonical byte contract.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionIdentityDocument {
    schema: &'static str,
    root: DefinitionContributor,
    ancestors: Vec<DefinitionContributor>,
    referenced_items: Vec<DefinitionContributor>,
    reference_edges: Vec<DefinitionReferenceEdge>,
    effective_trust_class: TrustClass,
    composed: KindComposedView,
}

impl DefinitionIdentityDocument {
    pub(crate) fn from_resolution(
        resolution: &ResolutionOutput,
        scope: DefinitionIdentityScope,
    ) -> Result<Self, EffectiveDefinitionDigestError> {
        let mut referenced_items = resolution
            .referenced_items
            .iter()
            .map(DefinitionContributor::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        referenced_items.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));

        let mut reference_edges = resolution
            .references_edges
            .iter()
            .map(DefinitionReferenceEdge::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        reference_edges.sort_by(|left, right| left.sort_key().cmp(&right.sort_key()));

        Ok(Self {
            schema: scope.schema_tag(),
            root: DefinitionContributor::try_from(&resolution.root)?,
            ancestors: resolution
                .ancestors
                .iter()
                .map(DefinitionContributor::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            referenced_items,
            reference_edges,
            effective_trust_class: resolution.effective_trust_class,
            composed: scope.composed_view(&resolution.composed),
        })
    }

    /// Exact canonical JSON bytes consumed by definition hashing.
    pub fn canonical_json(&self) -> Result<String, EffectiveDefinitionDigestError> {
        let value = serde_json::to_value(self).map_err(|error| {
            EffectiveDefinitionDigestError(format!(
                "serialize effective-definition document: {error}"
            ))
        })?;
        lillux::cas::canonical_json(&value).map_err(|error| {
            EffectiveDefinitionDigestError(format!(
                "canonicalize effective-definition document: {error}"
            ))
        })
    }

    /// Digest of these exact canonical bytes.
    pub fn digest(&self) -> Result<EffectiveDefinitionDigest, EffectiveDefinitionDigestError> {
        let canonical = self.canonical_json()?;
        EffectiveDefinitionDigest::parse(lillux::cas::sha256_hex(canonical.as_bytes()))
    }

    /// Bounded structural comparison of the exact documents that are hashed.
    pub fn diff(
        &self,
        right: &Self,
    ) -> Result<DefinitionIdentityDiff, EffectiveDefinitionDigestError> {
        self.diff_with_limits(right, DefinitionDiffLimits::default())
    }

    fn diff_with_limits(
        &self,
        right: &Self,
        limits: DefinitionDiffLimits,
    ) -> Result<DefinitionIdentityDiff, EffectiveDefinitionDigestError> {
        let left_digest = self.digest()?;
        let right_digest = right.digest()?;
        if left_digest == right_digest {
            return Ok(DefinitionIdentityDiff {
                left_digest,
                right_digest,
                complete: true,
                changes: Vec::new(),
                omitted_changes: Some(0),
            });
        }

        let mut builder = DefinitionDiffBuilder::new(limits);
        builder.diff_contributor(
            DefinitionChangeCategory::Root,
            "root".to_string(),
            Some(&self.root),
            Some(&right.root),
        );
        builder.diff_ancestors(&self.ancestors, &right.ancestors);
        builder.diff_referenced_items(&self.referenced_items, &right.referenced_items);
        builder.diff_reference_edges(&self.reference_edges, &right.reference_edges);
        builder.diff_closed_string(
            DefinitionChangeCategory::EffectiveTrust,
            "effective_trust_class".to_string(),
            trust_class_name(self.effective_trust_class),
            trust_class_name(right.effective_trust_class),
        );
        builder.diff_composed(&self.composed, &right.composed)?;
        Ok(builder.finish(left_digest, right_digest))
    }
}

#[derive(Debug, Clone, Copy)]
enum JsonDisclosure {
    TypeOnly,
    ValidatedExternalRealization,
}

struct DefinitionDiffBuilder {
    limits: DefinitionDiffLimits,
    visits: usize,
    complete: bool,
    halted: bool,
    changes: Vec<DefinitionIdentityChange>,
}

impl DefinitionDiffBuilder {
    fn new(limits: DefinitionDiffLimits) -> Self {
        Self {
            limits,
            visits: 0,
            complete: true,
            halted: false,
            changes: Vec::new(),
        }
    }

    fn visit(&mut self) -> bool {
        if self.halted {
            return false;
        }
        if self.visits >= self.limits.visits {
            self.complete = false;
            self.halted = true;
            return false;
        }
        self.visits += 1;
        true
    }

    fn finish(
        mut self,
        left_digest: EffectiveDefinitionDigest,
        right_digest: EffectiveDefinitionDigest,
    ) -> DefinitionIdentityDiff {
        self.changes.sort_by(|left, right| {
            (left.category, &left.coordinate, left.change).cmp(&(
                right.category,
                &right.coordinate,
                right.change,
            ))
        });
        DefinitionIdentityDiff {
            left_digest,
            right_digest,
            complete: self.complete,
            changes: self.changes,
            omitted_changes: self.complete.then_some(0),
        }
    }

    fn push(
        &mut self,
        category: DefinitionChangeCategory,
        coordinate: String,
        change: DefinitionChangeKind,
        left: Option<DefinitionValueSummary>,
        right: Option<DefinitionValueSummary>,
    ) {
        if self.halted {
            return;
        }
        if coordinate.len() > self.limits.coordinate_bytes {
            self.complete = false;
            return;
        }
        if self.changes.len() >= self.limits.rows {
            self.complete = false;
            self.halted = true;
            return;
        }
        self.changes.push(DefinitionIdentityChange {
            category,
            coordinate,
            change,
            left,
            right,
        });
    }

    fn public_string(&mut self, value: &str) -> DefinitionValueSummary {
        let public_scalar = if value.len() <= self.limits.public_scalar_bytes {
            Some(value.to_string())
        } else {
            self.complete = false;
            None
        };
        DefinitionValueSummary {
            value_type: DefinitionValueType::String,
            public_scalar,
        }
    }

    fn private_string() -> DefinitionValueSummary {
        DefinitionValueSummary {
            value_type: DefinitionValueType::String,
            public_scalar: None,
        }
    }

    fn object_summary() -> DefinitionValueSummary {
        DefinitionValueSummary {
            value_type: DefinitionValueType::Object,
            public_scalar: None,
        }
    }

    fn diff_contributor(
        &mut self,
        category: DefinitionChangeCategory,
        coordinate: String,
        left: Option<&DefinitionContributor>,
        right: Option<&DefinitionContributor>,
    ) {
        if !self.visit() {
            return;
        }
        match (left, right) {
            (None, None) => {}
            (None, Some(_)) => self.push(
                category,
                coordinate,
                DefinitionChangeKind::Added,
                None,
                Some(Self::object_summary()),
            ),
            (Some(_), None) => self.push(
                category,
                coordinate,
                DefinitionChangeKind::Removed,
                Some(Self::object_summary()),
                None,
            ),
            (Some(left), Some(right)) => {
                self.diff_public_string(
                    category,
                    format!("{coordinate}.canonical_ref"),
                    &left.canonical_ref,
                    &right.canonical_ref,
                );
                self.diff_public_string(
                    category,
                    format!("{coordinate}.root_raw_content_digest"),
                    &left.root_raw_content_digest,
                    &right.root_raw_content_digest,
                );
                self.diff_closed_string(
                    category,
                    format!("{coordinate}.source_space"),
                    left.source_space.as_str(),
                    right.source_space.as_str(),
                );
                self.diff_source_root(
                    category,
                    format!("{coordinate}.source_root"),
                    &left.source_root,
                    &right.source_root,
                );
                self.diff_closed_string(
                    category,
                    format!("{coordinate}.trust_class"),
                    trust_class_name(left.trust_class),
                    trust_class_name(right.trust_class),
                );
                self.diff_optional_public_string(
                    category,
                    format!("{coordinate}.signer_fingerprint"),
                    left.signer_fingerprint.as_deref(),
                    right.signer_fingerprint.as_deref(),
                );
                self.diff_closed_string(
                    category,
                    format!("{coordinate}.added_by"),
                    resolution_step_name(left.added_by),
                    resolution_step_name(right.added_by),
                );
            }
        }
    }

    fn diff_ancestors(&mut self, left: &[DefinitionContributor], right: &[DefinitionContributor]) {
        for index in 0..left.len().max(right.len()) {
            if self.halted {
                break;
            }
            self.diff_contributor(
                DefinitionChangeCategory::Ancestor,
                format!("ancestors[{index}]"),
                left.get(index),
                right.get(index),
            );
        }
    }

    fn diff_referenced_items(
        &mut self,
        left: &[DefinitionContributor],
        right: &[DefinitionContributor],
    ) {
        let left = group_contributors(left);
        let right = group_contributors(right);
        let keys = left
            .keys()
            .chain(right.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for (canonical_ref, added_by) in keys {
            let left_group = left.get(&(canonical_ref.clone(), added_by.clone()));
            let right_group = right.get(&(canonical_ref.clone(), added_by.clone()));
            let width = left_group
                .map(Vec::len)
                .unwrap_or(0)
                .max(right_group.map(Vec::len).unwrap_or(0));
            let encoded_ref =
                serde_json::to_string(&canonical_ref).unwrap_or_else(|_| "\"invalid\"".to_string());
            for occurrence in 0..width {
                if self.halted {
                    return;
                }
                self.diff_contributor(
                    DefinitionChangeCategory::ReferencedItem,
                    format!(
                        "referenced_items[canonical_ref={encoded_ref},added_by={added_by},occurrence={occurrence}]"
                    ),
                    left_group.and_then(|group| group.get(occurrence).copied()),
                    right_group.and_then(|group| group.get(occurrence).copied()),
                );
            }
        }
    }

    fn diff_reference_edges(
        &mut self,
        left: &[DefinitionReferenceEdge],
        right: &[DefinitionReferenceEdge],
    ) {
        let left = group_edges(left);
        let right = group_edges(right);
        let keys = left
            .keys()
            .chain(right.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for (from_ref, to_ref, added_by) in keys {
            let key = (from_ref.clone(), to_ref.clone(), added_by.clone());
            let left_group = left.get(&key);
            let right_group = right.get(&key);
            let width = left_group
                .map(Vec::len)
                .unwrap_or(0)
                .max(right_group.map(Vec::len).unwrap_or(0));
            let from_ref =
                serde_json::to_string(&from_ref).unwrap_or_else(|_| "\"invalid\"".to_string());
            let to_ref =
                serde_json::to_string(&to_ref).unwrap_or_else(|_| "\"invalid\"".to_string());
            for occurrence in 0..width {
                if self.halted {
                    return;
                }
                self.diff_edge(
                    format!(
                        "reference_edges[from_ref={from_ref},to_ref={to_ref},added_by={added_by},occurrence={occurrence}]"
                    ),
                    left_group.and_then(|group| group.get(occurrence).copied()),
                    right_group.and_then(|group| group.get(occurrence).copied()),
                );
            }
        }
    }

    fn diff_edge(
        &mut self,
        coordinate: String,
        left: Option<&DefinitionReferenceEdge>,
        right: Option<&DefinitionReferenceEdge>,
    ) {
        if !self.visit() {
            return;
        }
        let category = DefinitionChangeCategory::ReferenceEdge;
        match (left, right) {
            (None, None) => {}
            (None, Some(_)) => self.push(
                category,
                coordinate,
                DefinitionChangeKind::Added,
                None,
                Some(Self::object_summary()),
            ),
            (Some(_), None) => self.push(
                category,
                coordinate,
                DefinitionChangeKind::Removed,
                Some(Self::object_summary()),
                None,
            ),
            (Some(left), Some(right)) => {
                self.diff_public_string(
                    category,
                    format!("{coordinate}.from_ref"),
                    &left.from_ref,
                    &right.from_ref,
                );
                self.diff_public_string(
                    category,
                    format!("{coordinate}.to_ref"),
                    &left.to_ref,
                    &right.to_ref,
                );
                self.diff_closed_string(
                    category,
                    format!("{coordinate}.to_source_space"),
                    left.to_source_space.as_str(),
                    right.to_source_space.as_str(),
                );
                self.diff_closed_string(
                    category,
                    format!("{coordinate}.trust_class"),
                    trust_class_name(left.trust_class),
                    trust_class_name(right.trust_class),
                );
                self.diff_closed_string(
                    category,
                    format!("{coordinate}.added_by"),
                    resolution_step_name(left.added_by),
                    resolution_step_name(right.added_by),
                );
            }
        }
    }

    fn diff_source_root(
        &mut self,
        category: DefinitionChangeCategory,
        coordinate: String,
        left: &ItemSourceRoot,
        right: &ItemSourceRoot,
    ) {
        let (left_kind, left_name) = source_root_sort_key(left);
        let (right_kind, right_name) = source_root_sort_key(right);
        self.diff_closed_string(
            category,
            format!("{coordinate}.kind"),
            left_kind,
            right_kind,
        );
        if left_kind == right_kind && left_name != right_name {
            self.push(
                category,
                format!("{coordinate}.identity"),
                DefinitionChangeKind::Changed,
                Some(Self::private_string()),
                Some(Self::private_string()),
            );
        }
    }

    fn diff_public_string(
        &mut self,
        category: DefinitionChangeCategory,
        coordinate: String,
        left: &str,
        right: &str,
    ) {
        if left == right || !self.visit() {
            return;
        }
        let left = self.public_string(left);
        let right = self.public_string(right);
        self.push(
            category,
            coordinate,
            DefinitionChangeKind::Changed,
            Some(left),
            Some(right),
        );
    }

    fn diff_closed_string(
        &mut self,
        category: DefinitionChangeCategory,
        coordinate: String,
        left: &str,
        right: &str,
    ) {
        self.diff_public_string(category, coordinate, left, right);
    }

    fn diff_optional_public_string(
        &mut self,
        category: DefinitionChangeCategory,
        coordinate: String,
        left: Option<&str>,
        right: Option<&str>,
    ) {
        if left == right || !self.visit() {
            return;
        }
        let left = left.map(|value| self.public_string(value)).or_else(|| {
            Some(DefinitionValueSummary {
                value_type: DefinitionValueType::Null,
                public_scalar: None,
            })
        });
        let right = right.map(|value| self.public_string(value)).or_else(|| {
            Some(DefinitionValueSummary {
                value_type: DefinitionValueType::Null,
                public_scalar: None,
            })
        });
        self.push(
            category,
            coordinate,
            DefinitionChangeKind::Changed,
            left,
            right,
        );
    }

    fn diff_composed(
        &mut self,
        left: &KindComposedView,
        right: &KindComposedView,
    ) -> Result<(), EffectiveDefinitionDigestError> {
        self.diff_json(
            DefinitionChangeCategory::ComposedProgram,
            "composed".to_string(),
            Some(&left.composed),
            Some(&right.composed),
            JsonDisclosure::TypeOnly,
            false,
        );

        let derived_keys = left
            .derived
            .keys()
            .chain(right.derived.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut opaque_slot = 0usize;
        for key in derived_keys {
            if self.halted {
                break;
            }
            let left_value = left.derived.get(&key);
            let right_value = right.derived.get(&key);
            if key == crate::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY {
                self.diff_json(
                    DefinitionChangeCategory::HookPlan,
                    "derived.hook_plan".to_string(),
                    left_value,
                    right_value,
                    JsonDisclosure::TypeOnly,
                    false,
                );
            } else if key == EXTERNAL_REALIZATIONS_DERIVED_KEY {
                self.diff_external_realizations(left_value, right_value)?;
            } else if key == ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY {
                self.diff_source_closure(left_value, right_value)?;
            } else {
                let coordinate = format!("derived.slot[{opaque_slot:04}]");
                opaque_slot += 1;
                self.diff_json(
                    DefinitionChangeCategory::ComposedProgram,
                    coordinate,
                    left_value,
                    right_value,
                    JsonDisclosure::TypeOnly,
                    false,
                );
            }
        }

        let policy_keys = left
            .policy_facts
            .keys()
            .chain(right.policy_facts.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for (slot, key) in policy_keys.into_iter().enumerate() {
            if self.halted {
                break;
            }
            self.diff_json(
                DefinitionChangeCategory::Policy,
                format!("policy.slot[{slot:04}]"),
                left.policy_facts.get(&key),
                right.policy_facts.get(&key),
                JsonDisclosure::TypeOnly,
                false,
            );
        }
        Ok(())
    }

    fn diff_external_realizations(
        &mut self,
        left: Option<&serde_json::Value>,
        right: Option<&serde_json::Value>,
    ) -> Result<(), EffectiveDefinitionDigestError> {
        let parse = |value: Option<&serde_json::Value>| {
            value
                .map(ryeos_state::objects::ExternalContentRealizationSet::from_value)
                .transpose()
                .map(|set| set.unwrap_or_default())
                .map_err(|error| {
                    EffectiveDefinitionDigestError(format!(
                        "invalid external realization identity slot: {error}"
                    ))
                })
        };
        let left = parse(left)?;
        let right = parse(right)?;
        let left_by_id = left
            .iter()
            .map(|entry| (entry.id.as_str(), entry))
            .collect::<BTreeMap<_, _>>();
        let right_by_id = right
            .iter()
            .map(|entry| (entry.id.as_str(), entry))
            .collect::<BTreeMap<_, _>>();
        let ids = left_by_id
            .keys()
            .chain(right_by_id.keys())
            .copied()
            .collect::<BTreeSet<_>>();

        for (slot, id) in ids.into_iter().enumerate() {
            if self.halted {
                break;
            }
            let left_value = left_by_id
                .get(id)
                .map(|entry| serde_json::to_value(entry))
                .transpose()
                .map_err(|error| {
                    EffectiveDefinitionDigestError(format!(
                        "serialize external realization identity: {error}"
                    ))
                })?;
            let right_value = right_by_id
                .get(id)
                .map(|entry| serde_json::to_value(entry))
                .transpose()
                .map_err(|error| {
                    EffectiveDefinitionDigestError(format!(
                        "serialize external realization identity: {error}"
                    ))
                })?;
            self.diff_json(
                DefinitionChangeCategory::ExternalRealization,
                format!("derived.external_realization.slot[{slot:04}]"),
                left_value.as_ref(),
                right_value.as_ref(),
                JsonDisclosure::ValidatedExternalRealization,
                false,
            );
        }
        Ok(())
    }

    fn diff_source_closure(
        &mut self,
        left: Option<&serde_json::Value>,
        right: Option<&serde_json::Value>,
    ) -> Result<(), EffectiveDefinitionDigestError> {
        let parse = |value: Option<&serde_json::Value>| {
            value
                .map(ryeos_state::objects::EffectiveSourceClosureProjection::from_value)
                .transpose()
                .map_err(|error| {
                    EffectiveDefinitionDigestError(format!(
                        "invalid effective source closure identity slot: {error}"
                    ))
                })
        };
        let left = parse(left)?
            .map(|projection| serde_json::to_value(projection).expect("projection serializes"));
        let right = parse(right)?
            .map(|projection| serde_json::to_value(projection).expect("projection serializes"));
        self.diff_json(
            DefinitionChangeCategory::SourceClosure,
            "derived.source_closure".to_owned(),
            left.as_ref(),
            right.as_ref(),
            JsonDisclosure::TypeOnly,
            false,
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn diff_json(
        &mut self,
        category: DefinitionChangeCategory,
        coordinate: String,
        left: Option<&serde_json::Value>,
        right: Option<&serde_json::Value>,
        disclosure: JsonDisclosure,
        public_manifest_hash: bool,
    ) {
        if !self.visit() {
            return;
        }
        match (left, right) {
            (None, None) => {}
            (None, Some(right)) => {
                let right = self.json_summary(right, disclosure, public_manifest_hash);
                self.push(
                    category,
                    coordinate,
                    DefinitionChangeKind::Added,
                    None,
                    Some(right),
                );
            }
            (Some(left), None) => {
                let left = self.json_summary(left, disclosure, public_manifest_hash);
                self.push(
                    category,
                    coordinate,
                    DefinitionChangeKind::Removed,
                    Some(left),
                    None,
                );
            }
            (Some(left), Some(right)) => match (left, right) {
                (serde_json::Value::Object(left), serde_json::Value::Object(right)) => {
                    let keys = left
                        .keys()
                        .chain(right.keys())
                        .cloned()
                        .collect::<BTreeSet<_>>();
                    let mut opaque_slot = 0usize;
                    for key in keys {
                        if self.halted {
                            break;
                        }
                        let is_public_manifest_hash =
                            matches!(disclosure, JsonDisclosure::ValidatedExternalRealization)
                                && key == "manifest_hash";
                        let child_coordinate = if is_public_manifest_hash {
                            format!("{coordinate}.manifest_hash")
                        } else {
                            let child = format!("{coordinate}.slot[{opaque_slot:04}]");
                            opaque_slot += 1;
                            child
                        };
                        self.diff_json(
                            category,
                            child_coordinate,
                            left.get(&key),
                            right.get(&key),
                            disclosure,
                            is_public_manifest_hash,
                        );
                    }
                }
                (serde_json::Value::Array(left), serde_json::Value::Array(right)) => {
                    for index in 0..left.len().max(right.len()) {
                        if self.halted {
                            break;
                        }
                        self.diff_json(
                            category,
                            format!("{coordinate}[{index}]"),
                            left.get(index),
                            right.get(index),
                            disclosure,
                            false,
                        );
                    }
                }
                _ if left == right => {}
                _ => {
                    let left_summary = self.json_summary(left, disclosure, public_manifest_hash);
                    let right_summary = self.json_summary(right, disclosure, public_manifest_hash);
                    self.push(
                        category,
                        coordinate,
                        DefinitionChangeKind::Changed,
                        Some(left_summary),
                        Some(right_summary),
                    );
                }
            },
        }
    }

    fn json_summary(
        &mut self,
        value: &serde_json::Value,
        disclosure: JsonDisclosure,
        public_manifest_hash: bool,
    ) -> DefinitionValueSummary {
        let value_type = match value {
            serde_json::Value::Null => DefinitionValueType::Null,
            serde_json::Value::Bool(_) => DefinitionValueType::Boolean,
            serde_json::Value::Number(_) => DefinitionValueType::Number,
            serde_json::Value::String(_) => DefinitionValueType::String,
            serde_json::Value::Array(_) => DefinitionValueType::Array,
            serde_json::Value::Object(_) => DefinitionValueType::Object,
        };
        let public_scalar = if matches!(disclosure, JsonDisclosure::ValidatedExternalRealization)
            && public_manifest_hash
        {
            value
                .as_str()
                .filter(|value| is_lower_sha256(value))
                .and_then(|value| {
                    if value.len() <= self.limits.public_scalar_bytes {
                        Some(value.to_string())
                    } else {
                        self.complete = false;
                        None
                    }
                })
        } else {
            None
        };
        DefinitionValueSummary {
            value_type,
            public_scalar,
        }
    }
}

fn group_contributors<'a>(
    contributors: &'a [DefinitionContributor],
) -> BTreeMap<(String, String), Vec<&'a DefinitionContributor>> {
    let mut grouped = BTreeMap::new();
    for contributor in contributors {
        grouped
            .entry((
                contributor.canonical_ref.clone(),
                resolution_step_name(contributor.added_by).to_string(),
            ))
            .or_insert_with(Vec::new)
            .push(contributor);
    }
    grouped
}

fn group_edges<'a>(
    edges: &'a [DefinitionReferenceEdge],
) -> BTreeMap<(String, String, String), Vec<&'a DefinitionReferenceEdge>> {
    let mut grouped = BTreeMap::new();
    for edge in edges {
        grouped
            .entry((
                edge.from_ref.clone(),
                edge.to_ref.clone(),
                resolution_step_name(edge.added_by).to_string(),
            ))
            .or_insert_with(Vec::new)
            .push(edge);
    }
    grouped
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
struct DefinitionContributor {
    canonical_ref: String,
    root_raw_content_digest: String,
    source_space: ItemSpace,
    source_root: ItemSourceRoot,
    trust_class: TrustClass,
    signer_fingerprint: Option<String>,
    added_by: ResolutionStepName,
}

impl DefinitionContributor {
    fn sort_key(&self) -> (&str, &str, &str, &str, &str, &str, &str, &str) {
        let (source_root_kind, source_root_name) = source_root_sort_key(&self.source_root);
        (
            &self.canonical_ref,
            &self.root_raw_content_digest,
            self.source_space.as_str(),
            source_root_kind,
            source_root_name,
            trust_class_name(self.trust_class),
            self.signer_fingerprint.as_deref().unwrap_or(""),
            resolution_step_name(self.added_by),
        )
    }
}

impl TryFrom<&ResolvedAncestor> for DefinitionContributor {
    type Error = EffectiveDefinitionDigestError;

    fn try_from(value: &ResolvedAncestor) -> Result<Self, Self::Error> {
        require_canonical_ref("canonical ref", &value.resolved_ref)?;
        require_lower_sha256("root raw content digest", &value.raw_content_digest)?;
        if matches!(
            value.trust_class,
            TrustClass::TrustedBundle | TrustClass::TrustedProject | TrustClass::TrustedNode
        ) {
            let signer = value.signer_fingerprint.as_deref().ok_or_else(|| {
                EffectiveDefinitionDigestError(format!(
                    "trusted contributor `{}` has no signer fingerprint",
                    value.resolved_ref
                ))
            })?;
            require_lower_sha256("signer fingerprint", signer)?;
        } else if let Some(signer) = value.signer_fingerprint.as_deref() {
            require_lower_sha256("signer fingerprint", signer)?;
        }
        Ok(Self {
            canonical_ref: value.resolved_ref.clone(),
            root_raw_content_digest: value.raw_content_digest.clone(),
            source_space: value.source_space,
            source_root: value.source_root.clone(),
            trust_class: value.trust_class,
            signer_fingerprint: value.signer_fingerprint.clone(),
            added_by: value.added_by,
        })
    }
}

fn source_root_sort_key(value: &ItemSourceRoot) -> (&'static str, &str) {
    match value {
        ItemSourceRoot::Project => ("project", ""),
        ItemSourceRoot::Node => ("node", ""),
        ItemSourceRoot::Bundle { name } => ("bundle", name),
        ItemSourceRoot::Search { label } => ("search", label),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
struct DefinitionReferenceEdge {
    from_ref: String,
    to_ref: String,
    to_source_space: ItemSpace,
    trust_class: TrustClass,
    added_by: ResolutionStepName,
}

impl DefinitionReferenceEdge {
    fn sort_key(&self) -> (&str, &str, &str, &str, &str) {
        (
            &self.from_ref,
            &self.to_ref,
            self.to_source_space.as_str(),
            trust_class_name(self.trust_class),
            resolution_step_name(self.added_by),
        )
    }
}

impl TryFrom<&ResolutionEdge> for DefinitionReferenceEdge {
    type Error = EffectiveDefinitionDigestError;

    fn try_from(value: &ResolutionEdge) -> Result<Self, Self::Error> {
        require_canonical_ref("reference edge source", &value.from_ref)?;
        require_canonical_ref("reference edge target", &value.to_ref)?;
        Ok(Self {
            from_ref: value.from_ref.clone(),
            to_ref: value.to_ref.clone(),
            to_source_space: value.to_source_space,
            trust_class: value.trust_class,
            added_by: value.added_by,
        })
    }
}

fn require_canonical_ref(label: &str, value: &str) -> Result<(), EffectiveDefinitionDigestError> {
    crate::canonical_ref::CanonicalRef::parse(value)
        .map(|_| ())
        .map_err(|error| {
            EffectiveDefinitionDigestError(format!(
                "effective-definition document {label} `{value}` is not canonical: {error}"
            ))
        })
}

fn require_lower_sha256(label: &str, value: &str) -> Result<(), EffectiveDefinitionDigestError> {
    EffectiveDefinitionDigest::parse(value.to_string())
        .map(|_| ())
        .map_err(|_| EffectiveDefinitionDigestError(format!("invalid {label}: `{value}`")))
}

fn trust_class_name(value: TrustClass) -> &'static str {
    match value {
        TrustClass::TrustedBundle => "trusted_bundle",
        TrustClass::TrustedProject => "trusted_project",
        TrustClass::TrustedNode => "trusted_node",
        TrustClass::UntrustedProject => "untrusted_project",
        TrustClass::Unsigned => "unsigned",
    }
}

fn resolution_step_name(value: ResolutionStepName) -> &'static str {
    match value {
        ResolutionStepName::PipelineInit => "pipeline_init",
        ResolutionStepName::ResolveExtendsChain => "resolve_extends_chain",
        ResolutionStepName::ResolveReferences => "resolve_references",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn contributor(canonical_ref: &str, digest: char) -> DefinitionContributor {
        DefinitionContributor {
            canonical_ref: canonical_ref.to_string(),
            root_raw_content_digest: digest.to_string().repeat(64),
            source_space: ItemSpace::Bundle,
            source_root: ItemSourceRoot::Bundle {
                name: "standard".to_string(),
            },
            trust_class: TrustClass::TrustedBundle,
            signer_fingerprint: Some("f".repeat(64)),
            added_by: ResolutionStepName::ResolveReferences,
        }
    }

    fn document() -> DefinitionIdentityDocument {
        let mut root = contributor("graph:test/root", 'a');
        root.added_by = ResolutionStepName::PipelineInit;
        DefinitionIdentityDocument {
            schema: "ryeos.effective_definition.v2",
            root,
            ancestors: Vec::new(),
            referenced_items: Vec::new(),
            reference_edges: Vec::new(),
            effective_trust_class: TrustClass::TrustedBundle,
            composed: KindComposedView {
                composed: serde_json::json!({"config": {"start": "run"}}),
                derived: HashMap::new(),
                policy_facts: HashMap::new(),
            },
        }
    }

    #[test]
    fn equal_documents_have_one_complete_empty_diff() {
        let left = document();
        let diff = left.diff(&left).unwrap();
        assert!(diff.complete);
        assert!(diff.changes.is_empty());
        assert_eq!(diff.omitted_changes, Some(0));
        assert_eq!(diff.left_digest, diff.right_digest);
    }

    #[test]
    fn root_digest_change_discloses_only_the_public_identity() {
        let left = document();
        let mut right = document();
        right.root.root_raw_content_digest = "b".repeat(64);

        let diff = left.diff(&right).unwrap();
        assert!(diff.complete);
        assert_eq!(diff.changes.len(), 1);
        let change = &diff.changes[0];
        assert_eq!(change.category, DefinitionChangeCategory::Root);
        assert_eq!(change.coordinate, "root.root_raw_content_digest");
        assert_eq!(change.change, DefinitionChangeKind::Changed);
        assert_eq!(
            change.left.as_ref().unwrap().public_scalar.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            change.right.as_ref().unwrap().public_scalar.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn arbitrary_policy_hook_and_source_values_never_escape_or_get_hashed() {
        let secret_left = "operator-secret-left";
        let secret_right = "operator-secret-right";
        let mut left = document();
        let mut right = document();
        left.root.source_root = ItemSourceRoot::Bundle {
            name: secret_left.to_string(),
        };
        right.root.source_root = ItemSourceRoot::Bundle {
            name: secret_right.to_string(),
        };
        left.composed.derived.insert(
            crate::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY.to_string(),
            serde_json::json!({"hooks": [{"id": secret_left}]}),
        );
        right.composed.derived.insert(
            crate::hooks::EFFECTIVE_HOOK_PLAN_DERIVED_KEY.to_string(),
            serde_json::json!({"hooks": [{"id": secret_right}]}),
        );
        left.composed.policy_facts.insert(
            "effective_caps".to_string(),
            serde_json::json!([secret_left]),
        );
        right.composed.policy_facts.insert(
            "effective_caps".to_string(),
            serde_json::json!([secret_right]),
        );

        let diff = left.diff(&right).unwrap();
        let encoded = serde_json::to_string(&diff).unwrap();
        let left_hash = lillux::sha256_hex(secret_left.as_bytes());
        let right_hash = lillux::sha256_hex(secret_right.as_bytes());
        for forbidden in [
            secret_left,
            secret_right,
            left_hash.as_str(),
            right_hash.as_str(),
            "effective_caps",
        ] {
            assert!(!encoded.contains(forbidden), "leaked {forbidden}");
        }
        assert!(
            diff.changes
                .iter()
                .any(|change| change.category == DefinitionChangeCategory::HookPlan)
        );
        assert!(
            diff.changes
                .iter()
                .any(|change| change.category == DefinitionChangeCategory::Policy)
        );
    }

    #[test]
    fn referenced_item_alignment_does_not_rewrite_the_suffix() {
        let mut left = document();
        left.referenced_items = vec![
            contributor("tool:test/a", 'a'),
            contributor("tool:test/c", 'c'),
        ];
        let mut right = left.clone();
        right
            .referenced_items
            .insert(1, contributor("tool:test/b", 'b'));

        let diff = left.diff(&right).unwrap();
        let referenced = diff
            .changes
            .iter()
            .filter(|change| change.category == DefinitionChangeCategory::ReferencedItem)
            .collect::<Vec<_>>();
        assert_eq!(referenced.len(), 1);
        assert_eq!(referenced[0].change, DefinitionChangeKind::Added);
        assert!(referenced[0].coordinate.contains("tool:test/b"));
        assert!(!referenced[0].coordinate.contains("tool:test/c"));
    }

    #[test]
    fn external_realization_manifest_hashes_are_validated_public_identities() {
        let mut left = document();
        let mut right = document();
        let realization = |manifest: char| {
            serde_json::json!([{
                "id": "sim",
                "kind": "tree",
                "mode": "captured",
                "manifest_hash": manifest.to_string().repeat(64),
                "entry_count": 1,
                "total_bytes": 1,
                "mount": "vendor/sim"
            }])
        };
        left.composed.derived.insert(
            EXTERNAL_REALIZATIONS_DERIVED_KEY.to_string(),
            realization('a'),
        );
        right.composed.derived.insert(
            EXTERNAL_REALIZATIONS_DERIVED_KEY.to_string(),
            realization('b'),
        );

        let diff = left.diff(&right).unwrap();
        let change = diff
            .changes
            .iter()
            .find(|change| {
                change.category == DefinitionChangeCategory::ExternalRealization
                    && change.coordinate.ends_with(".manifest_hash")
            })
            .unwrap();
        assert_eq!(
            change.left.as_ref().unwrap().public_scalar.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            change.right.as_ref().unwrap().public_scalar.as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn source_closure_changes_have_a_typed_category_and_strict_shape() {
        let projection = |binding: char| {
            serde_json::json!({
                "schema": 1,
                "binding_hash": binding.to_string().repeat(64),
                "content_manifest_hash": "b".repeat(64),
                "owner_key": "c".repeat(64),
                "file_count": 2,
                "total_bytes": 10
            })
        };
        let mut left = document();
        let mut right = document();
        left.composed.derived.insert(
            ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY.to_owned(),
            projection('a'),
        );
        right.composed.derived.insert(
            ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY.to_owned(),
            projection('d'),
        );
        let diff = left.diff(&right).unwrap();
        assert!(
            diff.changes
                .iter()
                .any(|change| change.category == DefinitionChangeCategory::SourceClosure)
        );

        right.composed.derived.insert(
            ryeos_state::objects::SOURCE_CLOSURE_DERIVED_KEY.to_owned(),
            serde_json::json!({"binding_hash": "not-current"}),
        );
        assert!(right.diff(&left).is_err());
    }

    #[test]
    fn external_realization_alignment_does_not_rewrite_the_suffix() {
        let mut left = document();
        let mut right = document();
        let realization = |id: &str, manifest: char| {
            serde_json::json!({
                "id": id,
                "kind": "tree",
                "mode": "captured",
                "manifest_hash": manifest.to_string().repeat(64),
                "entry_count": 1,
                "total_bytes": 1,
                "mount": format!("vendor/{id}")
            })
        };
        left.composed.derived.insert(
            EXTERNAL_REALIZATIONS_DERIVED_KEY.to_string(),
            serde_json::json!([realization("a", 'a'), realization("c", 'c')]),
        );
        right.composed.derived.insert(
            EXTERNAL_REALIZATIONS_DERIVED_KEY.to_string(),
            serde_json::json!([
                realization("a", 'a'),
                realization("b", 'b'),
                realization("c", 'c')
            ]),
        );

        let diff = left.diff(&right).unwrap();
        let realization_changes = diff
            .changes
            .iter()
            .filter(|change| change.category == DefinitionChangeCategory::ExternalRealization)
            .collect::<Vec<_>>();
        assert_eq!(realization_changes.len(), 1);
        assert_eq!(realization_changes[0].change, DefinitionChangeKind::Added);
    }

    #[test]
    fn row_and_visit_limits_never_claim_completeness() {
        let mut left = document();
        let mut right = document();
        left.composed.composed = serde_json::json!({"a": 1, "b": 2, "c": 3});
        right.composed.composed = serde_json::json!({"a": 4, "b": 5, "c": 6});

        let row_limited = left
            .diff_with_limits(
                &right,
                DefinitionDiffLimits {
                    rows: 1,
                    ..DefinitionDiffLimits::default()
                },
            )
            .unwrap();
        assert!(!row_limited.complete);
        assert_eq!(row_limited.changes.len(), 1);
        assert_eq!(row_limited.omitted_changes, None);

        let visit_limited = left
            .diff_with_limits(
                &right,
                DefinitionDiffLimits {
                    visits: 1,
                    ..DefinitionDiffLimits::default()
                },
            )
            .unwrap();
        assert!(!visit_limited.complete);
        assert_eq!(visit_limited.omitted_changes, None);
    }

    #[test]
    fn swapping_operands_reverses_public_before_and_after() {
        let left = document();
        let mut right = document();
        right.root.root_raw_content_digest = "b".repeat(64);
        let forward = left.diff(&right).unwrap();
        let reverse = right.diff(&left).unwrap();

        assert_eq!(forward.changes.len(), reverse.changes.len());
        assert_eq!(forward.changes[0].left, reverse.changes[0].right);
        assert_eq!(forward.changes[0].right, reverse.changes[0].left);
    }
}
