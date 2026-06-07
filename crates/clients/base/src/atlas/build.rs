use std::collections::{BTreeMap, BTreeSet};

use super::model::{
    AtlasBoundsVm, AtlasInteractionVm, AtlasItemKind, AtlasLinkVm, AtlasNodeVm, AtlasProjectionVm,
    AtlasRegionVm, AtlasScope, AtlasStackItemVm, AtlasUiStateVm, AtlasVisualStateVm,
    NamespaceAtlasVm,
};
use crate::radial_tree::{layout_paths, RadialTreeNode};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AtlasInput {
    pub generation: u64,
    pub root_label: String,
    pub items: Vec<AtlasItemInput>,
    pub capabilities: Vec<String>,
    pub selected_ref: Option<String>,
    pub context_refs: Vec<String>,
    pub ui: AtlasUiStateVm,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AtlasItemInput {
    pub canonical_ref: String,
    pub kind: String,
    pub label: String,
    pub namespace: Option<String>,
    pub source_path: String,
    pub scope: String,
    pub executable: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AtlasFileSpaceInput {
    pub generation: u64,
    pub root_label: String,
    pub root: String,
    pub entries: Vec<AtlasFileInput>,
    pub selected_ref: Option<String>,
    pub ui: AtlasUiStateVm,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AtlasFileInput {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
}

pub fn build_namespace_atlas(input: AtlasInput) -> NamespaceAtlasVm {
    let mut stack_items: BTreeMap<String, Vec<AtlasStackItemVm>> = BTreeMap::new();
    let mut ref_to_namespace: BTreeMap<String, String> = BTreeMap::new();
    let mut paths = BTreeSet::new();

    for item in input.items {
        let Some(path) = normalize_item_path(&item) else {
            continue;
        };
        let namespace_key = path.join("/");
        paths.insert(path);
        let kind = AtlasItemKind::from_str(&item.kind);
        let canonical_ref = if item.canonical_ref.is_empty() {
            fallback_canonical_ref(kind, &namespace_key)
        } else {
            item.canonical_ref
        };
        ref_to_namespace.insert(canonical_ref.clone(), namespace_key.clone());
        stack_items
            .entry(namespace_key.clone())
            .or_default()
            .push(AtlasStackItemVm {
                id: format!("item:{canonical_ref}"),
                interaction: Some(AtlasInteractionVm::InspectItem {
                    canonical_ref: canonical_ref.clone(),
                }),
                canonical_ref,
                kind,
                scope: AtlasScope::from_str(&item.scope),
                label: non_empty(item.label).unwrap_or_else(|| label_from_key(&namespace_key)),
                source_path: item.source_path,
                executable: item.executable,
                y_offset: kind.layer_offset(),
            });
    }

    let capability_prefixes = capability_prefixes(&input.capabilities);
    for prefix in capability_prefixes.values() {
        if !prefix.is_empty() {
            paths.insert(prefix.clone());
        }
    }

    let layout_nodes = layout_paths(paths.into_iter().collect());
    let mut bounds = AtlasBoundsVm::default();
    let selected_ref = input.selected_ref.clone();
    let context_refs: BTreeSet<_> = input.context_refs.iter().cloned().collect();
    let selected_namespace = selected_ref
        .as_deref()
        .and_then(|selected| {
            stack_items
                .iter()
                .find(|(_, items)| items.iter().any(|item| item.canonical_ref == selected))
        })
        .map(|(namespace, _)| namespace.clone());

    let mut nodes = Vec::new();
    for layout in &layout_nodes {
        bounds.radius_max = bounds.radius_max.max(layout.radius);
        bounds.x_min = bounds.x_min.min(layout.position[0]);
        bounds.x_max = bounds.x_max.max(layout.position[0]);
        bounds.z_min = bounds.z_min.min(layout.position[2]);
        bounds.z_max = bounds.z_max.max(layout.position[2]);

        let namespace_key = layout.path.join("/");
        let mut stack = stack_items.remove(&namespace_key).unwrap_or_default();
        stack.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then_with(|| a.canonical_ref.cmp(&b.canonical_ref))
        });
        let selected = selected_ref
            .as_deref()
            .is_some_and(|selected| stack.iter().any(|item| item.canonical_ref == selected));
        let context_highlighted = stack
            .iter()
            .any(|item| context_refs.contains(&item.canonical_ref));
        let highlighted = selected
            || context_highlighted
            || selected_namespace
                .as_deref()
                .is_some_and(|selected_namespace| selected_namespace == namespace_key);
        let dimmed = selected_namespace.is_some() && !highlighted && !stack.is_empty();

        let visible_stack = stack
            .into_iter()
            .filter(|item| input.ui.item_visible(item.kind))
            .collect();
        nodes.push(node_from_layout(
            layout,
            visible_stack,
            &input.root_label,
            AtlasVisualStateVm {
                selected,
                highlighted,
                dimmed,
            },
            Some(AtlasInteractionVm::FocusFolder {
                root: None,
                path: namespace_key,
            }),
        ));
    }

    let regions = build_regions(&input.capabilities, capability_prefixes, &nodes);
    let links = build_context_links(&selected_ref, &context_refs, &ref_to_namespace, &nodes);

    NamespaceAtlasVm {
        schema_version: "ryeos.namespace_atlas.v1".to_string(),
        generation: input.generation,
        projection: AtlasProjectionVm::AiSpace,
        coordinate_system: "ryeos.radial_namespace.v1".to_string(),
        root_label: non_empty(input.root_label).unwrap_or_else(|| ".ai".to_string()),
        bounds,
        nodes,
        links,
        regions,
        selected_ref,
        ui: input.ui,
    }
}

pub fn build_file_space_atlas(input: AtlasFileSpaceInput) -> NamespaceAtlasVm {
    let mut stack_items: BTreeMap<String, Vec<AtlasStackItemVm>> = BTreeMap::new();
    let mut paths = BTreeSet::new();
    paths.insert(Vec::new());

    for entry in input.entries {
        let path = split_path(&entry.path);
        if entry.is_dir {
            paths.insert(path);
            continue;
        }
        let folder = path
            .get(..path.len().saturating_sub(1))
            .map(|parts| parts.to_vec())
            .unwrap_or_default();
        let folder_key = folder.join("/");
        paths.insert(folder);
        let file_ref = format!("file:{}:{}", input.root, entry.path);
        stack_items
            .entry(folder_key)
            .or_default()
            .push(AtlasStackItemVm {
                id: format!("item:{file_ref}"),
                interaction: Some(AtlasInteractionVm::ReadFile {
                    root: input.root.clone(),
                    path: entry.path.clone(),
                }),
                canonical_ref: file_ref,
                kind: AtlasItemKind::File,
                scope: AtlasScope::from_str(&input.root),
                label: non_empty(entry.name).unwrap_or_else(|| label_from_key(&entry.path)),
                source_path: entry.path,
                executable: false,
                y_offset: AtlasItemKind::File.layer_offset(),
            });
    }

    let layout_nodes = layout_paths(paths.into_iter().collect());
    let mut bounds = AtlasBoundsVm::default();
    let selected_ref = input.selected_ref.clone();
    let mut nodes = Vec::new();

    for layout in &layout_nodes {
        bounds.radius_max = bounds.radius_max.max(layout.radius);
        bounds.x_min = bounds.x_min.min(layout.position[0]);
        bounds.x_max = bounds.x_max.max(layout.position[0]);
        bounds.z_min = bounds.z_min.min(layout.position[2]);
        bounds.z_max = bounds.z_max.max(layout.position[2]);

        let namespace_key = layout.path.join("/");
        let mut stack = stack_items.remove(&namespace_key).unwrap_or_default();
        stack.sort_by(|a, b| a.source_path.cmp(&b.source_path));
        let selected = selected_ref
            .as_deref()
            .is_some_and(|selected| stack.iter().any(|item| item.canonical_ref == selected));
        let visible_stack = stack
            .into_iter()
            .filter(|item| input.ui.item_visible(item.kind))
            .collect();
        nodes.push(node_from_layout(
            layout,
            visible_stack,
            &input.root_label,
            AtlasVisualStateVm {
                selected,
                highlighted: selected,
                dimmed: false,
            },
            Some(AtlasInteractionVm::FocusFolder {
                root: Some(input.root.clone()),
                path: namespace_key,
            }),
        ));
    }

    NamespaceAtlasVm {
        schema_version: "ryeos.namespace_atlas.v1".to_string(),
        generation: input.generation,
        projection: AtlasProjectionVm::FileSpace,
        coordinate_system: "ryeos.radial_tree.v1".to_string(),
        root_label: non_empty(input.root_label).unwrap_or_else(|| "File Space".to_string()),
        bounds,
        nodes,
        links: Vec::new(),
        regions: Vec::new(),
        selected_ref,
        ui: input.ui,
    }
}

fn build_context_links(
    selected_ref: &Option<String>,
    context_refs: &BTreeSet<String>,
    ref_to_namespace: &BTreeMap<String, String>,
    nodes: &[AtlasNodeVm],
) -> Vec<AtlasLinkVm> {
    let Some(selected_ref) = selected_ref.as_deref() else {
        return Vec::new();
    };
    let Some(from_namespace) = ref_to_namespace.get(selected_ref) else {
        return Vec::new();
    };
    let namespace_to_node: BTreeMap<_, _> = nodes
        .iter()
        .map(|node| (node.namespace_key.as_str(), node.id.as_str()))
        .collect();
    let Some(from) = namespace_to_node.get(from_namespace.as_str()) else {
        return Vec::new();
    };

    context_refs
        .iter()
        .filter_map(|context_ref| {
            let to_namespace = ref_to_namespace.get(context_ref)?;
            let to = namespace_to_node.get(to_namespace.as_str())?;
            (to != from).then(|| AtlasLinkVm {
                id: format!("context:{selected_ref}->{context_ref}"),
                from: (*from).to_string(),
                to: (*to).to_string(),
                kind: "context".to_string(),
            })
        })
        .collect()
}

fn node_from_layout(
    layout: &RadialTreeNode,
    stack: Vec<AtlasStackItemVm>,
    root_label: &str,
    state: AtlasVisualStateVm,
    interaction: Option<AtlasInteractionVm>,
) -> AtlasNodeVm {
    let namespace_key = layout.path.join("/");
    AtlasNodeVm {
        id: if namespace_key.is_empty() {
            "ns:root".to_string()
        } else {
            format!("ns:{namespace_key}")
        },
        label: layout
            .path
            .last()
            .cloned()
            .unwrap_or_else(|| non_empty_str(root_label).unwrap_or(".ai").to_string()),
        namespace_key,
        path: layout.path.clone(),
        depth: layout.depth,
        angle: layout.angle,
        angle_start: layout.angle_start,
        angle_end: layout.angle_end,
        radius: layout.radius,
        position: layout.position,
        stack,
        state,
        interaction,
    }
}

fn build_regions(
    capabilities: &[String],
    prefixes: BTreeMap<String, Vec<String>>,
    nodes: &[AtlasNodeVm],
) -> Vec<AtlasRegionVm> {
    capabilities
        .iter()
        .filter_map(|capability| {
            let prefix = prefixes.get(capability)?;
            let key = prefix.join("/");
            let node = nodes.iter().find(|node| node.namespace_key == key)?;
            Some(AtlasRegionVm {
                id: format!("cap:{capability}"),
                capability: capability.clone(),
                label: capability.clone(),
                path_prefix: prefix.clone(),
                angle_start: node.angle_start,
                angle_end: node.angle_end,
                radius_min: node.radius,
                radius_max: nodes
                    .iter()
                    .filter(|candidate| {
                        candidate.namespace_key == key
                            || candidate.namespace_key.starts_with(&format!("{key}/"))
                    })
                    .map(|candidate| candidate.radius)
                    .fold(node.radius, f32::max),
                active: true,
            })
        })
        .collect()
}

fn capability_prefixes(capabilities: &[String]) -> BTreeMap<String, Vec<String>> {
    capabilities
        .iter()
        .filter_map(|capability| {
            capability_namespace_prefix(capability).map(|prefix| (capability.clone(), prefix))
        })
        .collect()
}

fn capability_namespace_prefix(capability: &str) -> Option<Vec<String>> {
    let marker = find_capability_layer_marker(capability)?;
    let suffix = capability[marker..].trim_start_matches(['.', '/']);
    let splitter = if suffix.contains('/') { '/' } else { '.' };
    let prefix: Vec<String> = suffix
        .split(splitter)
        .take_while(|token| *token != "*")
        .map(normalize_segment)
        .filter(|token| !token.is_empty())
        .collect();
    (!prefix.is_empty()).then_some(prefix)
}

fn find_capability_layer_marker(capability: &str) -> Option<usize> {
    let markers = [
        "directive",
        "directives",
        "tool",
        "tools",
        "knowledge",
        "config",
        "configs",
    ];
    let mut token_start = 0;
    for (index, ch) in capability.char_indices() {
        if ch != '.' && ch != '/' {
            continue;
        }
        if markers
            .iter()
            .any(|marker| capability[token_start..index].eq_ignore_ascii_case(marker))
        {
            return Some(index + ch.len_utf8());
        }
        token_start = index + ch.len_utf8();
    }
    markers
        .iter()
        .any(|marker| capability[token_start..].eq_ignore_ascii_case(marker))
        .then_some(capability.len())
}

fn normalize_item_path(item: &AtlasItemInput) -> Option<Vec<String>> {
    if let Some(namespace) = item.namespace.as_deref().and_then(non_empty_str) {
        let mut path = split_path(namespace);
        let label = item_label_path_segment(item);
        if path.last() != Some(&label) && !label.is_empty() {
            path.push(label);
        }
        return (!path.is_empty()).then_some(path);
    }
    if let Some(path) = path_from_canonical_ref(&item.canonical_ref) {
        return Some(path);
    }
    if let Some(path) = path_from_source_path(&item.source_path) {
        return Some(path);
    }
    let label = item_label_path_segment(item);
    (!label.is_empty()).then_some(vec![label])
}

fn path_from_canonical_ref(canonical_ref: &str) -> Option<Vec<String>> {
    let (_, bare) = canonical_ref.split_once(':')?;
    let path = split_path(bare);
    (!path.is_empty()).then_some(path)
}

fn path_from_source_path(source_path: &str) -> Option<Vec<String>> {
    let mut parts: Vec<_> = source_path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".ai")
        .map(str::to_string)
        .collect();
    if let Some(index) = parts.iter().position(|part| {
        matches!(
            part.as_str(),
            "directives" | "tools" | "knowledge" | "config" | "configs"
        )
    }) {
        parts = parts.split_off(index + 1);
    }
    if let Some(last) = parts.last_mut() {
        if let Some((stem, _)) = last.rsplit_once('.') {
            *last = stem.to_string();
        }
    }
    let path: Vec<_> = parts
        .into_iter()
        .map(|part| normalize_segment(&part))
        .filter(|part| !part.is_empty())
        .collect();
    (!path.is_empty()).then_some(path)
}

