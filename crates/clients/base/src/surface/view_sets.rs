//! Signed initial view-set composition. This is presentation data inside a
//! surface, not a second kind, view resolver or execution authority.

use super::{SlotsSpec, TilingSpec, ViewKindSpec};
use crate::ids::ViewGroupId;
use crate::layout::{
    LayoutTree, MAX_LAYOUT_DEPTH, MAX_LAYOUT_TILES, MAX_SPLIT_RATIO, MIN_SPLIT_RATIO, SplitAxis,
};
use crate::view_set::ViewSet;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_VIEW_SETS: usize = 16;
pub const MAX_VIEW_SET_LABEL_BYTES: usize = 128;
pub const MAX_SAVED_VIEW_SETS: usize = 64;
pub const MAX_SAVED_VIEW_SET_ID_BYTES: usize = 64;
pub const MAX_SAVED_VIEW_SET_LIBRARY_BYTES: usize = 256 * 1024;
pub const MAX_PARTICULAR_VIEW_SETS: usize = 64;
pub const MAX_PARTICULAR_VIEW_SET_LIBRARY_BYTES: usize = 256 * 1024;
pub const MAX_PROJECT_LOCAL_ID_BYTES: usize = 128;
pub const MAX_WORK_CHAIN_ROOT_ID_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewSetSeedSpec {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub root: Option<LayoutSeedSpec>,
    #[serde(default)]
    pub slots: SlotsSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LayoutSeedSpec {
    Group {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        views: Vec<ViewKindSpec>,
        active: usize,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

/// One reusable personal composition. This is deliberately only a template:
/// it carries no mounted identities, drafts, observations, compiled grants or
/// retained session authority. Opening it allocates a fresh view set and
/// revalidates every referenced view against the current compiled surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedViewSetTemplate {
    pub id: String,
    pub name: String,
    pub composition: ViewSetSeedSpec,
    pub relationships: Vec<SavedViewSelectionRelationship>,
}

/// Stable address of a mount inside a reusable composition. Tile indices use
/// the composition's deterministic depth-first view order; slots are named so
/// runtime mount ids never enter the durable template.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SavedViewMountRef {
    Tile { index: usize },
    Slot { edge: SavedViewSlotEdge },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedViewSlotEdge {
    Top,
    Bottom,
    Left,
    Right,
}

/// Portable subject relationship. A required subject records only its input
/// name and logical facet names; live values, fingerprints and authority are
/// supplied afresh when the template is opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SavedViewSelectionSource {
    FollowOwnSet,
    FollowSet { saved_view_set_id: String },
    RequiredSubject { input: String, facets: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedViewSelectionRelationship {
    pub mount: SavedViewMountRef,
    pub source: SavedViewSelectionSource,
}

/// Durable locator for one principal-registered project. This is registry
/// identity only: the daemon must re-resolve it and reopen current directory
/// authority when the particular set is resumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticularProjectRef {
    pub local_id: String,
}

/// Durable logical-work identity. Placement thread ids are intentionally not
/// persisted because continuation may advance while the set is closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticularWorkRef {
    pub chain_root_id: String,
}

/// Re-resolvable subject context for one particular composition. Absence of a
/// project with a work ref explicitly means projectless work; it never means
/// that a path or ambient project may be inferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticularViewSetContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ParticularProjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<ParticularWorkRef>,
}

/// One durable particular set. It contains reusable presentation plus only
/// stable lookup identities; grants, paths, drafts, mounted ids, observations,
/// arbitrary pinned values and retained attachment authority cannot be encoded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticularViewSetResume {
    pub id: String,
    pub name: String,
    pub composition: ViewSetSeedSpec,
    pub relationships: Vec<SavedViewSelectionRelationship>,
    pub context: ParticularViewSetContext,
}

/// Fresh runtime resolution returned by the daemon for one durable logical
/// work coordinate. This placement is deliberately absent from durable config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedParticularWork {
    pub thread: String,
    pub chain_root: String,
}

