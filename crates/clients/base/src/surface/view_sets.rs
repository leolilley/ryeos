//! Signed initial view-set composition. This is presentation data inside a
//! surface, not a second kind, view resolver or execution authority.

#[cfg(test)]
use super::TilingSpec;
use super::ViewKindSpec;
#[cfg(test)]
use crate::ids::ViewGroupId;
#[cfg(test)]
use crate::layout::LayoutTree;
use crate::layout::{
    MAX_LAYOUT_DEPTH, MAX_LAYOUT_TILES, MAX_SPLIT_RATIO, MIN_SPLIT_RATIO, SplitAxis,
};
#[cfg(test)]
use crate::view_set::ViewSet;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_VIEW_SETS: usize = 16;
pub const VIEW_SET_COMPOSITION_SCHEMA: u32 = 2;
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
    pub schema: u32,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub root: Option<LayoutSeedSpec>,
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
        first: Box<LayoutSeedSpec>,
        second: Box<LayoutSeedSpec>,
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
/// the composition's deterministic depth-first view order. Shared docks/dialogs
/// never enter a saved view set.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SavedViewMountRef {
    Tile { index: usize },
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
    /// Logical Surface definition selected when this exact set was retained.
    /// Resume must re-resolve it under current admission; this is not a grant.
    pub surface_ref: String,
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

impl ResolvedParticularWork {
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("thread", self.thread.as_str()),
            ("chain_root", self.chain_root.as_str()),
        ] {
            if !bounded_identifier(value, MAX_WORK_CHAIN_ROOT_ID_BYTES) {
                return Err(format!("resolved particular work {name} is invalid"));
            }
        }
        Ok(())
    }
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