fn item_label_path_segment(item: &AtlasItemInput) -> String {
    item.canonical_ref
        .split_once(':')
        .map(|(_, bare)| bare.rsplit('/').next().unwrap_or(bare))
        .and_then(non_empty_str)
        .or_else(|| non_empty_str(&item.label))
        .map(normalize_segment)
        .unwrap_or_default()
}

fn split_path(path: &str) -> Vec<String> {
    path.split('/')
        .map(normalize_segment)
        .filter(|part| !part.is_empty())
        .collect()
}

fn normalize_segment(segment: &str) -> String {
    segment.trim().trim_matches('/').to_string()
}

fn fallback_canonical_ref(kind: AtlasItemKind, namespace_key: &str) -> String {
    let prefix = match kind {
        AtlasItemKind::Directive => "directive",
        AtlasItemKind::Tool => "tool",
        AtlasItemKind::Knowledge => "knowledge",
        AtlasItemKind::Config => "config",
        AtlasItemKind::File => "file",
        AtlasItemKind::Other => "item",
    };
    format!("{prefix}:{namespace_key}")
}

fn label_from_key(namespace_key: &str) -> String {
    namespace_key
        .rsplit('/')
        .next()
        .filter(|label| !label.is_empty())
        .unwrap_or("root")
        .to_string()
}