/// Typed result paired with a particular-set renderer transition. The daemon
/// has already re-resolved its stable config record; the client may use this
/// only after admitting the accompanying fresh attachment (when present).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedParticularViewSet {
    pub id: String,
    pub name: String,
    pub composition: ViewSetSeedSpec,
    pub relationships: Vec<SavedViewSelectionRelationship>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_selection_work: Option<ResolvedParticularWork>,
}

pub fn validate_particular_view_set_resumes(
    resumes: &[ParticularViewSetResume],
) -> Result<(), String> {
    let encoded = serde_json::to_vec(resumes)
        .map_err(|error| format!("particular_view_sets cannot be encoded: {error}"))?;
    if encoded.len() > MAX_PARTICULAR_VIEW_SET_LIBRARY_BYTES {
        return Err(format!(
            "particular_view_sets exceeds {MAX_PARTICULAR_VIEW_SET_LIBRARY_BYTES} bytes"
        ));
    }
    if resumes.len() > MAX_PARTICULAR_VIEW_SETS {
        return Err(format!(
            "particular_view_sets exceeds {MAX_PARTICULAR_VIEW_SETS} entries"
        ));
    }
    let mut ids = BTreeSet::new();
    for (index, resume) in resumes.iter().enumerate() {
        let location = format!("particular_view_sets[{index}]");
        if !bounded_identifier(&resume.id, MAX_SAVED_VIEW_SET_ID_BYTES) || !ids.insert(&resume.id) {
            return Err(format!("{location}.id must be a unique bounded identifier"));
        }
        if resume.name.trim().is_empty()
            || resume.name.len() > MAX_VIEW_SET_LABEL_BYTES
            || resume.name.chars().any(char::is_control)
        {
            return Err(format!("{location}.name must be a nonempty bounded label"));
        }
        if resume.composition.id != resume.id {
            return Err(format!(
                "{location}.composition.id must match the particular-set id"
            ));
        }
        validate_saved_view_set_templates(std::slice::from_ref(&SavedViewSetTemplate {
            id: resume.id.clone(),
            name: resume.name.clone(),
            composition: resume.composition.clone(),
            relationships: resume.relationships.clone(),
        }))
        .map_err(|error| format!("{location}: {error}"))?;
        let supported_work_facets = [
            "selection.work",
            "selection.work.thread",
            "selection.work.chain_root",
        ];
        for relationship in &resume.relationships {
            let SavedViewSelectionSource::RequiredSubject { facets, .. } = &relationship.source
            else {
                continue;
            };
            if resume.context.work.is_none()
                || facets
                    .iter()
                    .any(|facet| !supported_work_facets.contains(&facet.as_str()))
            {
                return Err(format!(
                    "{location}.relationships contains a required subject that cannot be re-resolved from its logical-work context"
                ));
            }
        }
        if resume.context.project.is_none() && resume.context.work.is_none() {
            return Err(format!(
                "{location}.context must identify a project or logical work"
            ));
        }
        if let Some(project) = &resume.context.project
            && !bounded_identifier(&project.local_id, MAX_PROJECT_LOCAL_ID_BYTES)
        {
            return Err(format!(
                "{location}.context.project.local_id must be a bounded identifier"
            ));
        }
        if let Some(work) = &resume.context.work
            && !bounded_identifier(&work.chain_root_id, MAX_WORK_CHAIN_ROOT_ID_BYTES)
        {
            return Err(format!(
                "{location}.context.work.chain_root_id must be a bounded identifier"
            ));
        }
    }
    Ok(())
}

fn bounded_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':'))
}

