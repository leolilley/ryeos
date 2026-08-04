//! Renderer-neutral living-field facts, projection, and semantic view model.
//!
//! The daemon emits bounded substrate facts. Signed `view:` content assigns
//! their visual meaning through a closed projection grammar. Nothing in this
//! module knows a project noun or a physical coordinate.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::ui::content::ViewBinding;
use crate::ui::event::RyeOsUiIntent;
use crate::ui::view_model::{RyeOsRowDetailVm, RyeOsTone};
use crate::workspace::{FieldFingerprintState, FieldLocalState};

pub const FIELD_FACTS_SCHEMA: &str = "ryeos.ui.field.facts.v1";
pub const FIELD_PROJECTION_SCHEMA: &str = "ryeos.ui.field.projection.v1";
pub const FIELD_VM_SCHEMA: &str = "ryeos.ui.field.vm.v1";
pub const MAX_FIELD_FACT_ENTITIES: usize = 5_000;
pub const MAX_FIELD_FACT_RELATIONS: usize = 12_000;
pub const MAX_FIELD_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_FIELD_ATTRIBUTE_BYTES: usize = 256 * 1024;
pub const MAX_FIELD_PREVIEWS: usize = 256;
pub const MAX_FIELD_METRICS: usize = 1_024;
pub const MAX_FIELD_EXPANSIONS: usize = 16;
pub const MAX_FIELD_WARNINGS: usize = 256;
pub const MAX_FIELD_PREVIEW_BYTES: usize = 1024 * 1024;
pub const MAX_GRID_AXIS: u32 = 512;
pub const MAX_GRID_CELLS: usize = 262_144;
pub const MAX_GRID_PALETTE: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldFactSubject {
    pub kind: String,
    pub id: String,
    #[serde(default)]
    pub definition_ref: Option<String>,
    #[serde(default)]
    pub definition_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldEventRef {
    pub chain_root_id: String,
    pub chain_seq: u64,
    pub event_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum FieldCursor {
    Live,
    BraidCut {
        anchor: FieldEventRef,
        through_chain_seq: u64,
        outside_cut: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldReplay {
    pub capability: String,
    #[serde(default)]
    pub previous: Option<FieldEventRef>,
    #[serde(default)]
    pub next: Option<FieldEventRef>,
    #[serde(default)]
    pub live_head: Option<FieldEventRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldProvenance {
    pub source_ref: String,
    pub source_revision: String,
    #[serde(default)]
    pub evidence: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldFactEntity {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub canonical_ref: Option<String>,
    #[serde(default)]
    pub source_content_hash: Option<String>,
    #[serde(default)]
    pub definition_hash: Option<String>,
    #[serde(default)]
    pub admitted_capsule_hash: Option<String>,
    #[serde(default)]
    pub event_ref: Option<FieldEventRef>,
    #[serde(default)]
    pub artifact_ref: Option<Value>,
    pub attributes: Value,
    pub provenance: FieldProvenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldFactRelation {
    pub id: String,
    pub kind: String,
    pub source_id: String,
    pub target_id: String,
    #[serde(default)]
    pub status: Option<String>,
    pub directed: bool,
    pub attributes: Value,
    pub provenance: FieldProvenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldFactsDocument {
    pub schema_version: String,
    pub source: String,
    pub subject: FieldFactSubject,
    pub revision: String,
    pub cursor: FieldCursor,
    #[serde(default)]
    pub replay: Option<FieldReplay>,
    pub truncated: bool,
    pub entities: Vec<FieldFactEntity>,
    pub relations: Vec<FieldFactRelation>,
    #[serde(default)]
    pub previews: Vec<Value>,
    #[serde(default)]
    pub metrics: Vec<Value>,
    #[serde(default)]
    pub expansions: Vec<Value>,
    #[serde(default)]
    pub warnings: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldExpansionResult {
    pub root_id: String,
    pub applied_depth: u16,
    pub entity_count: u32,
    pub truncated: bool,
    #[serde(default)]
    pub continuation_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FieldShape {
    Dot,
    Disc,
    Ring,
    #[default]
    Rect,
    Capsule,
    Diamond,
    Hex,
    Anchor,
    Aggregate,
    Grid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FieldFill {
    #[default]
    Solid,
    Hollow,
    Ghost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FieldStroke {
    #[default]
    Solid,
    Dashed,
    Dotted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FieldEmphasis {
    Quiet,
    #[default]
    Normal,
    Strong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FieldMotion {
    #[default]
    None,
    Pulse,
    Flow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FieldLayout {
    #[default]
    Flow,
    Lanes,
    Stack,
    Grid,
    Overlay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RyeOsFieldEntityTraitsVm {
    pub shape: FieldShape,
    pub fill: FieldFill,
    pub stroke: FieldStroke,
    pub emphasis: FieldEmphasis,
    pub motion: FieldMotion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RyeOsFieldBadgeVm {
    pub label: String,
    pub tone: RyeOsTone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFieldEntityVm {
    pub id: String,
    pub source: String,
    pub kind: String,
    pub role: Option<String>,
    pub label: String,
    pub secondary: Option<String>,
    pub parent_id: Option<String>,
    pub group_id: Option<String>,
    pub layer_ids: Vec<String>,
    pub lane: Option<String>,
    pub rank: Option<i64>,
    pub order: Option<i64>,
    pub status: Option<String>,
    pub tone: RyeOsTone,
    pub traits: RyeOsFieldEntityTraitsVm,
    pub badges: Vec<RyeOsFieldBadgeVm>,
    pub preview_ids: Vec<String>,
    pub selected: bool,
    pub selectable: bool,
    pub select_intent: Option<RyeOsUiIntent>,
    pub activate_intent: Option<RyeOsUiIntent>,
    pub accessibility_label: String,
    pub detail: Vec<RyeOsRowDetailVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFieldRelationVm {
    pub id: String,
    pub kind: String,
    pub source_id: String,
    pub target_id: String,
    pub directed: bool,
    pub tone: RyeOsTone,
    pub stroke: FieldStroke,
    pub emphasis: FieldEmphasis,
    pub motion: FieldMotion,
    pub label: Option<String>,
    pub layer_ids: Vec<String>,
    pub selected: bool,
    pub activate_intent: Option<RyeOsUiIntent>,
    pub accessibility_label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFieldGroupVm {
    pub id: String,
    pub label: String,
    pub parent_id: Option<String>,
    pub layout: FieldLayout,
    pub collapsed: bool,
    pub aggregate: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFieldLayerVm {
    pub id: String,
    pub label: String,
    pub visible: bool,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFieldSubjectVm {
    pub source: String,
    pub kind: String,
    pub id: String,
    pub definition_ref: Option<String>,
    pub definition_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsSourcePhase {
    Loading,
    Ready,
    Refreshing,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFieldSourceStatusVm {
    pub name: String,
    pub source_ref: String,
    pub subject_fingerprint: Option<String>,
    pub revision: Option<String>,
    pub phase: RyeOsSourcePhase,
    pub truncated: bool,
    pub error: Option<String>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RyeOsFieldReplayEntryVm {
    pub event: FieldEventRef,
    pub label: String,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RyeOsFieldReplayVm {
    pub mode: String,
    pub playing: bool,
    pub anchor: Option<FieldEventRef>,
    pub previous: Option<FieldEventRef>,
    pub next: Option<FieldEventRef>,
    pub live_head: Option<FieldEventRef>,
    pub rail: Vec<RyeOsFieldReplayEntryVm>,
    pub outside_cut: Vec<String>,
}

impl Default for RyeOsFieldReplayVm {
    fn default() -> Self {
        Self {
            mode: "live".to_string(),
            playing: false,
            anchor: None,
            previous: None,
            next: None,
            live_head: None,
            rail: Vec::new(),
            outside_cut: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RyeOsFieldSearchVm {
    pub query: String,
    pub match_ids: Vec<String>,
    pub active_match: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFieldExpansionVm {
    pub source: String,
    pub root_id: String,
    pub phase: RyeOsSourcePhase,
    pub applied_depth: u16,
    pub entity_count: u32,
    pub truncated: bool,
    pub can_continue: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsFieldChangeKind {
    Entered,
    Exited,
    Updated,
    StatusChanged,
    RelationAdded,
    RelationRemoved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RyeOsFieldChangeVm {
    pub id: String,
    pub kind: RyeOsFieldChangeKind,
    pub at_ms: u64,
    pub tone: Option<RyeOsTone>,
    pub prior_fingerprint: Option<String>,
    pub fingerprint: Option<String>,
    pub tombstone: Option<RyeOsFieldTombstoneVm>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RyeOsFieldTombstoneVm {
    pub label: String,
    pub traits: RyeOsFieldEntityTraitsVm,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RyeOsGridPaletteEntryVm {
    pub index: u16,
    pub color: String,
    pub glyph: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RyeOsIndexedGridVm {
    pub width: u32,
    pub height: u32,
    pub cells: Vec<u16>,
    pub palette: Vec<RyeOsGridPaletteEntryVm>,
    pub changed: Vec<u32>,
    pub labels: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsArtifactPreviewVm {
    pub id: String,
    pub entity_id: Option<String>,
    pub kind: String,
    pub label: String,
    pub comparison_key: Option<String>,
    pub grid: Option<RyeOsIndexedGridVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsMetricVm {
    pub id: String,
    pub label: String,
    pub value: Value,
    pub unit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsFieldVm {
    pub schema_version: String,
    pub id: String,
    pub title: String,
    pub revision: String,
    pub structural_revision: String,
    pub data_revision: String,
    pub local_revision: String,
    pub sources: Vec<RyeOsFieldSourceStatusVm>,
    pub subjects: Vec<RyeOsFieldSubjectVm>,
    pub groups: Vec<RyeOsFieldGroupVm>,
    pub layers: Vec<RyeOsFieldLayerVm>,
    pub entities: Vec<RyeOsFieldEntityVm>,
    pub relations: Vec<RyeOsFieldRelationVm>,
    pub previews: Vec<RyeOsArtifactPreviewVm>,
    pub metrics: Vec<RyeOsMetricVm>,
    pub traversal: Vec<String>,
    pub selected: Option<String>,
    pub compare: Vec<String>,
    pub cursor: Option<FieldEventRef>,
    pub replay: RyeOsFieldReplayVm,
    pub search: RyeOsFieldSearchVm,
    pub expansions: Vec<RyeOsFieldExpansionVm>,
    pub changes: Vec<RyeOsFieldChangeVm>,
    pub warnings: Vec<String>,
    pub provenance: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FieldSourceInput<'a> {
    pub channel: &'a str,
    pub source_ref: &'a str,
    pub subject_fingerprint: Option<&'a str>,
    pub response: Option<&'a Value>,
    pub parsed: Option<&'a Result<FieldFactsDocument, String>>,
    pub error: Option<&'a str>,
    pub refreshing: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct FieldProjectionCacheEntry {
    pub fingerprint: String,
    pub base: RyeOsFieldVm,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldProjection {
    schema_version: String,
    #[serde(default)]
    groups: Vec<FieldGroupProjection>,
    #[serde(default)]
    layers: Vec<FieldLayerProjection>,
    #[serde(default)]
    entity_rules: Vec<FieldRule>,
    #[serde(default)]
    relation_rules: Vec<FieldRule>,
    #[serde(default)]
    derived_relations: Vec<FieldDerivedRelation>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldGroupProjection {
    id: String,
    label: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    layout: Option<String>,
    #[serde(default)]
    aggregate: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldLayerProjection {
    id: String,
    label: String,
    #[serde(default = "default_true")]
    default_visible: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldRule {
    #[serde(rename = "match")]
    matches: BTreeMap<String, Value>,
    #[serde(default)]
    set: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldJoinSide {
    #[serde(rename = "match")]
    matches: BTreeMap<String, Value>,
    keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldDerivedRelation {
    id: String,
    left: FieldJoinSide,
    right: FieldJoinSide,
    relation: BTreeMap<String, Value>,
}

fn default_true() -> bool {
    true
}

#[derive(Clone)]
struct MergedEntity {
    source: String,
    owner: String,
    raw: FieldFactEntity,
}

#[derive(Clone)]
struct MergedRelation {
    source: String,
    owner: String,
    raw: FieldFactRelation,
}

/// Strictly parse and recheck a daemon field document. The client repeats the
/// bounds because signed view content and transport responses are independent
/// trust boundaries.
pub fn parse_field_document(value: &Value) -> Result<FieldFactsDocument, String> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_FIELD_DOCUMENT_BYTES {
        return Err(format!(
            "field document is {} bytes (max {MAX_FIELD_DOCUMENT_BYTES})",
            bytes.len()
        ));
    }
    validate_json_shape(value, 0, &mut 0)?;
    let document: FieldFactsDocument = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid field facts: {error}"))?;
    if document.schema_version != FIELD_FACTS_SCHEMA {
        return Err(format!(
            "unsupported field facts schema '{}'",
            document.schema_version
        ));
    }
    if document.entities.len() > MAX_FIELD_FACT_ENTITIES
        || document.relations.len() > MAX_FIELD_FACT_RELATIONS
        || document.previews.len() > MAX_FIELD_PREVIEWS
        || document.metrics.len() > MAX_FIELD_METRICS
        || document.expansions.len() > MAX_FIELD_EXPANSIONS
        || document.warnings.len() > MAX_FIELD_WARNINGS
    {
        return Err("field facts exceed client count limits".to_string());
    }
    let mut entity_ids = BTreeSet::new();
    for entity in &document.entities {
        validate_id(&entity.id)?;
        if !entity_ids.insert(&entity.id) {
            return Err(format!("duplicate field entity '{}'", entity.id));
        }
        validate_attributes(&entity.attributes)?;
    }
    let mut relation_ids = BTreeSet::new();
    for relation in &document.relations {
        validate_id(&relation.id)?;
        if !relation_ids.insert(&relation.id) {
            return Err(format!("duplicate field relation '{}'", relation.id));
        }
        validate_attributes(&relation.attributes)?;
    }
    let mut preview_ids = BTreeSet::new();
    for preview in &document.previews {
        if serde_json::to_vec(preview)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_FIELD_PREVIEW_BYTES
        {
            return Err("field preview exceeds the inline byte limit".to_string());
        }
        let projected = project_preview(preview)?;
        if !preview_ids.insert(projected.id) {
            return Err("field document contains a duplicate preview ID".to_string());
        }
    }
    let mut expansion_roots = BTreeSet::new();
    for expansion in &document.expansions {
        let expansion: FieldExpansionResult = serde_json::from_value(expansion.clone())
            .map_err(|error| format!("invalid field expansion result: {error}"))?;
        validate_id(&expansion.root_id)?;
        if expansion.applied_depth > 32
            || expansion.entity_count as usize > MAX_FIELD_FACT_ENTITIES
            || expansion
                .continuation_token
                .as_ref()
                .is_some_and(|token| token.len() > 8 * 1024)
        {
            return Err("field expansion result exceeds client limits".to_string());
        }
        if !expansion_roots.insert(expansion.root_id) {
            return Err("field document contains a duplicate expansion root".to_string());
        }
    }
    Ok(document)
}

fn validate_json_shape(value: &Value, depth: usize, count: &mut usize) -> Result<(), String> {
    if depth > 32 {
        return Err("field facts exceed JSON depth limit".to_string());
    }
    *count += 1;
    if *count > 100_000 {
        return Err("field facts exceed JSON value limit".to_string());
    }
    match value {
        Value::Array(values) => {
            for value in values {
                validate_json_shape(value, depth + 1, count)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_json_shape(value, depth + 1, count)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 1024 || id.trim() != id || id.chars().any(char::is_control) {
        return Err("field fact contains invalid stable ID".to_string());
    }
    Ok(())
}

fn validate_attributes(value: &Value) -> Result<(), String> {
    if !value.is_object() {
        return Err("field attributes must be an object".to_string());
    }
    let bytes = serde_json::to_vec(value)
        .map_err(|error| error.to_string())?
        .len();
    if bytes > MAX_FIELD_ATTRIBUTE_BYTES {
        return Err(format!(
            "field attributes are {bytes} bytes (max {MAX_FIELD_ATTRIBUTE_BYTES})"
        ));
    }
    Ok(())
}

/// Merge every accepted named source and project it into one semantic VM.
/// Input order cannot affect output: documents, facts, and rules are folded in
/// authored channel / stable-ID order.
pub fn project_field(
    field_id: &str,
    title: &str,
    view_ref: &str,
    binding: &ViewBinding,
    source_inputs: &[FieldSourceInput<'_>],
    local: Option<&FieldLocalState>,
) -> RyeOsFieldVm {
    let local = local.cloned().unwrap_or_default();
    let projection = match serde_json::from_value::<FieldProjection>(binding.projections.clone()) {
        Ok(projection) if projection.schema_version == FIELD_PROJECTION_SCHEMA => Some(projection),
        Ok(projection) => {
            return placeholder_vm(
                field_id,
                title,
                format!(
                    "unsupported field projection schema '{}'",
                    projection.schema_version
                ),
            );
        }
        Err(error) => {
            return placeholder_vm(
                field_id,
                title,
                format!("invalid field projection: {error}"),
            );
        }
    };
    let projection = projection.expect("checked above");

    let mut ordered_inputs = source_inputs.to_vec();
    ordered_inputs.sort_by_key(|input| input.channel);
    let mut documents = Vec::new();
    let mut source_status = Vec::new();
    let mut warnings = Vec::new();
    for input in ordered_inputs {
        let parsed = input
            .parsed
            .cloned()
            .or_else(|| input.response.map(parse_field_document));
        match parsed {
            Some(Ok(document)) => {
                source_status.push(RyeOsFieldSourceStatusVm {
                    name: input.channel.to_string(),
                    source_ref: input.source_ref.to_string(),
                    subject_fingerprint: input.subject_fingerprint.map(str::to_string),
                    revision: Some(document.revision.clone()),
                    phase: if input.refreshing {
                        RyeOsSourcePhase::Refreshing
                    } else {
                        RyeOsSourcePhase::Ready
                    },
                    truncated: document.truncated,
                    error: input.error.map(str::to_string),
                    evidence: document
                        .entities
                        .iter()
                        .flat_map(|entity| entity.provenance.evidence.iter())
                        .take(32)
                        .map(compact_value)
                        .collect(),
                });
                documents.push((input.channel.to_string(), document));
            }
            Some(Err(error)) => {
                warnings.push(format!("source {}: {error}", input.channel));
                source_status.push(RyeOsFieldSourceStatusVm {
                    name: input.channel.to_string(),
                    source_ref: input.source_ref.to_string(),
                    subject_fingerprint: input.subject_fingerprint.map(str::to_string),
                    revision: None,
                    phase: RyeOsSourcePhase::Error,
                    truncated: false,
                    error: Some(error),
                    evidence: Vec::new(),
                });
            }
            None => source_status.push(RyeOsFieldSourceStatusVm {
                name: input.channel.to_string(),
                source_ref: input.source_ref.to_string(),
                subject_fingerprint: input.subject_fingerprint.map(str::to_string),
                revision: None,
                phase: if input.error.is_some() {
                    RyeOsSourcePhase::Error
                } else {
                    RyeOsSourcePhase::Loading
                },
                truncated: false,
                error: input.error.map(str::to_string),
                evidence: Vec::new(),
            }),
        }
    }

    let mut entity_candidates = BTreeMap::<String, Vec<MergedEntity>>::new();
    let mut relation_candidates = BTreeMap::<String, Vec<MergedRelation>>::new();
    let mut preview_candidates = BTreeMap::<String, Vec<(String, RyeOsArtifactPreviewVm)>>::new();
    let mut metric_candidates = BTreeMap::<String, Vec<(String, RyeOsMetricVm)>>::new();
    let mut subjects = Vec::new();
    let mut provenance = BTreeSet::new();
    for (channel, document) in &documents {
        subjects.push(RyeOsFieldSubjectVm {
            source: document.source.clone(),
            kind: document.subject.kind.clone(),
            id: document.subject.id.clone(),
            definition_ref: document.subject.definition_ref.clone(),
            definition_hash: document.subject.definition_hash.clone(),
        });
        for warning in &document.warnings {
            warnings.push(compact_value(warning));
        }
        for entity in &document.entities {
            provenance.insert(entity.provenance.source_ref.clone());
            let candidate = MergedEntity {
                source: document.source.clone(),
                owner: channel.clone(),
                raw: entity.clone(),
            };
            entity_candidates
                .entry(entity.id.clone())
                .or_default()
                .push(candidate);
        }
        for relation in &document.relations {
            provenance.insert(relation.provenance.source_ref.clone());
            let candidate = MergedRelation {
                source: document.source.clone(),
                owner: channel.clone(),
                raw: relation.clone(),
            };
            relation_candidates
                .entry(relation.id.clone())
                .or_default()
                .push(candidate);
        }
        for preview in &document.previews {
            match project_preview(preview) {
                Ok(preview) => preview_candidates
                    .entry(preview.id.clone())
                    .or_default()
                    .push((channel.clone(), preview)),
                Err(error) => warnings.push(error),
            }
        }
        for metric in &document.metrics {
            match project_metric(metric) {
                Ok(metric) => metric_candidates
                    .entry(metric.id.clone())
                    .or_default()
                    .push((channel.clone(), metric)),
                Err(error) => warnings.push(error),
            }
        }
    }

    let collided_entities = entity_candidates
        .iter()
        .filter(|(_, candidates)| {
            candidates
                .first()
                .is_some_and(|first| candidates.iter().skip(1).any(|item| item.raw != first.raw))
        })
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    let entity_owners = entity_candidates
        .iter()
        .flat_map(|(id, candidates)| {
            candidates
                .iter()
                .map(move |candidate| (candidate.owner.clone(), id.clone()))
        })
        .collect::<BTreeSet<_>>();
    let mut merged_entities = BTreeMap::<String, MergedEntity>::new();
    for (id, candidates) in entity_candidates {
        if collided_entities.contains(&id) {
            warnings.push(format!(
                "entity '{id}' has divergent owners {}; each copy was source-namespaced",
                candidates
                    .iter()
                    .map(|candidate| candidate.owner.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            for mut candidate in candidates {
                let namespaced = namespaced_collision_id(&candidate.owner, &id);
                candidate.raw.id.clone_from(&namespaced);
                if let Some(parent_id) = candidate.raw.parent_id.as_mut()
                    && collided_entities.contains(parent_id)
                    && entity_owners.contains(&(candidate.owner.clone(), parent_id.clone()))
                {
                    *parent_id = namespaced_collision_id(&candidate.owner, parent_id);
                }
                merged_entities.insert(namespaced, candidate);
            }
        } else if let Some(candidate) = candidates.into_iter().next() {
            merged_entities.insert(id, candidate);
        }
    }

    let collided_relations = relation_candidates
        .iter()
        .filter(|(_, candidates)| {
            candidates
                .first()
                .is_some_and(|first| candidates.iter().skip(1).any(|item| item.raw != first.raw))
        })
        .map(|(id, _)| id.clone())
        .collect::<BTreeSet<_>>();
    let mut merged_relations = BTreeMap::<String, MergedRelation>::new();
    for (id, candidates) in relation_candidates {
        let divergent = collided_relations.contains(&id);
        if divergent {
            warnings.push(format!(
                "relation '{id}' has divergent owners {}; each copy was source-namespaced",
                candidates
                    .iter()
                    .map(|candidate| candidate.owner.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for mut candidate in candidates
            .into_iter()
            .take(if divergent { usize::MAX } else { 1 })
        {
            for endpoint in [&mut candidate.raw.source_id, &mut candidate.raw.target_id] {
                if collided_entities.contains(endpoint)
                    && entity_owners.contains(&(candidate.owner.clone(), endpoint.clone()))
                {
                    *endpoint = namespaced_collision_id(&candidate.owner, endpoint);
                }
            }
            let merged_id = if divergent {
                namespaced_collision_id(&candidate.owner, &id)
            } else {
                id.clone()
            };
            candidate.raw.id.clone_from(&merged_id);
            merged_relations.insert(merged_id, candidate);
        }
    }

    let mut previews = Vec::new();
    for (id, candidates) in preview_candidates {
        let divergent = candidates
            .first()
            .is_some_and(|first| candidates.iter().skip(1).any(|item| item.1 != first.1));
        for (owner, mut preview) in
            candidates
                .into_iter()
                .take(if divergent { usize::MAX } else { 1 })
        {
            if divergent {
                preview.id = namespaced_collision_id(&owner, &id);
            }
            if let Some(entity_id) = preview.entity_id.as_mut()
                && collided_entities.contains(entity_id)
                && entity_owners.contains(&(owner.clone(), entity_id.clone()))
            {
                *entity_id = namespaced_collision_id(&owner, entity_id);
            }
            previews.push(preview);
        }
        if divergent {
            warnings.push(format!(
                "preview '{id}' has divergent owners and was source-namespaced"
            ));
        }
    }
    let mut metrics = Vec::new();
    for (id, candidates) in metric_candidates {
        let divergent = candidates
            .first()
            .is_some_and(|first| candidates.iter().skip(1).any(|item| item.1 != first.1));
        for (owner, mut metric) in
            candidates
                .into_iter()
                .take(if divergent { usize::MAX } else { 1 })
        {
            if divergent {
                metric.id = namespaced_collision_id(&owner, &id);
            }
            metrics.push(metric);
        }
        if divergent {
            warnings.push(format!(
                "metric '{id}' has divergent owners and was source-namespaced"
            ));
        }
    }

    let mut groups = projection
        .groups
        .iter()
        .map(|group| RyeOsFieldGroupVm {
            id: group.id.clone(),
            label: group.label.clone(),
            parent_id: group.parent.clone(),
            layout: parse_layout(group.layout.as_deref(), &mut warnings),
            collapsed: local.collapsed_groups.contains(&group.id),
            aggregate: group.aggregate.clone(),
        })
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| left.id.cmp(&right.id));

    let mut entities = merged_entities
        .values()
        .map(|entity| {
            project_entity(
                entity,
                &projection,
                binding,
                view_ref,
                &local,
                &mut warnings,
            )
        })
        .collect::<Vec<_>>();
    for entity in &mut entities {
        entity.preview_ids = previews
            .iter()
            .filter(|preview| preview.entity_id.as_deref() == Some(entity.id.as_str()))
            .map(|preview| preview.id.clone())
            .collect();
        entity.preview_ids.sort();
    }
    entities.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.order.cmp(&right.order))
            .then_with(|| left.id.cmp(&right.id))
    });
    let entity_ids = entities
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<BTreeSet<_>>();
    let mut relations = merged_relations
        .values()
        .filter_map(|relation| {
            if !entity_ids.contains(&relation.raw.source_id)
                || !entity_ids.contains(&relation.raw.target_id)
            {
                warnings.push(format!(
                    "relation '{}' has an unresolved endpoint",
                    relation.raw.id
                ));
                return None;
            }
            Some(project_relation(
                relation,
                &projection,
                &local,
                &mut warnings,
            ))
        })
        .collect::<Vec<_>>();
    relations.extend(project_derived_relations(
        &merged_entities,
        &projection,
        &local,
        &mut warnings,
    ));
    relations.sort_by(|left, right| left.id.cmp(&right.id));

    let mut layer_counts = BTreeMap::<String, usize>::new();
    for entity in &entities {
        for layer in &entity.layer_ids {
            *layer_counts.entry(layer.clone()).or_default() += 1;
        }
    }
    for relation in &relations {
        for layer in &relation.layer_ids {
            *layer_counts.entry(layer.clone()).or_default() += 1;
        }
    }
    let mut layers = projection
        .layers
        .iter()
        .map(|layer| RyeOsFieldLayerVm {
            id: layer.id.clone(),
            label: layer.label.clone(),
            visible: layer.default_visible && !local.hidden_layers.contains(&layer.id),
            count: layer_counts.get(&layer.id).copied().unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    layers.sort_by(|left, right| left.id.cmp(&right.id));

    let traversal = entities
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<Vec<_>>();
    let selected = local
        .selected
        .clone()
        .filter(|selected| entity_ids.contains(selected))
        .or_else(|| traversal.first().cloned());
    for entity in &mut entities {
        entity.selected = selected.as_deref() == Some(entity.id.as_str());
    }
    let query = local.query.trim().to_ascii_lowercase();
    let match_ids = if query.is_empty() {
        Vec::new()
    } else {
        entities
            .iter()
            .filter(|entity| searchable_text(entity).contains(&query))
            .map(|entity| entity.id.clone())
            .collect::<Vec<_>>()
    };
    let active_match = local
        .search_match
        .clone()
        .filter(|id| match_ids.contains(id))
        .or_else(|| match_ids.first().cloned());

    let mut compare = local
        .compare
        .iter()
        .filter(|id| entity_ids.contains(*id))
        .take(2)
        .cloned()
        .collect::<Vec<_>>();
    if compare
        .first()
        .is_some_and(|id| preview_for_entity(&previews, id).is_none())
    {
        compare.clear();
    }
    if compare.len() == 2
        && !entities_have_compatible_previews_in(&previews, &compare[0], &compare[1])
    {
        warnings.push("field comparison was cleared because its previews are incompatible".into());
        compare.truncate(1);
    }

    let replay = replay_vm(&documents, &local);
    let cursor = replay.anchor.clone();
    let expansions = expansion_vms(&documents, &source_status, &local);
    let structural_revision = hash_value(&json!({
        "groups": groups,
        "layers": layers,
        "entities": entities.iter().map(structural_entity).collect::<Vec<_>>(),
        "relations": relations.iter().map(structural_relation).collect::<Vec<_>>(),
    }));
    previews.sort_by(|left, right| left.id.cmp(&right.id));
    metrics.sort_by(|left, right| left.id.cmp(&right.id));
    let data_revision = hash_value(&json!({
        "sources": source_status.iter().map(|source| (&source.name, &source.revision)).collect::<Vec<_>>(),
        "entities": entities,
        "relations": relations,
        "previews": previews,
        "metrics": metrics,
        "expansions": expansions,
    }));
    apply_grid_comparison(&mut previews, &compare);
    // Arrival fingerprints and transient change records are derived source
    // state, not operator-local controls. Keeping them out of this digest
    // prevents a source refresh from masquerading as a local interaction.
    let local_revision = hash_value(&json!({
        "selected": local.selected,
        "collapsed_groups": local.collapsed_groups,
        "hidden_layers": local.hidden_layers,
        "compare": local.compare,
        "cursor": local.cursor,
        "playback": local.playback,
        "query": local.query,
        "search_match": local.search_match,
        "expansions": local.expansions,
    }));
    let revision = hash_value(&json!({
        "structural": structural_revision,
        "data": data_revision,
        "local": local_revision,
    }));

    warnings.sort();
    warnings.dedup();
    warnings.truncate(MAX_FIELD_WARNINGS);
    subjects.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.id.cmp(&right.id))
    });
    RyeOsFieldVm {
        schema_version: FIELD_VM_SCHEMA.to_string(),
        id: field_id.to_string(),
        title: title.to_string(),
        revision,
        structural_revision,
        data_revision,
        local_revision,
        sources: source_status,
        subjects,
        groups,
        layers,
        entities,
        relations,
        previews,
        metrics,
        traversal,
        selected,
        compare,
        cursor,
        replay,
        search: RyeOsFieldSearchVm {
            query: local.query,
            match_ids,
            active_match,
            truncated: false,
        },
        expansions,
        changes: local
            .changes
            .iter()
            .filter_map(|(_key, change)| {
                let kind = match change.kind.as_str() {
                    "entered" => RyeOsFieldChangeKind::Entered,
                    "exited" => RyeOsFieldChangeKind::Exited,
                    "updated" => RyeOsFieldChangeKind::Updated,
                    "status_changed" => RyeOsFieldChangeKind::StatusChanged,
                    "relation_added" => RyeOsFieldChangeKind::RelationAdded,
                    "relation_removed" => RyeOsFieldChangeKind::RelationRemoved,
                    _ => return None,
                };
                Some(RyeOsFieldChangeVm {
                    id: change.id.clone(),
                    kind,
                    at_ms: change.at_ms,
                    tone: change
                        .tone
                        .as_ref()
                        .and_then(|tone| serde_json::from_value(Value::String(tone.clone())).ok()),
                    prior_fingerprint: change.prior_fingerprint.clone(),
                    fingerprint: change.fingerprint.clone(),
                    tombstone: change
                        .tombstone_label
                        .as_ref()
                        .map(|label| RyeOsFieldTombstoneVm {
                            label: label.clone(),
                            traits: change
                                .tombstone_traits
                                .as_ref()
                                .and_then(|value| serde_json::from_value(value.clone()).ok())
                                .unwrap_or_default(),
                        }),
                })
            })
            .take(512)
            .collect(),
        warnings,
        provenance: provenance.into_iter().collect(),
    }
}

/// Fingerprint the expensive projection inputs. Accepted source revision and
/// subject identity are the cache boundary; refresh/error phase is included
/// because source health belongs in the serialized VM even when facts stay
/// unchanged.
pub(crate) fn field_projection_fingerprint(
    field_id: &str,
    title: &str,
    view_ref: &str,
    binding: &ViewBinding,
    source_inputs: &[FieldSourceInput<'_>],
) -> String {
    let mut sources = source_inputs
        .iter()
        .map(|input| {
            let (revision, parse_error) = match input.parsed {
                Some(Ok(document)) => (Some(document.revision.as_str()), None),
                Some(Err(error)) => (None, Some(error.as_str())),
                None => (
                    input
                        .response
                        .and_then(|response| response.get("revision"))
                        .and_then(Value::as_str),
                    None,
                ),
            };
            let uncached_digest = input
                .parsed
                .is_none()
                .then(|| input.response.map(hash_value))
                .flatten();
            json!({
                "channel": input.channel,
                "source_ref": input.source_ref,
                "subject": input.subject_fingerprint,
                "revision": revision,
                "parse_error": parse_error,
                "uncached_digest": uncached_digest,
                "fetch_error": input.error,
                "refreshing": input.refreshing,
            })
        })
        .collect::<Vec<_>>();
    sources.sort_by(|left, right| left["channel"].as_str().cmp(&right["channel"].as_str()));
    hash_value(&json!({
        "field_id": field_id,
        "title": title,
        "view_ref": view_ref,
        "projection": &binding.projections,
        "selection": &binding.selection,
        "sources": sources,
    }))
}

/// Apply instance-local controls to a cached projection. This only walks the
/// bounded projected vectors; parsing, collision folding, joins, rules,
/// preview decoding, and intent construction stay cached.
pub(crate) fn apply_field_local_state(
    mut field: RyeOsFieldVm,
    local: Option<&FieldLocalState>,
) -> RyeOsFieldVm {
    let local = local.cloned().unwrap_or_default();
    for group in &mut field.groups {
        group.collapsed = local.collapsed_groups.contains(&group.id);
    }
    for layer in &mut field.layers {
        // The cached base carries the authored default visibility.
        layer.visible = layer.visible && !local.hidden_layers.contains(&layer.id);
    }

    let entity_ids = field
        .entities
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<BTreeSet<_>>();
    let selected = local
        .selected
        .clone()
        .filter(|selected| entity_ids.contains(selected))
        .or_else(|| field.traversal.first().cloned());
    for entity in &mut field.entities {
        entity.selected = selected.as_deref() == Some(entity.id.as_str());
    }
    for relation in &mut field.relations {
        relation.selected = selected.as_ref().is_some_and(|selected| {
            selected == &relation.source_id || selected == &relation.target_id
        });
    }
    field.selected = selected;

    let query = local.query.trim().to_ascii_lowercase();
    let match_ids = if query.is_empty() {
        Vec::new()
    } else {
        field
            .entities
            .iter()
            .filter(|entity| searchable_text(entity).contains(&query))
            .map(|entity| entity.id.clone())
            .collect::<Vec<_>>()
    };
    let active_match = local
        .search_match
        .clone()
        .filter(|id| match_ids.contains(id))
        .or_else(|| match_ids.first().cloned());
    field.search = RyeOsFieldSearchVm {
        query: local.query.clone(),
        match_ids,
        active_match,
        truncated: false,
    };

    let mut compare = local
        .compare
        .iter()
        .filter(|id| entity_ids.contains(*id))
        .take(2)
        .cloned()
        .collect::<Vec<_>>();
    if compare
        .first()
        .is_some_and(|id| preview_for_entity(&field.previews, id).is_none())
    {
        compare.clear();
    }
    if compare.len() == 2
        && !entities_have_compatible_previews_in(&field.previews, &compare[0], &compare[1])
    {
        field
            .warnings
            .push("field comparison was cleared because its previews are incompatible".into());
        compare.truncate(1);
    }
    field.compare = compare;
    field.replay.playing = local.playback.playing;
    field.cursor = field.replay.anchor.clone();

    let mut expansion_keys = field
        .expansions
        .iter()
        .map(|item| (item.source.clone(), item.root_id.clone()))
        .collect::<BTreeSet<_>>();
    for key in local.expansions.keys() {
        let Some((source, root_id)) = key.split_once('\0') else {
            continue;
        };
        if expansion_keys.insert((source.to_string(), root_id.to_string())) {
            let phase = field
                .sources
                .iter()
                .find(|status| status.name == source)
                .map(|status| status.phase)
                .unwrap_or(RyeOsSourcePhase::Error);
            field.expansions.push(RyeOsFieldExpansionVm {
                source: source.to_string(),
                root_id: root_id.to_string(),
                phase,
                applied_depth: 0,
                entity_count: 0,
                truncated: false,
                can_continue: false,
            });
        }
    }
    field.expansions.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then_with(|| left.root_id.cmp(&right.root_id))
    });

    field.structural_revision = hash_value(&json!({
        "groups": field.groups,
        "layers": field.layers,
        "entities": field.entities.iter().map(structural_entity).collect::<Vec<_>>(),
        "relations": field.relations.iter().map(structural_relation).collect::<Vec<_>>(),
    }));
    field.data_revision = hash_value(&json!({
        "sources": field.sources.iter().map(|source| (&source.name, &source.revision)).collect::<Vec<_>>(),
        "entities": field.entities,
        "relations": field.relations,
        "previews": field.previews,
        "metrics": field.metrics,
        "expansions": field.expansions,
    }));
    apply_grid_comparison(&mut field.previews, &field.compare);
    field.local_revision = hash_value(&json!({
        "selected": local.selected,
        "collapsed_groups": local.collapsed_groups,
        "hidden_layers": local.hidden_layers,
        "compare": local.compare,
        "cursor": local.cursor,
        "playback": local.playback,
        "query": local.query,
        "search_match": local.search_match,
        "expansions": local.expansions,
    }));
    field.revision = hash_value(&json!({
        "structural": field.structural_revision,
        "data": field.data_revision,
        "local": field.local_revision,
    }));
    field.changes = field_change_vms(&local);
    field.warnings.sort();
    field.warnings.dedup();
    field.warnings.truncate(MAX_FIELD_WARNINGS);
    field
}

fn field_change_vms(local: &FieldLocalState) -> Vec<RyeOsFieldChangeVm> {
    local
        .changes
        .values()
        .filter_map(|change| {
            let kind = match change.kind.as_str() {
                "entered" => RyeOsFieldChangeKind::Entered,
                "exited" => RyeOsFieldChangeKind::Exited,
                "updated" => RyeOsFieldChangeKind::Updated,
                "status_changed" => RyeOsFieldChangeKind::StatusChanged,
                "relation_added" => RyeOsFieldChangeKind::RelationAdded,
                "relation_removed" => RyeOsFieldChangeKind::RelationRemoved,
                _ => return None,
            };
            Some(RyeOsFieldChangeVm {
                id: change.id.clone(),
                kind,
                at_ms: change.at_ms,
                tone: change
                    .tone
                    .as_ref()
                    .and_then(|tone| serde_json::from_value(Value::String(tone.clone())).ok()),
                prior_fingerprint: change.prior_fingerprint.clone(),
                fingerprint: change.fingerprint.clone(),
                tombstone: change
                    .tombstone_label
                    .as_ref()
                    .map(|label| RyeOsFieldTombstoneVm {
                        label: label.clone(),
                        traits: change
                            .tombstone_traits
                            .as_ref()
                            .and_then(|value| serde_json::from_value(value.clone()).ok())
                            .unwrap_or_default(),
                    }),
            })
        })
        .take(512)
        .collect()
}

/// Fingerprints the renderer-neutral projected facts, excluding transient
/// selection and executable intents. Keys carry the fact kind as well as the
/// stable ID so an entity and relation with the same authored ID cannot alias.
pub(crate) fn semantic_fingerprints(
    field: &RyeOsFieldVm,
) -> BTreeMap<String, FieldFingerprintState> {
    let mut fingerprints = BTreeMap::new();
    for entity in &field.entities {
        let traits = serde_json::to_value(&entity.traits).unwrap_or(Value::Null);
        let fingerprint = hash_value(&json!({
            "source": entity.source,
            "kind": entity.kind,
            "role": entity.role,
            "label": entity.label,
            "secondary": entity.secondary,
            "parent_id": entity.parent_id,
            "group_id": entity.group_id,
            "layer_ids": entity.layer_ids,
            "lane": entity.lane,
            "rank": entity.rank,
            "order": entity.order,
            "status": entity.status,
            "tone": entity.tone,
            "traits": entity.traits,
            "badges": entity.badges,
            "preview_ids": entity.preview_ids,
            "selectable": entity.selectable,
            "accessibility_label": entity.accessibility_label,
            "detail": entity.detail,
        }));
        fingerprints.insert(
            format!("entity\0{}", entity.id),
            FieldFingerprintState {
                id: entity.id.clone(),
                fact_kind: "entity".to_string(),
                fingerprint,
                status: entity.status.clone(),
                label: Some(entity.label.clone()),
                tone: serde_json::to_value(entity.tone)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string)),
                traits,
            },
        );
    }
    for relation in &field.relations {
        let fingerprint = hash_value(&json!({
            "kind": relation.kind,
            "source_id": relation.source_id,
            "target_id": relation.target_id,
            "directed": relation.directed,
            "tone": relation.tone,
            "stroke": relation.stroke,
            "emphasis": relation.emphasis,
            "motion": relation.motion,
            "label": relation.label,
            "layer_ids": relation.layer_ids,
            "accessibility_label": relation.accessibility_label,
        }));
        fingerprints.insert(
            format!("relation\0{}", relation.id),
            FieldFingerprintState {
                id: relation.id.clone(),
                fact_kind: "relation".to_string(),
                fingerprint,
                status: None,
                label: relation.label.clone(),
                tone: serde_json::to_value(relation.tone)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string)),
                traits: Value::Null,
            },
        );
    }
    fingerprints
}

pub(crate) fn source_subject_fingerprint(source_ref: &str, params: &Value) -> String {
    hash_value(&json!({ "source_ref": source_ref, "params": params }))
}

fn placeholder_vm(id: &str, title: &str, warning: String) -> RyeOsFieldVm {
    RyeOsFieldVm {
        schema_version: FIELD_VM_SCHEMA.to_string(),
        id: id.to_string(),
        title: title.to_string(),
        revision: hash_value(&Value::String(warning.clone())),
        structural_revision: String::new(),
        data_revision: String::new(),
        local_revision: String::new(),
        sources: Vec::new(),
        subjects: Vec::new(),
        groups: Vec::new(),
        layers: Vec::new(),
        entities: Vec::new(),
        relations: Vec::new(),
        previews: Vec::new(),
        metrics: Vec::new(),
        traversal: Vec::new(),
        selected: None,
        compare: Vec::new(),
        cursor: None,
        replay: RyeOsFieldReplayVm::default(),
        search: RyeOsFieldSearchVm::default(),
        expansions: Vec::new(),
        changes: Vec::new(),
        warnings: vec![warning],
        provenance: Vec::new(),
    }
}

fn project_entity(
    entity: &MergedEntity,
    projection: &FieldProjection,
    binding: &ViewBinding,
    view_ref: &str,
    local: &FieldLocalState,
    warnings: &mut Vec<String>,
) -> RyeOsFieldEntityVm {
    let raw_value = serde_json::to_value(&entity.raw).unwrap_or(Value::Null);
    let mut set = BTreeMap::new();
    for rule in &projection.entity_rules {
        if matches_record(&entity.source, &raw_value, &rule.matches) {
            set.extend(rule.set.clone());
        }
    }
    let layers = resolved_string_list(set.get("layer").or_else(|| set.get("layers")), &raw_value);
    let tone = parse_tone(
        resolved_string(set.get("tone"), &raw_value).as_deref(),
        warnings,
    );
    let traits = RyeOsFieldEntityTraitsVm {
        shape: parse_shape(
            resolved_string(set.get("shape"), &raw_value).as_deref(),
            warnings,
        ),
        fill: parse_fill(
            resolved_string(set.get("fill"), &raw_value).as_deref(),
            warnings,
        ),
        stroke: parse_stroke(
            resolved_string(set.get("stroke"), &raw_value).as_deref(),
            warnings,
        ),
        emphasis: parse_emphasis(
            resolved_string(set.get("emphasis"), &raw_value).as_deref(),
            warnings,
        ),
        motion: parse_motion(
            resolved_string(set.get("motion"), &raw_value).as_deref(),
            warnings,
        ),
    };
    let selected = local.selected.as_deref() == Some(entity.raw.id.as_str());
    let select_intent = binding.selection.as_ref().and_then(|selection| {
        selection
            .change
            .as_ref()
            .map(|affordance_id| RyeOsUiIntent::InvokeAffordance {
                view_ref: view_ref.to_string(),
                affordance_id: affordance_id.clone(),
                record: raw_value.clone(),
            })
    });
    let activate_intent = binding.selection.as_ref().and_then(|selection| {
        selection
            .activate
            .as_ref()
            .map(|affordance_id| RyeOsUiIntent::InvokeAffordance {
                view_ref: view_ref.to_string(),
                affordance_id: affordance_id.clone(),
                record: raw_value.clone(),
            })
    });
    let label =
        resolved_string(set.get("label"), &raw_value).unwrap_or_else(|| entity.raw.label.clone());
    let secondary = resolved_string(set.get("secondary"), &raw_value);
    RyeOsFieldEntityVm {
        id: entity.raw.id.clone(),
        source: entity.owner.clone(),
        kind: entity.raw.kind.clone(),
        role: resolved_string(set.get("role"), &raw_value)
            .or_else(|| path_value(&raw_value, "attributes.role").and_then(value_string)),
        label: label.clone(),
        secondary,
        parent_id: entity.raw.parent_id.clone(),
        group_id: resolved_string(set.get("group"), &raw_value),
        layer_ids: layers,
        lane: resolved_string(set.get("lane"), &raw_value),
        rank: resolved_i64(set.get("rank"), &raw_value),
        order: resolved_i64(set.get("order"), &raw_value),
        status: entity.raw.status.clone(),
        tone,
        traits,
        badges: Vec::new(),
        preview_ids: Vec::new(),
        selected,
        selectable: true,
        select_intent,
        activate_intent,
        accessibility_label: format!(
            "{}; {}; {}",
            label,
            entity.raw.kind,
            entity.raw.status.as_deref().unwrap_or("no status")
        ),
        detail: scalar_details(&entity.raw.attributes),
    }
}

fn project_relation(
    relation: &MergedRelation,
    projection: &FieldProjection,
    local: &FieldLocalState,
    warnings: &mut Vec<String>,
) -> RyeOsFieldRelationVm {
    let raw_value = serde_json::to_value(&relation.raw).unwrap_or(Value::Null);
    let mut set = BTreeMap::new();
    for rule in &projection.relation_rules {
        if matches_record(&relation.source, &raw_value, &rule.matches) {
            set.extend(rule.set.clone());
        }
    }
    RyeOsFieldRelationVm {
        id: relation.raw.id.clone(),
        kind: relation.raw.kind.clone(),
        source_id: relation.raw.source_id.clone(),
        target_id: relation.raw.target_id.clone(),
        directed: resolved_bool(set.get("directed"), &raw_value).unwrap_or(relation.raw.directed),
        tone: parse_tone(
            resolved_string(set.get("tone"), &raw_value).as_deref(),
            warnings,
        ),
        stroke: parse_stroke(
            resolved_string(set.get("stroke"), &raw_value).as_deref(),
            warnings,
        ),
        emphasis: parse_emphasis(
            resolved_string(set.get("emphasis"), &raw_value).as_deref(),
            warnings,
        ),
        motion: parse_motion(
            resolved_string(set.get("motion"), &raw_value).as_deref(),
            warnings,
        ),
        label: resolved_string(set.get("label"), &raw_value),
        layer_ids: resolved_string_list(set.get("layer").or_else(|| set.get("layers")), &raw_value),
        selected: local.selected.as_ref().is_some_and(|selected| {
            selected == &relation.raw.source_id || selected == &relation.raw.target_id
        }),
        activate_intent: None,
        accessibility_label: format!(
            "{} from {} to {}",
            relation.raw.kind, relation.raw.source_id, relation.raw.target_id
        ),
    }
}

fn project_derived_relations(
    entities: &BTreeMap<String, MergedEntity>,
    projection: &FieldProjection,
    local: &FieldLocalState,
    warnings: &mut Vec<String>,
) -> Vec<RyeOsFieldRelationVm> {
    let mut out = Vec::new();
    for derived in &projection.derived_relations {
        if derived.left.keys.is_empty() || derived.left.keys.len() != derived.right.keys.len() {
            warnings.push(format!(
                "derived relation '{}' has invalid compound keys",
                derived.id
            ));
            continue;
        }
        let left = join_index(entities, &derived.left);
        let right = join_index(entities, &derived.right);
        for (key, left_ids) in left {
            let Some(right_ids) = right.get(&key) else {
                continue;
            };
            if left_ids.len() != 1 || right_ids.len() != 1 {
                warnings.push(format!(
                    "derived relation '{}' rejected non-unique compound key",
                    derived.id
                ));
                continue;
            }
            let source_id = left_ids[0].clone();
            let target_id = right_ids[0].clone();
            let kind = derived
                .relation
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("related")
                .to_string();
            let id = format!("derived:{}:{}:{}", derived.id, source_id, target_id);
            out.push(RyeOsFieldRelationVm {
                id,
                kind: kind.clone(),
                source_id: source_id.clone(),
                target_id: target_id.clone(),
                directed: derived
                    .relation
                    .get("directed")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                tone: parse_tone(
                    derived.relation.get("tone").and_then(Value::as_str),
                    warnings,
                ),
                stroke: parse_stroke(
                    derived.relation.get("stroke").and_then(Value::as_str),
                    warnings,
                ),
                emphasis: parse_emphasis(
                    derived.relation.get("emphasis").and_then(Value::as_str),
                    warnings,
                ),
                motion: parse_motion(
                    derived.relation.get("motion").and_then(Value::as_str),
                    warnings,
                ),
                label: derived
                    .relation
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                layer_ids: resolved_string_list(derived.relation.get("layer"), &Value::Null),
                selected: local
                    .selected
                    .as_ref()
                    .is_some_and(|selected| selected == &source_id || selected == &target_id),
                activate_intent: None,
                accessibility_label: format!("{kind} from {source_id} to {target_id}"),
            });
        }
    }
    out
}

fn join_index(
    entities: &BTreeMap<String, MergedEntity>,
    side: &FieldJoinSide,
) -> BTreeMap<Vec<String>, Vec<String>> {
    let mut index = BTreeMap::<Vec<String>, Vec<String>>::new();
    for entity in entities.values() {
        let raw = serde_json::to_value(&entity.raw).unwrap_or(Value::Null);
        if !matches_record(&entity.source, &raw, &side.matches) {
            continue;
        }
        let values = side
            .keys
            .iter()
            .map(|path| path_value(&raw, path).and_then(value_string))
            .collect::<Option<Vec<_>>>();
        if let Some(values) = values {
            index.entry(values).or_default().push(entity.raw.id.clone());
        }
    }
    index
}

fn matches_record(source: &str, record: &Value, predicates: &BTreeMap<String, Value>) -> bool {
    predicates.iter().all(|(path, predicate)| {
        if path == "source" {
            return predicate.as_str() == Some(source);
        }
        let actual = path_value(record, path);
        match predicate {
            Value::Object(object) if object.len() == 1 && object.contains_key("eq") => {
                actual == object.get("eq")
            }
            Value::Object(object) if object.len() == 1 && object.contains_key("in") => object
                .get("in")
                .and_then(Value::as_array)
                .is_some_and(|values| actual.is_some_and(|actual| values.contains(actual))),
            Value::Object(object) if object.len() == 1 && object.contains_key("exists") => {
                actual.is_some()
                    == object
                        .get("exists")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
            }
            expected => actual == Some(expected),
        }
    })
}

fn resolved_string(value: Option<&Value>, record: &Value) -> Option<String> {
    let value = value?;
    if let Some(object) = value.as_object()
        && let Some(field) = object.get("field").and_then(Value::as_str)
    {
        let source = path_value(record, field).and_then(value_string);
        return source
            .as_ref()
            .and_then(|source| object.get("map")?.get(source))
            .and_then(value_string)
            .or_else(|| object.get("default").and_then(value_string));
    }
    let authored = value_string(value)?;
    if authored.starts_with('{') && authored.ends_with('}') && authored.matches('{').count() == 1 {
        return path_value(record, &authored[1..authored.len() - 1]).and_then(value_string);
    }
    Some(authored)
}

fn resolved_string_list(value: Option<&Value>, record: &Value) -> Vec<String> {
    match value {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(|value| resolved_string(Some(value), record))
            .collect(),
        value => resolved_string(value, record).into_iter().collect(),
    }
}

fn resolved_i64(value: Option<&Value>, record: &Value) -> Option<i64> {
    match value? {
        Value::Number(value) => value.as_i64(),
        value => resolved_string(Some(value), record)?.parse().ok(),
    }
}

fn resolved_bool(value: Option<&Value>, record: &Value) -> Option<bool> {
    match value? {
        Value::Bool(value) => Some(*value),
        value => resolved_string(Some(value), record)?.parse().ok(),
    }
}

fn path_value<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

fn value_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn namespaced_collision_id(owner: &str, id: &str) -> String {
    format!("source:{owner}::{id}")
}

fn scalar_details(attributes: &Value) -> Vec<RyeOsRowDetailVm> {
    fn visit(prefix: &str, value: &Value, out: &mut Vec<RyeOsRowDetailVm>) {
        if out.len() >= 24 {
            return;
        }
        match value {
            Value::Object(values) => {
                for (key, value) in values {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    visit(&path, value, out);
                }
            }
            value => {
                if let Some(value) = value_string(value) {
                    out.push(RyeOsRowDetailVm {
                        field: prefix.to_string(),
                        value,
                        tone: None,
                    });
                }
            }
        }
    }
    let mut out = Vec::new();
    visit("", attributes, &mut out);
    out
}

fn replay_vm(
    documents: &[(String, FieldFactsDocument)],
    local: &FieldLocalState,
) -> RyeOsFieldReplayVm {
    let Some((_channel, document)) = documents.iter().find(|(_, document)| {
        document.replay.is_some() || matches!(document.cursor, FieldCursor::BraidCut { .. })
    }) else {
        return RyeOsFieldReplayVm {
            playing: local.playback.playing,
            ..Default::default()
        };
    };
    let (mode, anchor, outside_cut) = match &document.cursor {
        FieldCursor::Live => ("live".to_string(), None, Vec::new()),
        FieldCursor::BraidCut {
            anchor,
            outside_cut,
            ..
        } => (
            "braid_cut".to_string(),
            Some(anchor.clone()),
            outside_cut.clone(),
        ),
    };
    let replay = document.replay.as_ref();
    let mut rail = document
        .entities
        .iter()
        .filter_map(|entity| {
            entity
                .event_ref
                .as_ref()
                .map(|event| (event, &entity.label))
        })
        .map(|(event, label)| RyeOsFieldReplayEntryVm {
            event: event.clone(),
            label: label.clone(),
            selected: anchor.as_ref() == Some(event),
        })
        .collect::<Vec<_>>();
    rail.sort_by(|left, right| left.event.chain_seq.cmp(&right.event.chain_seq));
    RyeOsFieldReplayVm {
        mode,
        playing: local.playback.playing,
        anchor,
        previous: replay.and_then(|replay| replay.previous.clone()),
        next: replay.and_then(|replay| replay.next.clone()),
        live_head: replay.and_then(|replay| replay.live_head.clone()),
        rail,
        outside_cut,
    }
}

fn project_preview(value: &Value) -> Result<RyeOsArtifactPreviewVm, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "field preview must be an object".to_string())?;
    let id = required_string(object, "id", "field preview")?;
    let kind = required_string(object, "kind", "field preview")?;
    if kind != "indexed_grid" {
        return Err(format!("unsupported field preview kind '{kind}'"));
    }
    let label = object
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_string();
    let grid =
        Some(project_grid(object.get("grid").ok_or_else(|| {
            "indexed_grid preview requires grid".to_string()
        })?)?);
    let comparison_key = object
        .get("comparison_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 1024)
        .map(str::to_string);
    if object.get("comparison_key").is_some() && comparison_key.is_none() {
        return Err("field preview comparison_key is invalid".to_string());
    }
    Ok(RyeOsArtifactPreviewVm {
        id,
        entity_id: object
            .get("entity_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        kind,
        label,
        comparison_key,
        grid,
    })
}

fn expansion_vms(
    documents: &[(String, FieldFactsDocument)],
    statuses: &[RyeOsFieldSourceStatusVm],
    local: &FieldLocalState,
) -> Vec<RyeOsFieldExpansionVm> {
    let mut results = BTreeMap::<(String, String), FieldExpansionResult>::new();
    for (channel, document) in documents {
        for value in &document.expansions {
            if let Ok(result) = serde_json::from_value::<FieldExpansionResult>(value.clone()) {
                results.insert((channel.clone(), result.root_id.clone()), result);
            }
        }
    }
    let mut expansions = local
        .expansions
        .keys()
        .filter_map(|key| key.split_once('\0'))
        .map(|(source, root_id)| (source.to_string(), root_id.to_string()))
        .collect::<BTreeSet<_>>();
    expansions.extend(results.keys().cloned());
    expansions
        .into_iter()
        .map(|(source, root_id)| {
            let result = results.get(&(source.clone(), root_id.clone()));
            let phase = statuses
                .iter()
                .find(|status| status.name == source)
                .map(|status| status.phase)
                .unwrap_or(RyeOsSourcePhase::Error);
            RyeOsFieldExpansionVm {
                source,
                root_id,
                phase,
                applied_depth: result
                    .map(|result| result.applied_depth)
                    .unwrap_or_default(),
                entity_count: result.map(|result| result.entity_count).unwrap_or_default(),
                truncated: result.is_some_and(|result| result.truncated),
                can_continue: result.is_some_and(|result| result.continuation_token.is_some()),
            }
        })
        .collect()
}

fn preview_for_entity<'a>(
    previews: &'a [RyeOsArtifactPreviewVm],
    entity_id: &str,
) -> Option<&'a RyeOsArtifactPreviewVm> {
    previews
        .iter()
        .find(|preview| preview.entity_id.as_deref() == Some(entity_id) && preview.grid.is_some())
}

fn palette_meaning(grid: &RyeOsIndexedGridVm) -> Vec<(u16, &str, Option<&str>)> {
    grid.palette
        .iter()
        .map(|entry| (entry.index, entry.glyph.as_str(), entry.label.as_deref()))
        .collect()
}

fn previews_compatible(left: &RyeOsArtifactPreviewVm, right: &RyeOsArtifactPreviewVm) -> bool {
    let (Some(left_key), Some(right_key), Some(left_grid), Some(right_grid)) = (
        left.comparison_key.as_deref(),
        right.comparison_key.as_deref(),
        left.grid.as_ref(),
        right.grid.as_ref(),
    ) else {
        return false;
    };
    left_key == right_key
        && left.kind == right.kind
        && left_grid.width == right_grid.width
        && left_grid.height == right_grid.height
        && palette_meaning(left_grid) == palette_meaning(right_grid)
}

fn entities_have_compatible_previews_in(
    previews: &[RyeOsArtifactPreviewVm],
    left_entity_id: &str,
    right_entity_id: &str,
) -> bool {
    preview_for_entity(previews, left_entity_id)
        .zip(preview_for_entity(previews, right_entity_id))
        .is_some_and(|(left, right)| previews_compatible(left, right))
}

pub(crate) fn entity_has_comparable_preview(field: &RyeOsFieldVm, entity_id: &str) -> bool {
    preview_for_entity(&field.previews, entity_id)
        .is_some_and(|preview| preview.comparison_key.is_some())
}

pub(crate) fn entities_have_compatible_previews(
    field: &RyeOsFieldVm,
    left_entity_id: &str,
    right_entity_id: &str,
) -> bool {
    entities_have_compatible_previews_in(&field.previews, left_entity_id, right_entity_id)
}

fn apply_grid_comparison(previews: &mut [RyeOsArtifactPreviewVm], compare: &[String]) {
    let [left_id, right_id] = compare else {
        return;
    };
    let Some(left_index) = previews.iter().position(|preview| {
        preview.entity_id.as_deref() == Some(left_id) && preview.grid.is_some()
    }) else {
        return;
    };
    let Some(right_index) = previews.iter().position(|preview| {
        preview.entity_id.as_deref() == Some(right_id) && preview.grid.is_some()
    }) else {
        return;
    };
    if !previews_compatible(&previews[left_index], &previews[right_index]) {
        return;
    }
    let changed = previews[left_index]
        .grid
        .as_ref()
        .zip(previews[right_index].grid.as_ref())
        .map(|(left, right)| {
            left.cells
                .iter()
                .zip(&right.cells)
                .enumerate()
                .filter_map(|(index, (left, right))| {
                    (left != right).then(|| u32::try_from(index).expect("grid index is bounded"))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if left_index < right_index {
        let (left, right) = previews.split_at_mut(right_index);
        left[left_index].grid.as_mut().unwrap().changed = changed.clone();
        right[0].grid.as_mut().unwrap().changed = changed;
    } else {
        let (right, left) = previews.split_at_mut(left_index);
        left[0].grid.as_mut().unwrap().changed = changed.clone();
        right[right_index].grid.as_mut().unwrap().changed = changed;
    }
}

fn project_grid(value: &Value) -> Result<RyeOsIndexedGridVm, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "indexed grid must be an object".to_string())?;
    let width = object.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
    let height = object.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
    if width == 0 || height == 0 || width > MAX_GRID_AXIS || height > MAX_GRID_AXIS {
        return Err("indexed grid dimensions are outside the supported bounds".to_string());
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .filter(|count| *count <= MAX_GRID_CELLS)
        .ok_or_else(|| "indexed grid exceeds the decoded-cell limit".to_string())?;
    let cells = if let Some(cells) = object.get("cells").and_then(Value::as_array) {
        cells
            .iter()
            .map(|cell| {
                cell.as_u64()
                    .and_then(|cell| u16::try_from(cell).ok())
                    .ok_or_else(|| "indexed grid cell is not a u16".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?
    } else if let Some(runs) = object.get("rle").and_then(Value::as_array) {
        let mut cells = Vec::with_capacity(expected);
        for run in runs {
            let pair = run
                .as_array()
                .ok_or_else(|| "indexed grid RLE run must be [index,count]".to_string())?;
            let index = pair
                .first()
                .and_then(Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .ok_or_else(|| "indexed grid RLE index is invalid".to_string())?;
            let count = pair
                .get(1)
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| "indexed grid RLE count is invalid".to_string())?;
            if count == 0 || cells.len().saturating_add(count) > expected {
                return Err("indexed grid RLE exceeds its dimensions".to_string());
            }
            cells.extend(std::iter::repeat_n(index, count));
        }
        cells
    } else {
        return Err("indexed grid requires cells or rle".to_string());
    };
    if cells.len() != expected {
        return Err(format!(
            "indexed grid decoded {} cells; expected {expected}",
            cells.len()
        ));
    }
    let palette_values = object
        .get("palette")
        .and_then(Value::as_array)
        .ok_or_else(|| "indexed grid requires a palette".to_string())?;
    if palette_values.len() > MAX_GRID_PALETTE {
        return Err("indexed grid palette exceeds 256 entries".to_string());
    }
    let mut palette = Vec::new();
    let mut indexes = BTreeSet::new();
    for entry in palette_values {
        let entry = entry
            .as_object()
            .ok_or_else(|| "indexed grid palette entry must be an object".to_string())?;
        let index = entry
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .ok_or_else(|| "indexed grid palette index is invalid".to_string())?;
        if !indexes.insert(index) {
            return Err("indexed grid palette contains a duplicate index".to_string());
        }
        let glyph = required_string(entry, "glyph", "indexed grid palette entry")?;
        if glyph.is_empty() {
            return Err("indexed grid palette glyph must not be empty".to_string());
        }
        palette.push(RyeOsGridPaletteEntryVm {
            index,
            color: required_string(entry, "color", "indexed grid palette entry")?,
            glyph,
            label: entry
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    if cells.iter().any(|cell| !indexes.contains(cell)) {
        return Err("indexed grid cell references a missing palette index".to_string());
    }
    palette.sort_by_key(|entry| entry.index);
    Ok(RyeOsIndexedGridVm {
        width,
        height,
        cells,
        palette,
        changed: Vec::new(),
        labels: object
            .get("labels")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    })
}

fn project_metric(value: &Value) -> Result<RyeOsMetricVm, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "field metric must be an object".to_string())?;
    Ok(RyeOsMetricVm {
        id: required_string(object, "id", "field metric")?,
        label: required_string(object, "label", "field metric")?,
        value: object
            .get("value")
            .cloned()
            .ok_or_else(|| "field metric requires value".to_string())?,
        unit: object
            .get("unit")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn required_string(object: &Map<String, Value>, key: &str, label: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{label} requires string '{key}'"))
}

fn searchable_text(entity: &RyeOsFieldEntityVm) -> String {
    format!(
        "{} {} {} {} {}",
        entity.label,
        entity.secondary.as_deref().unwrap_or(""),
        entity.kind,
        entity.role.as_deref().unwrap_or(""),
        entity
            .badges
            .iter()
            .map(|badge| badge.label.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    )
    .to_ascii_lowercase()
}

fn structural_entity(entity: &RyeOsFieldEntityVm) -> Value {
    json!({
        "id": entity.id,
        "parent": entity.parent_id,
        "group": entity.group_id,
        "layers": entity.layer_ids,
        "lane": entity.lane,
        "rank": entity.rank,
        "order": entity.order,
        "traits": entity.traits,
    })
}

fn structural_relation(relation: &RyeOsFieldRelationVm) -> Value {
    json!({
        "id": relation.id,
        "source": relation.source_id,
        "target": relation.target_id,
        "directed": relation.directed,
        "layers": relation.layer_ids,
    })
}

fn hash_value(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn compact_value(value: &Value) -> String {
    let mut text = serde_json::to_string(value).unwrap_or_else(|_| "null".to_string());
    if text.len() > 512 {
        text.truncate(509);
        text.push_str("...");
    }
    text
}

macro_rules! parse_enum {
    ($name:ident, $type:ty, $default:expr, {$($value:literal => $variant:expr),+ $(,)?}) => {
        fn $name(value: Option<&str>, warnings: &mut Vec<String>) -> $type {
            match value {
                None => $default,
                $(Some($value) => $variant,)+
                Some(value) => {
                    warnings.push(format!("unknown field visual trait '{value}'"));
                    $default
                }
            }
        }
    };
}

parse_enum!(parse_shape, FieldShape, FieldShape::Rect, {
    "dot" => FieldShape::Dot, "disc" => FieldShape::Disc, "ring" => FieldShape::Ring,
    "rect" => FieldShape::Rect, "capsule" => FieldShape::Capsule, "diamond" => FieldShape::Diamond,
    "hex" => FieldShape::Hex, "anchor" => FieldShape::Anchor, "aggregate" => FieldShape::Aggregate,
    "grid" => FieldShape::Grid
});
parse_enum!(parse_fill, FieldFill, FieldFill::Solid, {
    "solid" => FieldFill::Solid, "hollow" => FieldFill::Hollow, "ghost" => FieldFill::Ghost
});
parse_enum!(parse_stroke, FieldStroke, FieldStroke::Solid, {
    "solid" => FieldStroke::Solid, "dashed" => FieldStroke::Dashed, "dotted" => FieldStroke::Dotted
});
parse_enum!(parse_emphasis, FieldEmphasis, FieldEmphasis::Normal, {
    "quiet" => FieldEmphasis::Quiet, "normal" => FieldEmphasis::Normal, "strong" => FieldEmphasis::Strong
});
parse_enum!(parse_motion, FieldMotion, FieldMotion::None, {
    "none" => FieldMotion::None, "pulse" => FieldMotion::Pulse, "flow" => FieldMotion::Flow
});
parse_enum!(parse_layout, FieldLayout, FieldLayout::Flow, {
    "flow" => FieldLayout::Flow, "lanes" => FieldLayout::Lanes, "stack" => FieldLayout::Stack,
    "grid" => FieldLayout::Grid, "overlay" => FieldLayout::Overlay
});
parse_enum!(parse_tone, RyeOsTone, RyeOsTone::Neutral, {
    "good" => RyeOsTone::Good, "warn" => RyeOsTone::Warn, "danger" => RyeOsTone::Danger,
    "neutral" => RyeOsTone::Neutral, "accent" => RyeOsTone::Accent
});

#[cfg(test)]
mod tests {
    use super::*;

    fn document(source: &str, id: &str) -> Value {
        json!({
            "schema_version": FIELD_FACTS_SCHEMA,
            "source": source,
            "subject": { "kind": "project", "id": "project:test" },
            "revision": format!("revision:{source}"),
            "cursor": { "mode": "live" },
            "truncated": false,
            "entities": [{
                "id": id,
                "kind": "step",
                "label": id,
                "status": "running",
                "attributes": { "rank": 2, "scope": "one" },
                "provenance": { "source_ref": format!("service:{source}"), "source_revision": format!("revision:{source}"), "evidence": [] }
            }],
            "relations": [],
            "previews": [], "metrics": [], "expansions": [], "warnings": []
        })
    }

    fn binding() -> ViewBinding {
        serde_json::from_value(json!({
            "widget": "field",
            "sources": {
                "project": { "ref": "service:project" },
                "execution": { "ref": "service:execution" }
            },
            "projections": {
                "schema_version": FIELD_PROJECTION_SCHEMA,
                "groups": [{ "id": "work", "label": "Work", "layout": "flow" }],
                "layers": [{ "id": "live", "label": "Live" }],
                "entity_rules": [{
                    "match": { "kind": "step" },
                    "set": { "group": "work", "layer": "live", "rank": "{attributes.rank}", "shape": "capsule" }
                }]
            }
        }))
        .unwrap()
    }

    #[test]
    fn source_arrival_permutations_produce_identical_field_vm() {
        let project = document("project", "step:a");
        let execution = document("execution", "step:b");
        let binding = binding();
        let left = project_field(
            "field:test",
            "Test",
            "view:test",
            &binding,
            &[
                FieldSourceInput {
                    channel: "project",
                    source_ref: "service:project",
                    subject_fingerprint: None,
                    response: Some(&project),
                    parsed: None,
                    error: None,
                    refreshing: false,
                },
                FieldSourceInput {
                    channel: "execution",
                    source_ref: "service:execution",
                    subject_fingerprint: None,
                    response: Some(&execution),
                    parsed: None,
                    error: None,
                    refreshing: false,
                },
            ],
            None,
        );
        let right = project_field(
            "field:test",
            "Test",
            "view:test",
            &binding,
            &[
                FieldSourceInput {
                    channel: "execution",
                    source_ref: "service:execution",
                    subject_fingerprint: None,
                    response: Some(&execution),
                    parsed: None,
                    error: None,
                    refreshing: false,
                },
                FieldSourceInput {
                    channel: "project",
                    source_ref: "service:project",
                    subject_fingerprint: None,
                    response: Some(&project),
                    parsed: None,
                    error: None,
                    refreshing: false,
                },
            ],
            None,
        );
        assert_eq!(left, right);
        assert_eq!(left.entities.len(), 2);
        assert_eq!(left.entities[0].traits.shape, FieldShape::Capsule);
    }

    #[test]
    fn indexed_grid_rle_is_bounded_and_palette_checked() {
        let grid = project_grid(&json!({
            "width": 3, "height": 2,
            "rle": [[0, 2], [1, 4]],
            "palette": [
                { "index": 0, "color": "#000000", "glyph": "." },
                { "index": 1, "color": "#ffffff", "glyph": "#" }
            ]
        }))
        .unwrap();
        assert_eq!(grid.cells, vec![0, 0, 1, 1, 1, 1]);
        assert!(
            project_grid(&json!({
                "width": 2, "height": 2,
                "cells": [0, 0, 0, 2],
                "palette": [{ "index": 0, "color": "#000", "glyph": "." }]
            }))
            .is_err()
        );
    }

    #[test]
    fn divergent_cross_source_ids_are_namespaced_instead_of_overlaid() {
        let project = document("project", "same");
        let mut execution = document("execution", "same");
        execution["entities"][0]["label"] = json!("different");
        let binding = binding();
        let field = project_field(
            "field:test",
            "Test",
            "view:test",
            &binding,
            &[
                FieldSourceInput {
                    channel: "project",
                    source_ref: "service:project",
                    subject_fingerprint: None,
                    response: Some(&project),
                    parsed: None,
                    error: None,
                    refreshing: false,
                },
                FieldSourceInput {
                    channel: "execution",
                    source_ref: "service:execution",
                    subject_fingerprint: None,
                    response: Some(&execution),
                    parsed: None,
                    error: None,
                    refreshing: false,
                },
            ],
            None,
        );
        assert_eq!(
            field
                .entities
                .iter()
                .map(|entity| entity.id.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["source:execution::same", "source:project::same"])
        );
        assert!(
            field
                .warnings
                .iter()
                .any(|warning| warning.contains("source-namespaced"))
        );
    }

    #[test]
    fn compatible_grid_compare_is_computed_in_shared_vm() {
        let mut source = document("project", "left");
        source["entities"].as_array_mut().unwrap().push(json!({
            "id": "right", "kind": "step", "label": "right", "status": "running",
            "attributes": { "rank": 3, "scope": "one" },
            "provenance": { "source_ref": "service:project", "source_revision": "revision:project", "evidence": [] }
        }));
        source["previews"] = json!([
            {
                "id": "preview:left", "entity_id": "left", "kind": "indexed_grid",
                "label": "left", "comparison_key": "board:3x1",
                "grid": {
                    "width": 3, "height": 1, "cells": [0, 0, 1],
                    "palette": [
                        { "index": 0, "color": "#000", "glyph": "." },
                        { "index": 1, "color": "#fff", "glyph": "#" }
                    ]
                }
            },
            {
                "id": "preview:right", "entity_id": "right", "kind": "indexed_grid",
                "label": "right", "comparison_key": "board:3x1",
                "grid": {
                    "width": 3, "height": 1, "cells": [0, 1, 1],
                    "palette": [
                        { "index": 0, "color": "black", "glyph": "." },
                        { "index": 1, "color": "white", "glyph": "#" }
                    ]
                }
            }
        ]);
        let local = FieldLocalState {
            compare: vec!["left".to_string(), "right".to_string()],
            ..Default::default()
        };
        let binding = binding();
        let field = project_field(
            "field:test",
            "Test",
            "view:test",
            &binding,
            &[FieldSourceInput {
                channel: "project",
                source_ref: "service:project",
                subject_fingerprint: Some("subject:project"),
                response: Some(&source),
                parsed: None,
                error: None,
                refreshing: false,
            }],
            Some(&local),
        );
        assert_eq!(field.compare, vec!["left", "right"]);
        assert!(
            field
                .entities
                .iter()
                .all(|entity| entity.preview_ids.len() == 1)
        );
        assert!(
            field
                .previews
                .iter()
                .all(|preview| preview.grid.as_ref().unwrap().changed == vec![1])
        );
    }

    #[test]
    fn deleted_source_is_an_explicit_error_state_not_an_empty_success() {
        let binding = binding();
        let field = project_field(
            "field:test",
            "Test",
            "view:test",
            &binding,
            &[FieldSourceInput {
                channel: "execution",
                source_ref: "service:execution",
                subject_fingerprint: Some("thread:deleted"),
                response: None,
                parsed: None,
                error: Some("execution chain no longer exists"),
                refreshing: false,
            }],
            None,
        );
        assert!(field.entities.is_empty());
        assert_eq!(field.sources.len(), 1);
        assert_eq!(field.sources[0].phase, RyeOsSourcePhase::Error);
        assert_eq!(
            field.sources[0].error.as_deref(),
            Some("execution chain no longer exists")
        );
    }

    #[test]
    fn deterministic_fact_mutations_fail_closed_without_panicking() {
        for index in 0..128usize {
            let mut mutated = document("project", "step:a");
            match index % 8 {
                0 => mutated["schema_version"] = json!(format!("unknown:{index}")),
                1 => {
                    let duplicate = mutated["entities"][0].clone();
                    mutated["entities"].as_array_mut().unwrap().push(duplicate);
                }
                2 => mutated["entities"][0]["attributes"] = json!(index),
                3 => mutated["entities"][0]["id"] = json!(format!("bad\n{index}")),
                4 => mutated["unexpected"] = json!(index),
                5 => mutated["cursor"] = json!({"mode": "invented", "index": index}),
                6 => {
                    mutated["expansions"] = json!([{
                        "root_id": "step:a",
                        "applied_depth": 33,
                        "entity_count": 1,
                        "truncated": false
                    }]);
                }
                _ => {
                    let mut deep = json!(index);
                    for _ in 0..34 {
                        deep = json!([deep]);
                    }
                    mutated["warnings"] = json!([deep]);
                }
            }
            assert!(
                parse_field_document(&mutated).is_err(),
                "mutation {index} was accepted"
            );
        }
    }

    #[test]
    fn cyclic_relations_remain_finite_and_reachable_in_shared_projection() {
        let mut source = document("project", "a");
        source["entities"].as_array_mut().unwrap().push(json!({
            "id": "b", "kind": "step", "label": "b", "status": "running",
            "attributes": {"rank": 2, "scope": "one"},
            "provenance": {"source_ref": "service:project", "source_revision": "revision:project", "evidence": []}
        }));
        source["relations"] = json!([
            {
                "id": "a-b", "kind": "flows_to", "source_id": "a", "target_id": "b",
                "directed": true, "attributes": {},
                "provenance": {"source_ref": "service:project", "source_revision": "revision:project", "evidence": []}
            },
            {
                "id": "b-a", "kind": "flows_to", "source_id": "b", "target_id": "a",
                "directed": true, "attributes": {},
                "provenance": {"source_ref": "service:project", "source_revision": "revision:project", "evidence": []}
            }
        ]);
        let binding = binding();
        let field = project_field(
            "field:test",
            "Test",
            "view:test",
            &binding,
            &[FieldSourceInput {
                channel: "project",
                source_ref: "service:project",
                subject_fingerprint: None,
                response: Some(&source),
                parsed: None,
                error: None,
                refreshing: false,
            }],
            None,
        );
        assert_eq!(field.traversal, vec!["a", "b"]);
        assert_eq!(field.relations.len(), 2);
    }
}