fn non_empty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn non_empty_str(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(canonical_ref: &str, kind: &str) -> AtlasItemInput {
        AtlasItemInput {
            canonical_ref: canonical_ref.to_string(),
            kind: kind.to_string(),
            label: String::new(),
            namespace: None,
            source_path: String::new(),
            scope: "project".to_string(),
            executable: false,
        }
    }

    #[test]
    fn stacks_items_with_same_namespace() {
        let atlas = build_namespace_atlas(AtlasInput {
            items: vec![
                item("directive:rye/core/create_tool", "directive"),
                item("tool:rye/core/create_tool", "tool"),
                item("knowledge:rye/core/create_tool", "knowledge"),
            ],
            ..AtlasInput::default()
        });
        let node = atlas
            .nodes
            .iter()
            .find(|node| node.namespace_key == "rye/core/create_tool")
            .expect("stack node");
        assert_eq!(node.stack.len(), 3);
        assert_eq!(node.stack[0].kind, AtlasItemKind::Knowledge);
        assert_eq!(node.stack[1].kind, AtlasItemKind::Tool);
        assert_eq!(node.stack[2].kind, AtlasItemKind::Directive);
    }

    #[test]
    fn layout_is_independent_of_input_order() {
        let mut input_a = AtlasInput {
            items: vec![
                item("directive:rye/core/create_tool", "directive"),
                item("knowledge:rye/core/signing", "knowledge"),
            ],
            ..AtlasInput::default()
        };
        let mut input_b = input_a.clone();
        input_b.items.reverse();
        let atlas_a = build_namespace_atlas(input_a.clone());
        input_a.items.reverse();
        let atlas_b = build_namespace_atlas(input_b);
        let positions_a: Vec<_> = atlas_a
            .nodes
            .iter()
            .map(|node| (node.namespace_key.clone(), node.position))
            .collect();
        let positions_b: Vec<_> = atlas_b
            .nodes
            .iter()
            .map(|node| (node.namespace_key.clone(), node.position))
            .collect();
        assert_eq!(positions_a, positions_b);
    }

    #[test]
    fn capability_maps_to_namespace_region() {
        let atlas = build_namespace_atlas(AtlasInput {
            items: vec![item("tool:rye/file-system/read", "tool")],
            capabilities: vec!["rye.execute.tool.rye.file-system.*".to_string()],
            ..AtlasInput::default()
        });
        assert_eq!(atlas.regions.len(), 1);
        assert_eq!(atlas.regions[0].path_prefix, vec!["rye", "file-system"]);
    }

    #[test]
    fn capability_slash_path_preserves_dotted_segments() {
        let atlas = build_namespace_atlas(AtlasInput {
            items: vec![item("knowledge:rye/v1.2/release.notes", "knowledge")],
            capabilities: vec!["rye.execute.knowledge.rye/v1.2/*".to_string()],
            ..AtlasInput::default()
        });
        assert_eq!(atlas.regions.len(), 1);
        assert_eq!(atlas.regions[0].path_prefix, vec!["rye", "v1.2"]);
    }

    #[test]
    fn namespace_projection_prefers_canonical_tail_over_label() {
        let atlas = build_namespace_atlas(AtlasInput {
            items: vec![AtlasItemInput {
                canonical_ref: "directive:rye/core/create_tool".to_string(),
                kind: "directive".to_string(),
                label: "Create Tool".to_string(),
                namespace: Some("rye/core".to_string()),
                scope: "project".to_string(),
                ..AtlasItemInput::default()
            }],
            ..AtlasInput::default()
        });
        assert!(atlas
            .nodes
            .iter()
            .any(|node| node.namespace_key == "rye/core/create_tool"));
        assert!(!atlas
            .nodes
            .iter()
            .any(|node| node.namespace_key == "rye/core/Create Tool"));
    }

    #[test]
    fn context_refs_highlight_matching_stack() {
        let atlas = build_namespace_atlas(AtlasInput {
            selected_ref: Some("directive:rye/core/create_tool".to_string()),
            context_refs: vec!["knowledge:rye/core/signing".to_string()],
            items: vec![
                item("directive:rye/core/create_tool", "directive"),
                item("knowledge:rye/core/signing", "knowledge"),
                item("knowledge:rye/other/unrelated", "knowledge"),
            ],
            ..AtlasInput::default()
        });
        let signing = atlas
            .nodes
            .iter()
            .find(|node| node.namespace_key == "rye/core/signing")
            .expect("context node");
        let unrelated = atlas
            .nodes
            .iter()
            .find(|node| node.namespace_key == "rye/other/unrelated")
            .expect("unrelated node");
        assert!(signing.state.highlighted);
        assert!(!signing.state.dimmed);
        assert!(unrelated.state.dimmed);
        assert_eq!(atlas.links.len(), 1);
        assert_eq!(atlas.links[0].kind, "context");
    }

    #[test]
    fn source_path_strips_only_filename_extension() {
        let atlas = build_namespace_atlas(AtlasInput {
            items: vec![AtlasItemInput {
                canonical_ref: String::new(),
                kind: "knowledge".to_string(),
                source_path: ".ai/knowledge/rye/v1.2/release.notes.md".to_string(),
                scope: "project_ai".to_string(),
                ..AtlasItemInput::default()
            }],
            ..AtlasInput::default()
        });
        let node = atlas
            .nodes
            .iter()
            .find(|node| node.namespace_key == "rye/v1.2/release.notes")
            .expect("dotted namespace node");
        assert_eq!(node.stack[0].scope, AtlasScope::Project);
    }

    #[test]
    fn canonical_ref_preserves_dotted_segments() {
        let atlas = build_namespace_atlas(AtlasInput {
            items: vec![item("knowledge:rye/v1.2/release.notes", "knowledge")],
            ..AtlasInput::default()
        });
        assert!(atlas
            .nodes
            .iter()
            .any(|node| node.namespace_key == "rye/v1.2/release.notes"));
    }
}