pub fn validate_saved_view_set_templates(templates: &[SavedViewSetTemplate]) -> Result<(), String> {
    let encoded = serde_json::to_vec(templates)
        .map_err(|error| format!("saved_view_sets cannot be encoded: {error}"))?;
    if encoded.len() > MAX_SAVED_VIEW_SET_LIBRARY_BYTES {
        return Err(format!(
            "saved_view_sets exceeds {MAX_SAVED_VIEW_SET_LIBRARY_BYTES} bytes"
        ));
    }
    if templates.len() > MAX_SAVED_VIEW_SETS {
        return Err(format!(
            "saved_view_sets exceeds {MAX_SAVED_VIEW_SETS} entries"
        ));
    }
    let mut ids = BTreeSet::new();
    for (index, template) in templates.iter().enumerate() {
        let location = format!("saved_view_sets[{index}]");
        if template.id.is_empty()
            || template.id.len() > MAX_SAVED_VIEW_SET_ID_BYTES
            || !template
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || !ids.insert(&template.id)
        {
            return Err(format!("{location}.id must be a unique bounded identifier"));
        }
        if template.name.trim().is_empty()
            || template.name.len() > MAX_VIEW_SET_LABEL_BYTES
            || template.name.chars().any(char::is_control)
        {
            return Err(format!("{location}.name must be a nonempty bounded label"));
        }
        validate_seeds(std::slice::from_ref(&template.composition))
            .map_err(|error| format!("{location}.composition: {error}"))?;
        let mut mounts = BTreeSet::new();
        let mut required_inputs = std::collections::BTreeMap::new();
        let tile_count = template
            .composition
            .root
            .as_ref()
            .map_or(0, layout_view_count);
        let mut expected_mounts = (0..tile_count)
            .map(|index| SavedViewMountRef::Tile { index })
            .collect::<BTreeSet<_>>();
        for edge in [
            SavedViewSlotEdge::Top,
            SavedViewSlotEdge::Bottom,
            SavedViewSlotEdge::Left,
            SavedViewSlotEdge::Right,
        ] {
            if saved_slot_present(&template.composition, edge) {
                expected_mounts.insert(SavedViewMountRef::Slot { edge });
            }
        }
        for (relationship_index, relationship) in template.relationships.iter().enumerate() {
            let relationship_location = format!("{location}.relationships[{relationship_index}]");
            if !mounts.insert(&relationship.mount) {
                return Err(format!("{relationship_location}.mount is duplicated"));
            }
            match &relationship.mount {
                SavedViewMountRef::Tile { index } if *index >= tile_count => {
                    return Err(format!("{relationship_location}.mount is not present"));
                }
                SavedViewMountRef::Slot { edge }
                    if !saved_slot_present(&template.composition, *edge) =>
                {
                    return Err(format!("{relationship_location}.mount is not present"));
                }
                _ => {}
            }
            match &relationship.source {
                SavedViewSelectionSource::FollowOwnSet => {}
                SavedViewSelectionSource::FollowSet { saved_view_set_id }
                    if !bounded_saved_set_identifier(saved_view_set_id) =>
                {
                    return Err(format!(
                        "{relationship_location}.source.saved_view_set_id must be a bounded identifier"
                    ));
                }
                SavedViewSelectionSource::FollowSet { .. } => {}
                SavedViewSelectionSource::RequiredSubject { input, facets } => {
                    if !bounded_saved_set_identifier(input) {
                        return Err(format!(
                            "{relationship_location}.source.input must be a bounded identifier"
                        ));
                    }
                    let mut unique = BTreeSet::new();
                    if facets.is_empty()
                        || facets.iter().any(|facet| {
                            facet.len() > MAX_VIEW_SET_LABEL_BYTES
                                || !(facet == "selection" || facet.starts_with("selection."))
                                || !unique.insert(facet)
                        })
                    {
                        return Err(format!(
                            "{relationship_location}.source.facets must be unique bounded selection facets"
                        ));
                    }
                    if let Some(previous) = required_inputs.insert(input, facets)
                        && previous != facets
                    {
                        return Err(format!(
                            "{relationship_location}.source.input must name one consistent facet contract"
                        ));
                    }
                }
            }
        }
        if mounts != expected_mounts {
            return Err(format!(
                "{location}.relationships must name every mounted tile and slot exactly once"
            ));
        }
    }
    Ok(())
}

fn bounded_saved_set_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SAVED_VIEW_SET_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn layout_view_count(layout: &LayoutSeedSpec) -> usize {
    match layout {
        LayoutSeedSpec::Group { views, .. } => views.len(),
        LayoutSeedSpec::Split { first, second, .. } => {
            layout_view_count(first) + layout_view_count(second)
        }
    }
}