impl ResolvedParticularViewSet {
    pub fn validate(&self) -> Result<(), String> {
        validate_saved_view_set_templates(std::slice::from_ref(&SavedViewSetTemplate {
            id: self.id.clone(),
            name: self.name.clone(),
            composition: self.composition.clone(),
            relationships: self.relationships.clone(),
        }))?;
        if self.composition.id != self.id {
            return Err("resolved particular composition differs from its identity".into());
        }
        if let Some(work) = &self.resolved_selection_work {
            work.validate()?;
        }
        let supported_work_facets = [
            "selection.work",
            "selection.work.thread",
            "selection.work.chain_root",
        ];
        if self.relationships.iter().any(|relationship| {
            let SavedViewSelectionSource::RequiredSubject { facets, .. } = &relationship.source
            else {
                return false;
            };
            self.resolved_selection_work.is_none()
                || facets
                    .iter()
                    .any(|facet| !supported_work_facets.contains(&facet.as_str()))
        }) {
            return Err("resolved particular subject differs from its work context".into());
        }
        Ok(())
    }
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
        if resume
            .context
            .surface_ref
            .strip_prefix("surface:")
            .is_none_or(|path| path.is_empty())
            || resume.context.surface_ref.trim() != resume.context.surface_ref
            || resume.context.surface_ref.chars().any(char::is_control)
        {
            return Err(format!(
                "{location}.context.surface_ref must name the retained logical Surface"
            ));
        }
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
        let expected_mounts = (0..tile_count)
            .map(|index| SavedViewMountRef::Tile { index })
            .collect::<BTreeSet<_>>();
        for (relationship_index, relationship) in template.relationships.iter().enumerate() {
            let relationship_location = format!("{location}.relationships[{relationship_index}]");
            if !mounts.insert(relationship.mount.clone()) {
                return Err(format!("{relationship_location}.mount is duplicated"));
            }
            match &relationship.mount {
                SavedViewMountRef::Tile { index } if *index >= tile_count => {
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
                "{location}.relationships must name every mounted tile exactly once"
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

pub fn validate_seeds(seeds: &[ViewSetSeedSpec]) -> Result<(), String> {
    if seeds.len() > MAX_VIEW_SETS {
        return Err(format!("view_sets exceeds {MAX_VIEW_SETS} entries"));
    }
    let mut ids = BTreeSet::new();
    let mut total_views = 0;
    for (index, seed) in seeds.iter().enumerate() {
        let location = format!("view_sets[{index}]");
        if seed.schema != VIEW_SET_COMPOSITION_SCHEMA {
            return Err(format!("{location}.schema is not current"));
        }
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
    #[cfg(test)]
    pub fn instantiate(
        &self,
        tiling: &TilingSpec,
        presentation: &crate::ids::PresentationId,
        surface: &crate::ids::SurfaceInstanceId,
    ) -> Result<ViewSet, String> {
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
        let mut view_set = ViewSet::from_tiling(tiling.clone(), views, presentation, surface);
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
        json!({ "schema": 2, "id": "work", "title": "Work", "root": {
            "type": "split", "axis": "horizontal", "ratio": 0.7,
            "first": { "type": "group", "label": "Worker", "views": ["view:test/conversation", "view:test/evidence"], "active": 1 },
            "second": { "type": "group", "label": "Changes", "views": ["view:test/changes"], "active": 0 }
        } })
    }

    #[test]
    fn predecessor_compositions_and_slot_addresses_are_rejected() {
        let mut old = seed();
        old.as_object_mut().unwrap().remove("schema");
        assert!(serde_json::from_value::<ViewSetSeedSpec>(old).is_err());
        let mut stale = seed();
        stale["schema"] = json!(1);
        assert!(validate_seeds(&[serde_json::from_value(stale).unwrap()]).is_err());
        let mut slots = seed();
        slots["slots"] = json!({});
        assert!(serde_json::from_value::<ViewSetSeedSpec>(slots).is_err());
        assert!(
            serde_json::from_value::<SavedViewMountRef>(json!({"kind":"slot","edge":"left"}))
                .is_err()
        );
        let current: ViewSetSeedSpec = serde_json::from_value(seed()).unwrap();
        let roundtrip: ViewSetSeedSpec =
            serde_json::from_value(serde_json::to_value(&current).unwrap()).unwrap();
        assert_eq!(current, roundtrip);
        assert!(validate_seeds(&[roundtrip]).is_ok());
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
                .contains("every mounted tile")
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
                surface_ref: "surface:test/particular".into(),
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
            surface_ref: "surface:test/particular".into(),
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
        let presentation = crate::ids::PresentationId::try_new("seed-fixture").unwrap();
        let surface = crate::ids::SurfaceInstanceId::try_new("seed-surface").unwrap();
        let first = spec
            .instantiate(&TilingSpec::default(), &presentation, &surface)
            .unwrap();
        let second = spec
            .instantiate(&TilingSpec::default(), &presentation, &surface)
            .unwrap();
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
    fn core_mounts_named_view_sets_without_duplicating_shared_docks() {
        use crate::ui::model::RyeOsDockEdge;
        use crate::ui::reducer::test_support::{core, session_with_surface};
        let work = seed();
        let session = session_with_surface(json!({
            "name": "test",
            "view_sets": [work, {"schema":2,"id":"review","title":"Review"}],
            "views": {
                "view:test/conversation":{"widget":"text"},
                "view:test/evidence":{"widget":"rows"},
                "view:test/changes":{"widget":"rows"}
            }
        }));
        let mut core = core(session);
        let sets = core.active_surface_view_sets().unwrap();
        assert_eq!(sets.len(), 2);
        for set in sets {
            let serialized = serde_json::to_value(set).unwrap();
            assert!(serialized.get("docks").is_none());
            assert!(serialized.get("dock_local").is_none());
        }
        assert_eq!(sets[1].title, "Review");
        assert!(sets[1].center_is_empty());
        let shared_left = core.shared_dock_instance_key(RyeOsDockEdge::Left).unwrap();
        let shared_bottom = core
            .shared_dock_instance_key(RyeOsDockEdge::Bottom)
            .unwrap();
        let review = sets[1].id;
        core.active_surface_mut()
            .unwrap()
            .select_view_set(review)
            .unwrap();
        assert_eq!(
            core.shared_dock_instance_key(RyeOsDockEdge::Left).unwrap(),
            shared_left
        );
        assert_eq!(
            core.shared_dock_instance_key(RyeOsDockEdge::Bottom)
                .unwrap(),
            shared_bottom
        );
        assert_eq!(core.presentation.as_ref().unwrap().docks().len(), 3);
    }
}
