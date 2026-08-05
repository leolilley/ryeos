//! Renderer-independent source contract for living execution fields.
//!
//! Sources publish bounded substrate facts. Signed view content owns the
//! project/domain interpretation, while shared clients own projection and UI
//! state. Keep this module free of project-specific nouns and presentation.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const FIELD_FACTS_SCHEMA: &str = "ryeos.ui.field.facts.v2";
pub const MAX_FIELD_FACT_ENTITIES: usize = 5_000;
pub const MAX_FIELD_FACT_RELATIONS: usize = 12_000;
pub const MAX_FIELD_FACT_ATTRIBUTE_BYTES: usize = 256 * 1024;
pub const MAX_FIELD_FACT_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_FIELD_EXPANSIONS: usize = 16;
pub const MAX_EXPANSION_DEPTH: u16 = 32;
pub const MAX_EXPANSION_ENTITIES: u32 = 1_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldFactSubject {
    pub kind: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_definition_digest: Option<String>,
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
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum FieldCursorRequest {
    Live,
    BraidCut { anchor: FieldEventRef },
}

impl Default for FieldCursorRequest {
    fn default() -> Self {
        Self::Live
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldExpansionRequest {
    pub root_id: String,
    pub max_depth: u16,
    pub max_entities: u32,
    #[serde(default)]
    pub continuation_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldExpansionResult {
    pub root_id: String,
    pub applied_depth: u16,
    pub entity_count: u32,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldExpansionTokenClaims {
    schema_version: String,
    service_ref: String,
    subject_fingerprint: String,
    cursor_fingerprint: String,
    root_id: String,
    max_depth: u16,
    max_entities: u32,
    base_revision: String,
    prior_response_revision: String,
    next_offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldReplay {
    pub capability: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<FieldEventRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<FieldEventRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_head: Option<FieldEventRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldAnchorConformance {
    ContractV2,
    Malformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldManifestVerification {
    NotProvided,
    NotChecked,
    Verified,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldEventRef {
    pub chain_root_id: String,
    pub chain_seq: u64,
    pub event_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldArtifactRef {
    pub thread_id: String,
    pub artifact_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FieldEvidenceRef {
    Item {
        canonical_ref: String,
        source_content_digest: String,
    },
    Event {
        event: FieldEventRef,
    },
    Artifact {
        thread_id: String,
        artifact_id: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        content_hash: Option<String>,
    },
    Thread {
        thread_id: String,
    },
    AdmittedLaunchCapsule {
        content_hash: String,
    },
    HookObservation {
        observation_id: String,
        response_hash: String,
        occurrence: ryeos_runtime::callback::HookDispatchOccurrence,
        event: FieldEventRef,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldProvenance {
    pub source_ref: String,
    pub source_revision: String,
    pub evidence: Vec<FieldEvidenceRef>,
}

impl FieldProvenance {
    pub fn pending(source_ref: impl Into<String>, evidence: Vec<FieldEvidenceRef>) -> Self {
        Self {
            source_ref: source_ref.into(),
            source_revision: String::new(),
            evidence,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldFactEntity {
    pub id: String,
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_content_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_definition_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admitted_launch_capsule_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_ref: Option<FieldEventRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<FieldArtifactRef>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay: Option<FieldReplay>,
    pub truncated: bool,
    pub entities: Vec<FieldFactEntity>,
    pub relations: Vec<FieldFactRelation>,
    pub previews: Vec<Value>,
    pub metrics: Vec<Value>,
    pub expansions: Vec<Value>,
    pub warnings: Vec<Value>,
    /// Full source-owned candidate set retained only until the handler has
    /// applied expansion requests. These fields never cross the wire; the
    /// bounded public vectors above remain the complete v1 schema.
    #[serde(skip)]
    expansion_entities: Vec<FieldFactEntity>,
    #[serde(skip)]
    expansion_relations: Vec<FieldFactRelation>,
}

pub struct FieldFactsBuilder {
    source: String,
    source_ref: String,
    subject: FieldFactSubject,
    entities: BTreeMap<String, FieldFactEntity>,
    relations: BTreeMap<String, FieldFactRelation>,
    warnings: Vec<Value>,
    truncated: bool,
    cursor: FieldCursor,
    replay: Option<FieldReplay>,
    previews: Vec<Value>,
    metrics: Vec<Value>,
    expansions: Vec<Value>,
}

impl FieldFactsBuilder {
    pub fn new(
        source: impl Into<String>,
        source_ref: impl Into<String>,
        subject: FieldFactSubject,
    ) -> Self {
        Self {
            source: source.into(),
            source_ref: source_ref.into(),
            subject,
            entities: BTreeMap::new(),
            relations: BTreeMap::new(),
            warnings: Vec::new(),
            truncated: false,
            cursor: FieldCursor::Live,
            replay: None,
            previews: Vec::new(),
            metrics: Vec::new(),
            expansions: Vec::new(),
        }
    }

    pub fn provenance(&self, evidence: Vec<FieldEvidenceRef>) -> FieldProvenance {
        FieldProvenance::pending(self.source_ref.clone(), evidence)
    }

    pub fn add_entity(&mut self, entity: FieldFactEntity) -> Result<()> {
        validate_stable_id("field entity", &entity.id)?;
        validate_attributes("field entity attributes", &entity.attributes)?;
        if let Some(existing) = self.entities.get(&entity.id) {
            if existing != &entity {
                bail!("field entity `{}` has divergent duplicate facts", entity.id);
            }
            return Ok(());
        }
        self.entities.insert(entity.id.clone(), entity);
        Ok(())
    }

    pub fn add_relation(&mut self, relation: FieldFactRelation) -> Result<()> {
        validate_stable_id("field relation", &relation.id)?;
        validate_attributes("field relation attributes", &relation.attributes)?;
        if let Some(existing) = self.relations.get(&relation.id) {
            if existing != &relation {
                bail!(
                    "field relation `{}` has divergent duplicate facts",
                    relation.id
                );
            }
            return Ok(());
        }
        self.relations.insert(relation.id.clone(), relation);
        Ok(())
    }

    pub fn warn(&mut self, code: &str, message: impl Into<String>) {
        self.warnings.push(serde_json::json!({
            "code": code,
            "message": message.into(),
        }));
    }

    pub fn mark_truncated(&mut self) {
        self.truncated = true;
    }

    pub fn set_cursor(&mut self, cursor: FieldCursor, replay: Option<FieldReplay>) {
        self.cursor = cursor;
        self.replay = replay;
    }

    pub fn add_preview(&mut self, preview: Value) -> Result<()> {
        let bytes = lillux::canonical_json(&preview)
            .context("canonicalize field preview")?
            .len();
        if bytes > 1024 * 1024 {
            bail!("field preview exceeds the 1 MiB inline limit");
        }
        self.previews.push(preview);
        Ok(())
    }

    pub fn add_metric(&mut self, metric: Value) {
        self.metrics.push(metric);
    }

    pub fn set_expansions(&mut self, expansions: Vec<FieldExpansionResult>) -> Result<()> {
        if expansions.len() > MAX_FIELD_EXPANSIONS {
            bail!("field source has too many expansion results");
        }
        self.expansions = expansions
            .into_iter()
            .map(serde_json::to_value)
            .collect::<std::result::Result<_, _>>()
            .context("serialize field expansion results")?;
        Ok(())
    }

    pub fn finish(self) -> Result<FieldFactsDocument> {
        let Self {
            source,
            source_ref: _,
            subject,
            entities,
            relations,
            mut warnings,
            mut truncated,
            cursor,
            replay,
            previews,
            metrics,
            expansions,
        } = self;
        // Consume the sorted maps into the hidden expansion reservoir once.
        // The public base vectors are bounded clones; non-expanding clients no
        // longer pay for cloning the full, potentially multi-MiB reservoir.
        let expansion_entities = entities.into_values().collect::<Vec<_>>();
        let expansion_relations = relations.into_values().collect::<Vec<_>>();
        let expansion_reservoir_revision =
            expansion_reservoir_revision(&expansion_entities, &expansion_relations)?;
        if expansion_entities.len() > MAX_FIELD_FACT_ENTITIES {
            truncated = true;
            warnings.push(serde_json::json!({
                "code": "entity_limit",
                "message": format!("field source exceeded {MAX_FIELD_FACT_ENTITIES} entities"),
            }));
        }
        let entities = expansion_entities
            .iter()
            .take(MAX_FIELD_FACT_ENTITIES)
            .cloned()
            .collect::<Vec<_>>();
        // Named sources intentionally cross-link: a run summary may point at a
        // definition entity owned by the project or historical-definition
        // channel. Preserve such relations and let the shared merge layer
        // resolve the endpoint (or surface a missing-endpoint warning).
        let relation_count = expansion_relations.len();
        let relations = expansion_relations
            .iter()
            .take(MAX_FIELD_FACT_RELATIONS)
            .cloned()
            .collect::<Vec<_>>();
        if relations.len() != relation_count {
            truncated = true;
            warnings.push(serde_json::json!({
                "code": "relation_limit",
                "message": format!("field source exceeded {MAX_FIELD_FACT_RELATIONS} relations"),
            }));
        }

        let mut entities = entities;
        let mut relations = relations;
        let mut document_limit_warned = false;
        loop {
            let revision_input = serde_json::json!({
                "schema_version": FIELD_FACTS_SCHEMA,
                "source": &source,
                "subject": &subject,
                "entities": &entities,
                "relations": &relations,
                "cursor": &cursor,
                "replay": &replay,
                "truncated": truncated,
                "warnings": &warnings,
                "previews": &previews,
                "metrics": &metrics,
                "expansions": &expansions,
                "expansion_reservoir_revision": &expansion_reservoir_revision,
            });
            let canonical = lillux::canonical_json(&revision_input)
                .context("canonicalize field source revision")?;
            let revision = lillux::sha256_hex(canonical.as_bytes());
            let mut stamped_entities = entities.clone();
            let mut stamped_relations = relations.clone();
            for entity in &mut stamped_entities {
                entity.provenance.source_revision.clone_from(&revision);
            }
            for relation in &mut stamped_relations {
                relation.provenance.source_revision.clone_from(&revision);
            }
            let mut document = FieldFactsDocument {
                schema_version: FIELD_FACTS_SCHEMA.to_string(),
                source: source.clone(),
                subject: subject.clone(),
                revision,
                cursor: cursor.clone(),
                replay: replay.clone(),
                truncated,
                entities: stamped_entities,
                relations: stamped_relations,
                previews: previews.clone(),
                metrics: metrics.clone(),
                expansions: expansions.clone(),
                warnings: warnings.clone(),
                // These fields are serde-skipped and do not participate in
                // the wire-size check. Move the reservoir into the one final
                // document only after the bounded public shape fits.
                expansion_entities: Vec::new(),
                expansion_relations: Vec::new(),
            };
            let document_value = serde_json::to_value(&document)
                .context("serialize bounded field facts document")?;
            let bytes = lillux::canonical_json(&document_value)
                .context("canonicalize bounded field facts document")?
                .len();
            if bytes <= MAX_FIELD_FACT_DOCUMENT_BYTES {
                document.expansion_entities = expansion_entities;
                document.expansion_relations = expansion_relations;
                return Ok(document);
            }

            truncated = true;
            if !document_limit_warned {
                warnings.push(serde_json::json!({
                    "code": "document_limit",
                    "message": format!(
                        "field source was deterministically trimmed to {MAX_FIELD_FACT_DOCUMENT_BYTES} bytes"
                    ),
                }));
                document_limit_warned = true;
                continue;
            }
            let excess = bytes - MAX_FIELD_FACT_DOCUMENT_BYTES;
            if !relations.is_empty() {
                let remove = relations
                    .len()
                    .saturating_mul(excess)
                    .div_ceil(bytes)
                    .max(1)
                    .min(relations.len());
                relations.truncate(relations.len() - remove);
            } else if !entities.is_empty() {
                let remove = entities
                    .len()
                    .saturating_mul(excess)
                    .div_ceil(bytes)
                    .max(1)
                    .min(entities.len());
                entities.truncate(entities.len() - remove);
            } else {
                bail!(
                    "field facts response chrome is {bytes} bytes (max {MAX_FIELD_FACT_DOCUMENT_BYTES})"
                );
            }
        }
    }
}

fn expansion_reservoir_revision(
    entities: &[FieldFactEntity],
    relations: &[FieldFactRelation],
) -> Result<String> {
    // Stream the exact canonical JSON object formerly materialized as one
    // large Value/String. This preserves revision identity byte-for-byte while
    // bounding peak memory to one fact's canonical form.
    let mut hasher = Sha256::new();
    hasher.update(b"{\"entities\":[");
    for (index, entity) in entities.iter().enumerate() {
        if index > 0 {
            hasher.update(b",");
        }
        let value = serde_json::to_value(entity).context("serialize field expansion entity")?;
        let canonical =
            lillux::canonical_json(&value).context("canonicalize field expansion entity")?;
        hasher.update(canonical.as_bytes());
    }
    hasher.update(b"],\"relations\":[");
    for (index, relation) in relations.iter().enumerate() {
        if index > 0 {
            hasher.update(b",");
        }
        let value = serde_json::to_value(relation).context("serialize field expansion relation")?;
        let canonical =
            lillux::canonical_json(&value).context("canonicalize field expansion relation")?;
        hasher.update(canonical.as_bytes());
    }
    hasher.update(b"]}");
    Ok(format!("{:x}", hasher.finalize()))
}

/// Apply bounded, deterministic neighborhood expansion metadata to a source
/// document. Continuation tokens are authenticated by daemon-owned UI state
/// and bind the exact service, subject, cursor, root, bounds, base facts, and
/// preceding response revision. Renderers never inspect or manufacture them.
pub fn apply_bounded_expansions(
    mut document: FieldFactsDocument,
    requests: &[FieldExpansionRequest],
    ui_state: &crate::state::UiState,
    service_ref: &str,
) -> Result<FieldFactsDocument> {
    if requests.len() > MAX_FIELD_EXPANSIONS {
        bail!("field expansion request exceeds {MAX_FIELD_EXPANSIONS} roots");
    }
    let mut roots = BTreeSet::new();
    let entity_ids = document
        .entities
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<BTreeSet<_>>();
    let base_revision = document.revision.clone();
    let subject_value =
        serde_json::to_value(&document.subject).context("serialize field expansion subject")?;
    let subject_fingerprint = lillux::sha256_hex(
        lillux::canonical_json(&subject_value)
            .context("canonicalize field expansion subject")?
            .as_bytes(),
    );
    let cursor_value =
        serde_json::to_value(&document.cursor).context("serialize field expansion cursor")?;
    let cursor_fingerprint = lillux::sha256_hex(
        lillux::canonical_json(&cursor_value)
            .context("canonicalize field expansion cursor")?
            .as_bytes(),
    );
    let adjacency = expansion_adjacency(&document);
    let mut pending = Vec::new();
    for request in requests {
        validate_stable_id("field expansion root", &request.root_id)?;
        if !roots.insert(request.root_id.clone()) {
            bail!("field expansion request repeats root `{}`", request.root_id);
        }
        if !entity_ids.contains(&request.root_id) {
            bail!(
                "field expansion root `{}` is outside the source subject",
                request.root_id
            );
        }
        let max_depth = request.max_depth.clamp(1, MAX_EXPANSION_DEPTH);
        let max_entities = request.max_entities.clamp(1, MAX_EXPANSION_ENTITIES);
        let start = if let Some(token) = request.continuation_token.as_deref() {
            let claims = decode_expansion_token(ui_state, token)?;
            if claims.service_ref != service_ref
                || claims.subject_fingerprint != subject_fingerprint
                || claims.cursor_fingerprint != cursor_fingerprint
                || claims.root_id != request.root_id
                || claims.max_depth != max_depth
                || claims.max_entities != max_entities
                || claims.base_revision != base_revision
            {
                bail!("field expansion continuation token is stale or belongs to another subject");
            }
            claims.next_offset
        } else {
            0
        };
        let traversal = expansion_traversal(&request.root_id, max_depth, &adjacency);
        let start_usize = usize::try_from(start).context("field expansion offset overflow")?;
        if start_usize > traversal.len() {
            bail!("field expansion continuation token offset is outside the closure");
        }
        let end = start_usize
            .saturating_add(max_entities as usize)
            .min(traversal.len());
        let applied_depth = traversal[start_usize..end]
            .iter()
            .map(|(_, depth)| *depth)
            .max()
            .unwrap_or_default();
        pending.push((
            request,
            FieldExpansionResult {
                root_id: request.root_id.clone(),
                applied_depth,
                entity_count: u32::try_from(end).unwrap_or(u32::MAX),
                truncated: end < traversal.len(),
                continuation_token: None,
            },
            u32::try_from(end).unwrap_or(u32::MAX),
            start,
        ));
    }
    materialize_expansion_facts(&mut document, &mut pending, &adjacency)?;
    let revision_projection = pending
        .iter()
        .map(|(_, result, _, _)| result)
        .collect::<Vec<_>>();
    let response_revision = lillux::sha256_hex(
        lillux::canonical_json(&serde_json::json!({
            "base_revision": base_revision,
            "expansions": revision_projection,
        }))
        .context("canonicalize expanded field revision")?
        .as_bytes(),
    );
    let mut results = Vec::new();
    let mut stalled = false;
    for (request, mut result, next_offset, start_offset) in pending {
        if result.truncated && next_offset > start_offset {
            result.continuation_token = Some(encode_expansion_token(
                ui_state,
                &FieldExpansionTokenClaims {
                    schema_version: "ryeos.ui.field.expansion-token.v1".to_string(),
                    service_ref: service_ref.to_string(),
                    subject_fingerprint: subject_fingerprint.clone(),
                    cursor_fingerprint: cursor_fingerprint.clone(),
                    root_id: request.root_id.clone(),
                    max_depth: request.max_depth.clamp(1, MAX_EXPANSION_DEPTH),
                    max_entities: request.max_entities.clamp(1, MAX_EXPANSION_ENTITIES),
                    base_revision: document.revision.clone(),
                    prior_response_revision: response_revision.clone(),
                    next_offset,
                },
            )?);
        } else if result.truncated {
            // Shared entity/wire bounds can prevent one of several expansion
            // roots from advancing. Emitting the unchanged (or regressed)
            // offset would let a well-behaved client loop forever.
            stalled = true;
        }
        results.push(result);
    }
    if stalled {
        document.warnings.push(serde_json::json!({
            "code": "expansion_no_progress",
            "message": "bounded expansion could not advance this root under the shared response limit",
        }));
    }
    document.revision = response_revision.clone();
    document.expansions = results
        .into_iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<_, _>>()
        .context("serialize bounded field expansions")?;
    for entity in &mut document.entities {
        entity
            .provenance
            .source_revision
            .clone_from(&response_revision);
    }
    for relation in &mut document.relations {
        relation
            .provenance
            .source_revision
            .clone_from(&response_revision);
    }
    Ok(document)
}

fn expansion_adjacency(document: &FieldFactsDocument) -> BTreeMap<String, BTreeSet<String>> {
    let entities = if document.expansion_entities.is_empty() {
        &document.entities
    } else {
        &document.expansion_entities
    };
    let relations = if document.expansion_relations.is_empty() {
        &document.relations
    } else {
        &document.expansion_relations
    };
    let mut adjacency = entities
        .iter()
        .map(|entity| (entity.id.clone(), BTreeSet::new()))
        .collect::<BTreeMap<_, _>>();
    for relation in relations {
        if adjacency.contains_key(&relation.source_id)
            && adjacency.contains_key(&relation.target_id)
        {
            adjacency
                .entry(relation.source_id.clone())
                .or_default()
                .insert(relation.target_id.clone());
            adjacency
                .entry(relation.target_id.clone())
                .or_default()
                .insert(relation.source_id.clone());
        }
    }
    adjacency
}

fn materialize_expansion_facts(
    document: &mut FieldFactsDocument,
    pending: &mut [(&FieldExpansionRequest, FieldExpansionResult, u32, u32)],
    adjacency: &BTreeMap<String, BTreeSet<String>>,
) -> Result<()> {
    let reservoir_entities = if document.expansion_entities.is_empty() {
        document.entities.clone()
    } else {
        document.expansion_entities.clone()
    };
    let reservoir_relations = if document.expansion_relations.is_empty() {
        document.relations.clone()
    } else {
        document.expansion_relations.clone()
    };
    let entity_by_id = reservoir_entities
        .iter()
        .map(|entity| (entity.id.clone(), entity.clone()))
        .collect::<BTreeMap<_, _>>();
    let reservoir_ids = entity_by_id.keys().cloned().collect::<BTreeSet<_>>();

    // Expansion facts take priority over unrelated base facts at the fixed
    // wire cap. This is what makes an expansion able to page into the source's
    // retained candidate set even when the ordinary base document is full.
    let mut desired = BTreeSet::new();
    for (request, result, next_offset, _) in pending.iter_mut() {
        let traversal = expansion_traversal(
            &request.root_id,
            request.max_depth.clamp(1, MAX_EXPANSION_DEPTH),
            adjacency,
        );
        let requested_end = usize::try_from(*next_offset)
            .unwrap_or(usize::MAX)
            .min(traversal.len());
        let mut accepted_end = 0usize;
        for (index, (id, _)) in traversal.iter().take(requested_end).enumerate() {
            if desired.contains(id) || desired.len() < MAX_FIELD_FACT_ENTITIES {
                desired.insert(id.clone());
                accepted_end = index + 1;
            } else {
                break;
            }
        }
        *next_offset = u32::try_from(accepted_end).unwrap_or(u32::MAX);
        result.entity_count = *next_offset;
        result.applied_depth = traversal[..accepted_end]
            .iter()
            .map(|(_, depth)| *depth)
            .max()
            .unwrap_or_default();
        result.truncated = accepted_end < traversal.len();
    }

    let original_ids = document
        .entities
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<BTreeSet<_>>();
    let mut final_ids = desired.clone();
    for entity in &document.entities {
        if final_ids.len() >= MAX_FIELD_FACT_ENTITIES {
            break;
        }
        final_ids.insert(entity.id.clone());
    }
    document.entities = final_ids
        .iter()
        .filter_map(|id| entity_by_id.get(id).cloned())
        .collect();

    let mut relation_by_id = document
        .relations
        .iter()
        .cloned()
        .map(|relation| (relation.id.clone(), relation))
        .collect::<BTreeMap<_, _>>();
    for relation in reservoir_relations {
        if final_ids.contains(&relation.source_id) && final_ids.contains(&relation.target_id) {
            relation_by_id
                .entry(relation.id.clone())
                .or_insert(relation);
        }
    }
    document.relations = relation_by_id
        .into_values()
        .filter(|relation| {
            (!reservoir_ids.contains(&relation.source_id)
                || final_ids.contains(&relation.source_id))
                && (!reservoir_ids.contains(&relation.target_id)
                    || final_ids.contains(&relation.target_id))
        })
        .take(MAX_FIELD_FACT_RELATIONS)
        .collect();

    let base_trimmed = original_ids.iter().any(|id| !final_ids.contains(id));
    // Leave room for continuation tokens and the final expansion metadata.
    let target_bytes = MAX_FIELD_FACT_DOCUMENT_BYTES.saturating_sub(64 * 1024);
    let wire_trimmed = trim_expanded_document_to_bytes(document, target_bytes, &desired)?;
    let retained_ids = document
        .entities
        .iter()
        .map(|entity| entity.id.clone())
        .collect::<BTreeSet<_>>();
    document.relations.retain(|relation| {
        (!reservoir_ids.contains(&relation.source_id) || retained_ids.contains(&relation.source_id))
            && (!reservoir_ids.contains(&relation.target_id)
                || retained_ids.contains(&relation.target_id))
    });
    for (request, result, next_offset, _) in pending.iter_mut() {
        let traversal = expansion_traversal(
            &request.root_id,
            request.max_depth.clamp(1, MAX_EXPANSION_DEPTH),
            adjacency,
        );
        let requested_end = usize::try_from(*next_offset)
            .unwrap_or(usize::MAX)
            .min(traversal.len());
        let accepted_end = traversal
            .iter()
            .take(requested_end)
            .take_while(|(id, _)| retained_ids.contains(id))
            .count();
        *next_offset = u32::try_from(accepted_end).unwrap_or(u32::MAX);
        result.entity_count = *next_offset;
        result.applied_depth = traversal[..accepted_end]
            .iter()
            .map(|(_, depth)| *depth)
            .max()
            .unwrap_or_default();
        result.truncated = accepted_end < traversal.len();
    }
    if base_trimmed || wire_trimmed {
        document.truncated = true;
        document.warnings.push(serde_json::json!({
            "code": "expansion_bound",
            "message": "bounded expansion prioritized requested neighborhood facts at the fixed field wire limit",
        }));
    }
    Ok(())
}

fn serialized_field_document_bytes(document: &FieldFactsDocument) -> Result<usize> {
    let value = serde_json::to_value(document).context("serialize expanded field document")?;
    Ok(lillux::canonical_json(&value)
        .context("canonicalize expanded field document")?
        .len())
}

fn trim_expanded_document_to_bytes(
    document: &mut FieldFactsDocument,
    target_bytes: usize,
    desired: &BTreeSet<String>,
) -> Result<bool> {
    if serialized_field_document_bytes(document)? <= target_bytes {
        return Ok(false);
    }

    // Preserve the prior deterministic policy exactly: remove relations from
    // the end first. Binary-search the largest retained prefix so a response
    // with thousands of relations needs O(log n), not O(n), full-document
    // canonicalizations.
    let relations = document.relations.clone();
    let mut low = 0usize;
    let mut high = relations.len();
    while low < high {
        let keep = low + (high - low).div_ceil(2);
        document.relations = relations[..keep].to_vec();
        if serialized_field_document_bytes(document)? <= target_bytes {
            low = keep;
        } else {
            high = keep - 1;
        }
    }
    document.relations = relations[..low].to_vec();
    if serialized_field_document_bytes(document)? <= target_bytes {
        return Ok(true);
    }

    // The old loop reached entities only after every relation was removed. It
    // removed the last non-requested entity repeatedly, then popped requested
    // entities from the end. Encode that exact removal order once and binary
    // search the minimum removed prefix.
    let entities = document.entities.clone();
    let mut removal_order = entities
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(index, entity)| (!desired.contains(&entity.id)).then_some(index))
        .collect::<Vec<_>>();
    removal_order.extend(
        entities
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(index, entity)| desired.contains(&entity.id).then_some(index)),
    );
    let mut removal_rank = vec![usize::MAX; entities.len()];
    for (rank, index) in removal_order.into_iter().enumerate() {
        removal_rank[index] = rank;
    }
    let set_removed = |document: &mut FieldFactsDocument, count: usize| {
        document.entities = entities
            .iter()
            .enumerate()
            .filter(|(index, _)| removal_rank[*index] >= count)
            .map(|(_, entity)| entity.clone())
            .collect();
    };

    let mut low = 0usize;
    let mut high = entities.len();
    while low < high {
        let removed = low + (high - low) / 2;
        set_removed(document, removed);
        if serialized_field_document_bytes(document)? <= target_bytes {
            high = removed;
        } else {
            low = removed + 1;
        }
    }
    set_removed(document, low);
    if serialized_field_document_bytes(document)? > target_bytes {
        bail!("expanded field response chrome exceeds the document byte limit");
    }
    Ok(true)
}

fn expansion_traversal(
    root_id: &str,
    max_depth: u16,
    adjacency: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<(String, u16)> {
    let mut seen = BTreeSet::from([root_id.to_string()]);
    let mut frontier = vec![(root_id.to_string(), 0u16)];
    let mut traversal = Vec::new();
    let mut index = 0;
    while index < frontier.len() {
        let (id, depth) = frontier[index].clone();
        index += 1;
        traversal.push((id.clone(), depth));
        if depth >= max_depth {
            continue;
        }
        for neighbor in adjacency.get(&id).into_iter().flatten() {
            if seen.insert(neighbor.clone()) {
                frontier.push((neighbor.clone(), depth + 1));
            }
        }
    }
    traversal
}

fn encode_expansion_token(
    ui_state: &crate::state::UiState,
    claims: &FieldExpansionTokenClaims,
) -> Result<String> {
    let claims_value =
        serde_json::to_value(claims).context("serialize field expansion token claims")?;
    let payload =
        lillux::canonical_json(&claims_value).context("canonicalize field expansion token")?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
    let mac = ui_state.sign_field_token(encoded.as_bytes());
    Ok(format!("{encoded}.{mac}"))
}

fn decode_expansion_token(
    ui_state: &crate::state::UiState,
    token: &str,
) -> Result<FieldExpansionTokenClaims> {
    let (encoded, mac) = token
        .split_once('.')
        .context("field expansion continuation token is malformed")?;
    if !ui_state.verify_field_token_mac(encoded.as_bytes(), mac) {
        bail!("field expansion continuation token signature is invalid");
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .context("decode field expansion continuation token")?;
    let claims: FieldExpansionTokenClaims =
        serde_json::from_slice(&payload).context("parse field expansion continuation token")?;
    if claims.schema_version != "ryeos.ui.field.expansion-token.v1" {
        bail!("unsupported field expansion continuation token schema");
    }
    Ok(claims)
}

fn validate_stable_id(label: &str, id: &str) -> Result<()> {
    if id.trim() != id || id.is_empty() || id.len() > 1024 || id.chars().any(char::is_control) {
        bail!("{label} has an invalid stable ID");
    }
    Ok(())
}

fn validate_attributes(label: &str, attributes: &Value) -> Result<()> {
    if !attributes.is_object() {
        bail!("{label} must be an object");
    }
    let bytes = lillux::canonical_json(attributes)
        .with_context(|| format!("canonicalize {label}"))?
        .len();
    if bytes > MAX_FIELD_FACT_ATTRIBUTE_BYTES {
        bail!("{label} is {bytes} bytes (max {MAX_FIELD_FACT_ATTRIBUTE_BYTES})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(id: &str, label: &str) -> FieldFactEntity {
        FieldFactEntity {
            id: id.to_string(),
            kind: "item".to_string(),
            label: label.to_string(),
            parent_id: None,
            status: None,
            canonical_ref: None,
            source_content_digest: None,
            effective_definition_digest: None,
            admitted_launch_capsule_hash: None,
            event_ref: None,
            artifact_ref: None,
            attributes: serde_json::json!({}),
            provenance: FieldProvenance::pending("service:test", Vec::new()),
        }
    }

    #[test]
    fn facts_are_stably_sorted_revisioned_and_collision_closed() {
        let subject = FieldFactSubject {
            kind: "project".to_string(),
            id: "project:test".to_string(),
            definition_ref: None,
            effective_definition_digest: None,
        };
        let mut left = FieldFactsBuilder::new("project", "service:test", subject.clone());
        left.add_entity(entity("item:z", "z")).unwrap();
        left.add_entity(entity("item:a", "a")).unwrap();
        let left = left.finish().unwrap();

        let mut right = FieldFactsBuilder::new("project", "service:test", subject);
        right.add_entity(entity("item:a", "a")).unwrap();
        right.add_entity(entity("item:z", "z")).unwrap();
        let right = right.finish().unwrap();
        assert_eq!(left, right);
        assert_eq!(left.entities[0].id, "item:a");
        assert!(
            left.entities
                .iter()
                .all(|entity| entity.provenance.source_revision == left.revision)
        );

        let mut divergent = FieldFactsBuilder::new(
            "project",
            "service:test",
            FieldFactSubject {
                kind: "project".to_string(),
                id: "project:test".to_string(),
                definition_ref: None,
                effective_definition_digest: None,
            },
        );
        divergent.add_entity(entity("item:a", "a")).unwrap();
        assert!(divergent.add_entity(entity("item:a", "changed")).is_err());
    }

    #[test]
    fn streamed_expansion_reservoir_revision_matches_canonical_document_identity() {
        let entities = vec![entity("item:a", "a"), entity("item:b", "b")];
        let relations = vec![FieldFactRelation {
            id: "edge:a-b".to_string(),
            kind: "contains".to_string(),
            source_id: "item:a".to_string(),
            target_id: "item:b".to_string(),
            status: None,
            directed: true,
            attributes: serde_json::json!({"rank": 1}),
            provenance: FieldProvenance::pending("service:test", Vec::new()),
        }];
        let expected = lillux::sha256_hex(
            lillux::canonical_json(&serde_json::json!({
                "entities": &entities,
                "relations": &relations,
            }))
            .unwrap()
            .as_bytes(),
        );

        assert_eq!(
            expansion_reservoir_revision(&entities, &relations).unwrap(),
            expected
        );
    }

    #[test]
    fn cursor_participates_in_revision_identity() {
        let subject = FieldFactSubject {
            kind: "thread".to_string(),
            id: "T-test".to_string(),
            definition_ref: None,
            effective_definition_digest: None,
        };
        let live = FieldFactsBuilder::new("execution", "service:test", subject.clone())
            .finish()
            .unwrap();
        let anchor = FieldEventRef {
            chain_root_id: "T-test".to_string(),
            chain_seq: 4,
            event_hash: "a".repeat(64),
        };
        let mut cut = FieldFactsBuilder::new("execution", "service:test", subject);
        cut.set_cursor(
            FieldCursor::BraidCut {
                anchor: anchor.clone(),
                through_chain_seq: 4,
                outside_cut: Vec::new(),
            },
            Some(FieldReplay {
                capability: "adjacent_event_refs".to_string(),
                previous: None,
                next: None,
                live_head: Some(anchor),
            }),
        );
        let cut = cut.finish().unwrap();
        assert_ne!(live.revision, cut.revision);
    }

    #[test]
    fn oversized_document_is_deterministically_trimmed() {
        let subject = FieldFactSubject {
            kind: "project".to_string(),
            id: "project:test".to_string(),
            definition_ref: None,
            effective_definition_digest: None,
        };
        let build = || {
            let mut builder = FieldFactsBuilder::new("project", "service:test", subject.clone());
            for index in 0..64 {
                let mut fact = entity(&format!("item:{index:03}"), "large");
                fact.attributes = serde_json::json!({"payload": "x".repeat(80 * 1024)});
                builder.add_entity(fact).unwrap();
            }
            builder.finish().unwrap()
        };
        let left = build();
        let right = build();
        assert_eq!(left, right);
        assert!(left.truncated);
        assert!(left.entities.len() < 64);
        assert!(left.warnings.iter().any(|warning| {
            warning.get("code").and_then(Value::as_str) == Some("document_limit")
        }));
        assert!(
            lillux::canonical_json(&serde_json::to_value(left).unwrap())
                .unwrap()
                .len()
                <= MAX_FIELD_FACT_DOCUMENT_BYTES
        );
    }

    #[test]
    fn expanded_wire_trim_preserves_relation_and_requested_entity_priority() {
        let subject = FieldFactSubject {
            kind: "project".to_string(),
            id: "project:test".to_string(),
            definition_ref: None,
            effective_definition_digest: None,
        };
        let mut builder = FieldFactsBuilder::new("project", "service:test", subject);
        for index in 0..4 {
            let mut fact = entity(&format!("item:{index}"), &format!("item {index}"));
            fact.attributes = serde_json::json!({"payload": "x".repeat(1024)});
            builder.add_entity(fact).unwrap();
            builder
                .add_relation(FieldFactRelation {
                    id: format!("edge:{index}"),
                    kind: "contains".to_string(),
                    source_id: "item:0".to_string(),
                    target_id: format!("item:{index}"),
                    status: None,
                    directed: true,
                    attributes: serde_json::json!({"payload": "r".repeat(256)}),
                    provenance: FieldProvenance::pending("service:test", Vec::new()),
                })
                .unwrap();
        }
        let document = builder.finish().unwrap();

        let mut expected_relations = document.clone();
        expected_relations.relations.truncate(2);
        let relation_target = serialized_field_document_bytes(&expected_relations).unwrap();
        let mut relation_trimmed = document.clone();
        assert!(
            trim_expanded_document_to_bytes(
                &mut relation_trimmed,
                relation_target,
                &BTreeSet::new(),
            )
            .unwrap()
        );
        assert_eq!(relation_trimmed.relations, expected_relations.relations);
        assert_eq!(relation_trimmed.entities, expected_relations.entities);

        let mut entity_source = document;
        entity_source.relations.clear();
        let desired = BTreeSet::from(["item:3".to_string()]);
        let mut expected_entities = entity_source.clone();
        expected_entities.entities.remove(2);
        let entity_target = serialized_field_document_bytes(&expected_entities).unwrap();
        assert!(
            trim_expanded_document_to_bytes(&mut entity_source, entity_target, &desired).unwrap()
        );
        assert_eq!(entity_source.entities, expected_entities.entities);
    }

    #[test]
    fn expansion_tokens_are_deterministic_bounded_and_fail_closed() {
        let subject = FieldFactSubject {
            kind: "project".to_string(),
            id: "project:test".to_string(),
            definition_ref: None,
            effective_definition_digest: None,
        };
        let mut builder = FieldFactsBuilder::new("project", "service:test", subject);
        for index in 0..6 {
            builder
                .add_entity(entity(&format!("item:{index}"), &format!("item {index}")))
                .unwrap();
            if index > 0 {
                builder
                    .add_relation(FieldFactRelation {
                        id: format!("edge:{}-{index}", index - 1),
                        kind: "contains".to_string(),
                        source_id: format!("item:{}", index - 1),
                        target_id: format!("item:{index}"),
                        status: None,
                        directed: true,
                        attributes: serde_json::json!({}),
                        provenance: FieldProvenance::pending("service:test", Vec::new()),
                    })
                    .unwrap();
            }
        }
        let base = builder.finish().unwrap();
        let ui_state = crate::state::UiState::new();
        let request = FieldExpansionRequest {
            root_id: "item:0".to_string(),
            max_depth: 10,
            max_entities: 2,
            continuation_token: None,
        };
        let first = apply_bounded_expansions(
            base.clone(),
            std::slice::from_ref(&request),
            &ui_state,
            "service:test",
        )
        .unwrap();
        let repeated =
            apply_bounded_expansions(base.clone(), &[request], &ui_state, "service:test").unwrap();
        assert_eq!(first, repeated);
        let first_result: FieldExpansionResult =
            serde_json::from_value(first.expansions[0].clone()).unwrap();
        assert_eq!(first_result.entity_count, 2);
        assert!(first_result.truncated);
        let token = first_result.continuation_token.unwrap();

        let second = apply_bounded_expansions(
            base.clone(),
            &[FieldExpansionRequest {
                root_id: "item:0".to_string(),
                max_depth: 10,
                max_entities: 2,
                continuation_token: Some(token.clone()),
            }],
            &ui_state,
            "service:test",
        )
        .unwrap();
        let second_result: FieldExpansionResult =
            serde_json::from_value(second.expansions[0].clone()).unwrap();
        assert_eq!(second_result.entity_count, 4);
        assert!(second_result.truncated);

        let mut tampered = token.into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'a' { b'b' } else { b'a' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(
            apply_bounded_expansions(
                base,
                &[FieldExpansionRequest {
                    root_id: "item:0".to_string(),
                    max_depth: 10,
                    max_entities: 2,
                    continuation_token: Some(tampered),
                }],
                &ui_state,
                "service:test",
            )
            .is_err()
        );
    }

    #[test]
    fn expansion_does_not_issue_a_continuation_when_the_shared_budget_cannot_advance() {
        let subject = FieldFactSubject {
            kind: "project".to_string(),
            id: "project:test".to_string(),
            definition_ref: None,
            effective_definition_digest: None,
        };
        let mut builder = FieldFactsBuilder::new("project", "service:test", subject);
        let mut requests = Vec::new();
        for group in 0..6 {
            let root_id = format!("item:000-root-{group}");
            builder.add_entity(entity(&root_id, &root_id)).unwrap();
            requests.push(FieldExpansionRequest {
                root_id: root_id.clone(),
                max_depth: 1,
                max_entities: MAX_EXPANSION_ENTITIES,
                continuation_token: None,
            });
            for index in 0..1_000 {
                let child_id = format!("item:100-child-{group}-{index:04}");
                builder.add_entity(entity(&child_id, &child_id)).unwrap();
                builder
                    .add_relation(FieldFactRelation {
                        id: format!("edge:{group}-{index:04}"),
                        kind: "contains".to_string(),
                        source_id: root_id.clone(),
                        target_id: child_id,
                        status: None,
                        directed: true,
                        attributes: serde_json::json!({}),
                        provenance: FieldProvenance::pending("service:test", Vec::new()),
                    })
                    .unwrap();
            }
        }

        let expanded = apply_bounded_expansions(
            builder.finish().unwrap(),
            &requests,
            &crate::state::UiState::new(),
            "service:test",
        )
        .unwrap();
        let results = expanded
            .expansions
            .iter()
            .cloned()
            .map(serde_json::from_value::<FieldExpansionResult>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(results.len(), 6);
        assert!(results[..5].iter().all(|result| {
            result.entity_count == MAX_EXPANSION_ENTITIES
                && result.truncated
                && result.continuation_token.is_some()
        }));
        assert_eq!(results[5].entity_count, 0);
        assert!(results[5].truncated);
        assert!(results[5].continuation_token.is_none());
        assert!(expanded.warnings.iter().any(|warning| {
            warning.get("code").and_then(Value::as_str) == Some("expansion_no_progress")
        }));
    }

    #[test]
    fn expansion_materializes_facts_outside_the_base_document() {
        let subject = FieldFactSubject {
            kind: "project".to_string(),
            id: "project:test".to_string(),
            definition_ref: None,
            effective_definition_digest: None,
        };
        let mut builder = FieldFactsBuilder::new("project", "service:test", subject);
        for index in 0..=MAX_FIELD_FACT_ENTITIES {
            let id = format!("item:{index:04}");
            builder.add_entity(entity(&id, &id)).unwrap();
        }
        builder
            .add_relation(FieldFactRelation {
                id: "edge:outside-base".to_string(),
                kind: "contains".to_string(),
                source_id: "item:0000".to_string(),
                target_id: format!("item:{MAX_FIELD_FACT_ENTITIES:04}"),
                status: None,
                directed: true,
                attributes: serde_json::json!({}),
                provenance: FieldProvenance::pending("service:test", Vec::new()),
            })
            .unwrap();
        let base = builder.finish().unwrap();
        assert!(
            !base
                .entities
                .iter()
                .any(|entity| { entity.id == format!("item:{MAX_FIELD_FACT_ENTITIES:04}") })
        );

        let expanded = apply_bounded_expansions(
            base,
            &[FieldExpansionRequest {
                root_id: "item:0000".to_string(),
                max_depth: 4,
                max_entities: 2,
                continuation_token: None,
            }],
            &crate::state::UiState::new(),
            "service:test",
        )
        .unwrap();
        assert!(
            expanded
                .entities
                .iter()
                .any(|entity| { entity.id == format!("item:{MAX_FIELD_FACT_ENTITIES:04}") })
        );
        assert!(
            expanded
                .relations
                .iter()
                .any(|relation| relation.id == "edge:outside-base")
        );
        assert!(
            expanded
                .entities
                .iter()
                .all(|entity| { entity.provenance.source_revision == expanded.revision })
        );
    }

    #[test]
    fn expansion_continuation_is_rejected_after_source_revision_changes() {
        let subject = FieldFactSubject {
            kind: "project".to_string(),
            id: "project:test".to_string(),
            definition_ref: None,
            effective_definition_digest: None,
        };
        let build = |extra: bool| {
            let mut builder = FieldFactsBuilder::new("project", "service:test", subject.clone());
            for index in 0..3 {
                builder
                    .add_entity(entity(&format!("item:{index}"), &format!("item {index}")))
                    .unwrap();
            }
            if extra {
                builder
                    .add_entity(entity("item:changed", "changed"))
                    .unwrap();
            }
            for index in 1..3 {
                builder
                    .add_relation(FieldFactRelation {
                        id: format!("edge:{}-{index}", index - 1),
                        kind: "contains".to_string(),
                        source_id: format!("item:{}", index - 1),
                        target_id: format!("item:{index}"),
                        status: None,
                        directed: true,
                        attributes: serde_json::json!({}),
                        provenance: FieldProvenance::pending("service:test", Vec::new()),
                    })
                    .unwrap();
            }
            builder.finish().unwrap()
        };
        let ui_state = crate::state::UiState::new();
        let request = FieldExpansionRequest {
            root_id: "item:0".to_string(),
            max_depth: 8,
            max_entities: 1,
            continuation_token: None,
        };
        let first =
            apply_bounded_expansions(build(false), &[request], &ui_state, "service:test").unwrap();
        let token = serde_json::from_value::<FieldExpansionResult>(first.expansions[0].clone())
            .unwrap()
            .continuation_token
            .unwrap();

        let error = apply_bounded_expansions(
            build(true),
            &[FieldExpansionRequest {
                root_id: "item:0".to_string(),
                max_depth: 8,
                max_entities: 1,
                continuation_token: Some(token),
            }],
            &ui_state,
            "service:test",
        )
        .expect_err("a continuation cannot cross source revisions");
        assert!(error.to_string().contains("stale"));
    }
}