fn saved_slot_present(composition: &ViewSetSeedSpec, edge: SavedViewSlotEdge) -> bool {
    match edge {
        SavedViewSlotEdge::Top => composition.slots.top.is_some(),
        SavedViewSlotEdge::Bottom => composition.slots.bottom.is_some(),
        SavedViewSlotEdge::Left => composition.slots.left.is_some(),
        SavedViewSlotEdge::Right => composition.slots.right.is_some(),
    }
}

pub fn validate_seeds(seeds: &[ViewSetSeedSpec]) -> Result<(), String> {
    if seeds.len() > MAX_VIEW_SETS {
        return Err(format!("view_sets exceeds {MAX_VIEW_SETS} entries"));
    }
    let mut ids = BTreeSet::new();
    let mut total_views = 0;
    for (index, seed) in seeds.iter().enumerate() {
        let location = format!("view_sets[{index}]");
        if seed.id.is_empty()
            || seed.id.len() > 64
            || !seed
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || !ids.insert(&seed.id)
        {
            return Err(format!("{location}.id must be a unique bounded identifier"));
        }
        if seed.title.trim().is_empty()
            || seed.title.len() > MAX_VIEW_SET_LABEL_BYTES
            || seed.title.chars().any(char::is_control)
        {
            return Err(format!("{location}.title must be a nonempty bounded label"));
        }
        let mut pending = seed
            .root
            .as_ref()
            .map(|root| vec![(root, 1, format!("{location}.root"))])
            .unwrap_or_default();
        while let Some((node, depth, path)) = pending.pop() {
            if depth > MAX_LAYOUT_DEPTH {
                return Err(format!("{path} exceeds layout depth {MAX_LAYOUT_DEPTH}"));
            }
            match node {
                LayoutSeedSpec::Group {
                    label,
                    views,
                    active,
                } => {
                    if label.as_ref().is_some_and(|label| {
                        label.trim().is_empty()
                            || label.len() > MAX_VIEW_SET_LABEL_BYTES
                            || label.chars().any(char::is_control)
                    }) {
                        return Err(format!("{path}.label is not a bounded display label"));
                    }
                    if views.is_empty() || *active >= views.len() {
                        return Err(format!("{path}: group requires views and an active member"));
                    }
                    total_views += views.len();
                    if total_views > MAX_LAYOUT_TILES {
                        return Err(format!(
                            "{path}: surface exceeds {MAX_LAYOUT_TILES} mounted views"
                        ));
                    }
                }
                LayoutSeedSpec::Split {
                    ratio,
                    first,
                    second,
                    ..
                } => {
                    if !ratio.is_finite() || !(MIN_SPLIT_RATIO..=MAX_SPLIT_RATIO).contains(ratio) {
                        return Err(format!(
                            "{path}.ratio is outside supported split proportions"
                        ));
                    }
                    pending.push((second, depth + 1, format!("{path}.second")));
                    pending.push((first, depth + 1, format!("{path}.first")));
                }
            }
        }
    }
    Ok(())
}

impl ViewSetSeedSpec {
    pub fn instantiate(&self, tiling: &TilingSpec) -> Result<ViewSet, String> {
        validate_seeds(std::slice::from_ref(self))?;
        fn collect(node: &LayoutSeedSpec, views: &mut Vec<crate::view_set::ViewSpec>) {
            match node {
                LayoutSeedSpec::Group { views: group, .. } => {
                    views.extend(group.iter().map(ViewKindSpec::to_view_spec))
                }
                LayoutSeedSpec::Split { first, second, .. } => {
                    collect(first, views);
                    collect(second, views);
                }
            }
        }
        let mut views = Vec::new();
        if let Some(root) = &self.root {
            collect(root, &mut views);
        }
        // Allocate mounted identities once. Authored tree membership references
        // those instances; it never creates a second mutable view collection.
        let mut view_set = ViewSet::from_tiling(tiling.clone(), views);
        let mut ids: Vec<_> = view_set.tiles.keys().copied().collect();
        ids.sort_by_key(|id| id.0);
        fn instantiate(
            node: &LayoutSeedSpec,
            ids: &mut std::slice::Iter<'_, crate::ids::TileId>,
        ) -> LayoutTree {
            match node {
                LayoutSeedSpec::Group {
                    label,
                    views,
                    active,
                } => {
                    let tabs: Vec<_> = ids.by_ref().take(views.len()).copied().collect();
                    LayoutTree::Group {
                        group_id: ViewGroupId::new(tabs[0].0),
                        label: label.clone(),
                        active: tabs[*active],
                        tabs,
                    }
                }
                LayoutSeedSpec::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => LayoutTree::Split {
                    axis: *axis,
                    ratio: *ratio,
                    first: Box::new(instantiate(first, ids)),
                    second: Box::new(instantiate(second, ids)),
                },
            }
        }
        view_set.root = self
            .root
            .as_ref()
            .map(|root| instantiate(root, &mut ids.iter()));
        view_set.title = self.title.clone();
        view_set.docks = crate::ui::model::RyeOsDockState::from_slots(&self.slots);
        if let Some(root) = &view_set.root {
            root.validate()?;
            let first = root.active_tile_ids()[0];
            view_set.focus_tile(first);
        }
        Ok(view_set)
    }
}

/// Shared daemon/client check. Nested references still resolve through the
/// ordinary surface view collector, including inactive group tabs.
pub fn validate_effective_view_sets(value: &serde_json::Value) -> Result<(), String> {
    let Some(raw) = value.get("view_sets") else {
        return Ok(());
    };
    let seeds: Vec<ViewSetSeedSpec> =
        serde_json::from_value(raw.clone()).map_err(|error| format!("view_sets: {error}"))?;
    validate_seeds(&seeds)?;
    if !seeds.is_empty()
        && (value
            .get("tiles")
            .and_then(|v| v.as_array())
            .is_some_and(|v| !v.is_empty())
            || value
                .get("slots")
                .and_then(|v| v.as_object())
                .is_some_and(|v| v.values().any(|slot| !slot.is_null())))
    {
        return Err("view_sets cannot be combined with surface-level tiles or slots; author each view set's composition explicitly".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed() -> serde_json::Value {
        json!({ "id": "work", "title": "Work", "root": {
            "type": "split", "axis": "horizontal", "ratio": 0.7,
            "first": { "type": "group", "label": "Worker", "views": ["view:test/conversation", "view:test/evidence"], "active": 1 },
            "second": { "type": "group", "label": "Changes", "views": ["view:test/changes"], "active": 0 }
        } })
    }

    #[test]
    fn reusable_relationships_are_mount_bounded_and_cannot_encode_live_pins() {
        let composition: ViewSetSeedSpec = serde_json::from_value(seed()).unwrap();
        let template = SavedViewSetTemplate {
            id: "work".into(),
            name: "Work".into(),
            composition,
            relationships: vec![
                SavedViewSelectionRelationship {
                    mount: SavedViewMountRef::Tile { index: 0 },
                    source: SavedViewSelectionSource::RequiredSubject {
                        input: "work_subject".into(),
                        facets: vec!["selection.work.id".into()],
                    },
                },
                SavedViewSelectionRelationship {
                    mount: SavedViewMountRef::Tile { index: 1 },
                    source: SavedViewSelectionSource::FollowOwnSet,
                },
                SavedViewSelectionRelationship {
                    mount: SavedViewMountRef::Tile { index: 2 },
                    source: SavedViewSelectionSource::FollowOwnSet,
                },
            ],
        };
        validate_saved_view_set_templates(std::slice::from_ref(&template)).unwrap();
        let encoded = serde_json::to_value(&template).unwrap();
        assert!(encoded.to_string().contains("required_subject"));
        assert!(!encoded.to_string().contains("fingerprint"));
        assert!(!encoded.to_string().contains("values"));

        let mut incomplete = template.clone();
        incomplete.relationships.pop();
        assert!(
            validate_saved_view_set_templates(&[incomplete])
                .unwrap_err()
                .contains("every mounted tile and slot")
        );

        let mut missing_mount = template.clone();
        missing_mount.relationships[0].mount = SavedViewMountRef::Tile { index: 99 };
        assert!(validate_saved_view_set_templates(&[missing_mount]).is_err());

        let mut injected = encoded;
        injected["relationships"][0]["source"]["values"] = json!({"selection.work.id":"old"});
        assert!(serde_json::from_value::<SavedViewSetTemplate>(injected).is_err());
    }

    fn particular(id: &str) -> ParticularViewSetResume {
        let mut composition: ViewSetSeedSpec = serde_json::from_value(seed()).unwrap();
        composition.id = id.into();
        ParticularViewSetResume {
            id: id.into(),
            name: "Current work".into(),
            composition,
            relationships: (0..3)
                .map(|index| SavedViewSelectionRelationship {
                    mount: SavedViewMountRef::Tile { index },
                    source: SavedViewSelectionSource::FollowOwnSet,
                })
                .collect(),
            context: ParticularViewSetContext {
                project: Some(ParticularProjectRef {
                    local_id: "prj_example".into(),
                }),
                work: Some(ParticularWorkRef {
                    chain_root_id: "T-chain-root".into(),
                }),
            },
        }
    }

    #[test]
    fn particular_resume_contains_only_re_resolvable_identity() {
        let resume = particular("particular-1");
        validate_particular_view_set_resumes(std::slice::from_ref(&resume)).unwrap();
        let value = serde_json::to_value(&resume).unwrap();
        assert_eq!(value["context"]["project"]["local_id"], "prj_example");
        assert_eq!(value["context"]["work"]["chain_root_id"], "T-chain-root");
        for forbidden in [
            "project_path",
            "thread_id",
            "binding_digest",
            "grants",
            "draft",
            "mounted_id",
            "values",
            "fingerprint",
        ] {
            assert!(!value.to_string().contains(forbidden), "found {forbidden}");
        }
        let mut forbidden = value;
        forbidden["context"]["work"]["thread_id"] = json!("T-placement");
        assert!(serde_json::from_value::<ParticularViewSetResume>(forbidden).is_err());
    }

    #[test]
    fn particular_resume_rejects_subjects_outside_its_stable_context() {
        let mut resume = particular("particular-unsupported");
        resume.relationships[0].source = SavedViewSelectionSource::RequiredSubject {
            input: "file".into(),
            facets: vec!["selection.file.path".into()],
        };
        assert!(
            validate_particular_view_set_resumes(&[resume])
                .unwrap_err()
                .contains("cannot be re-resolved")
        );
    }

    #[test]
    fn particular_resume_validates_duplicates_context_and_identifiers() {
        let resume = particular("particular-1");
        assert!(
            validate_particular_view_set_resumes(&[resume.clone(), resume.clone()])
                .unwrap_err()
                .contains("unique bounded identifier")
        );
        let mut empty = resume.clone();
        empty.context = ParticularViewSetContext {
            project: None,
            work: None,
        };
        assert!(
            validate_particular_view_set_resumes(&[empty])
                .unwrap_err()
                .contains("project or logical work")
        );
        let mut bad_project = resume.clone();
        bad_project.context.project.as_mut().unwrap().local_id = "../project".into();
        assert!(validate_particular_view_set_resumes(&[bad_project]).is_err());
        let mut bad_work = resume;
        bad_work.context.work.as_mut().unwrap().chain_root_id = "work id".into();
        assert!(validate_particular_view_set_resumes(&[bad_work]).is_err());
    }

    #[test]
    fn particular_resume_accepts_explicit_projectless_work_and_enforces_count() {
        let mut projectless = particular("projectless");
        projectless.context.project = None;
        validate_particular_view_set_resumes(&[projectless]).unwrap();

        let resumes = (0..=MAX_PARTICULAR_VIEW_SETS)
            .map(|index| particular(&format!("particular-{index}")))
            .collect::<Vec<_>>();
        assert!(
            validate_particular_view_set_resumes(&resumes)
                .unwrap_err()
                .contains("entries")
        );

        let mut oversized = particular("oversized");
        oversized.name = "x".repeat(MAX_PARTICULAR_VIEW_SET_LIBRARY_BYTES);
        assert!(
            validate_particular_view_set_resumes(&[oversized])
                .unwrap_err()
                .contains("bytes")
        );
    }

    #[test]
    fn authored_groups_preserve_selection_and_allocate_fresh_mounts() {
        let spec: ViewSetSeedSpec = serde_json::from_value(seed()).unwrap();
        let first = spec.instantiate(&TilingSpec::default()).unwrap();
        let second = spec.instantiate(&TilingSpec::default()).unwrap();
        assert_eq!(first.title, "Work");
        assert_eq!(
            first.tiles[&first.focused_tile].view.view_ref,
            "view:test/evidence"
        );
        assert_eq!(first.root.as_ref().unwrap().active_tile_ids().len(), 2);
        let LayoutTree::Split {
            first: worker,
            second: changes,
            ..
        } = first.root.as_ref().unwrap()
        else {
            panic!("authored split was not retained");
        };
        assert!(
            matches!(worker.as_ref(), LayoutTree::Group { label: Some(label), .. } if label == "Worker")
        );
        assert!(
            matches!(changes.as_ref(), LayoutTree::Group { label: Some(label), .. } if label == "Changes")
        );
        assert_eq!(first.tiles.len(), 3);
        assert!(
            first
                .tile_ids()
                .iter()
                .all(|id| !second.tiles.contains_key(id))
        );
    }

    #[test]
    fn malformed_compositions_fail_with_the_authored_location() {
        let mut bad = seed();
        bad["root"]["first"]["active"] = json!(9);
        let error = validate_effective_view_sets(&json!({ "view_sets": [bad] })).unwrap_err();
        assert!(error.contains("view_sets[0].root.first"));
        let mut bad = seed();
        bad["root"]["ratio"] = json!(1.0);
        assert!(
            validate_effective_view_sets(&json!({ "view_sets": [bad] }))
                .unwrap_err()
                .contains("root.ratio")
        );
        assert!(validate_effective_view_sets(&json!({ "view_sets": [seed(), seed()] })).is_err());
        assert!(
            validate_effective_view_sets(
                &json!({ "view_sets": [seed()], "tiles": ["view:test/other"] })
            )
            .is_err()
        );
        assert!(validate_effective_view_sets(&json!({ "view_sets": [seed()], "slots": { "left": { "content": "view:test/other" } } })).is_err());
    }

    #[test]
    fn core_mounts_only_the_named_view_sets_and_their_slots() {
        let mut work = seed();
        work["slots"] =
            json!({ "left": { "content": "view:test/tree", "open": true, "size": 24 } });
        let effective_surface =
            json!({ "name": "test", "view_sets": [work, { "id": "review", "title": "Review" }] });
        let session = crate::ui::model::BrowserSession {
            surface_attachment_id: "view-sets-test-attachment".into(),
            binding_attachments: vec![crate::ui::binding::UiBindingAttachment {
                binding_attachment_id: "view-sets-test-attachment".into(),
                binding_generation: 1,
                binding_digest: "view-sets-test-binding".into(),
                surface_ref: "surface:test/view-sets".into(),
                surface_generation: "view-sets-test-surface".into(),
                effective_surface,
                project_path: None,
                posture: Default::default(),
                binding_request_bounds: crate::ui::binding::UiBindingRequestBounds {
                    max_request_bytes: 64 * 1024,
                    max_input_bytes: 16 * 1024,
                },
            }],
            ..Default::default()
        };
        let core = crate::ui::model::RyeOsCore::new(session, Default::default(), 0);
        assert_eq!(core.view_sets.len(), 2);
        assert!(core.view_sets[0].docks.left.is_some());
        assert!(core.view_sets[1].docks.left.is_none());
        assert_eq!(core.view_sets[1].title, "Review");
        assert!(core.view_sets[1].center_is_empty());
    }
}
