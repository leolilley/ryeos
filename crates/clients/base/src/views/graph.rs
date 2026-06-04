//! Graph view — state graph topology visualization.

use crate::atlas::{build_namespace_atlas, AtlasInput, AtlasItemInput};
use crate::model::AppModel;
use crate::text_surface::TextSurface;

pub fn build(model: &AppModel, w: usize, h: usize) -> TextSurface {
    let capabilities = model
        .store
        .cockpit
        .as_ref()
        .map(|cockpit| {
            let mut caps = cockpit.session.granted_caps.clone();
            for service in &cockpit.local_node.services {
                caps.extend(service.required_caps.clone());
            }
            caps
        })
        .unwrap_or_default();
    let items = model
        .store
        .items
        .values()
        .map(|item| AtlasItemInput {
            canonical_ref: canonical_ref_for(&item.kind, &item.name),
            kind: item.kind.clone(),
            label: item.name.clone(),
            namespace: item.category.clone(),
            source_path: String::new(),
            scope: String::new(),
            executable: false,
        })
        .collect();
    let atlas = build_namespace_atlas(AtlasInput {
        generation: model.generation,
        root_label: ".ai".to_string(),
        items,
        capabilities,
        selected_ref: model
            .store
            .item_inspection
            .as_ref()
            .map(|inspection| inspection.canonical_ref.clone()),
        context_refs: Vec::new(),
        ui: model.visual.atlas.clone(),
    });

    crate::atlas::text::render_atlas(&atlas, w, h)
}

fn canonical_ref_for(kind: &str, name: &str) -> String {
    if name.contains(':') {
        return name.to_string();
    }
    let prefix = match kind.trim().to_ascii_lowercase().as_str() {
        "directive" | "directives" => "directive",
        "tool" | "tools" => "tool",
        "knowledge" => "knowledge",
        "config" | "configs" | "configuration" => "config",
        _ => "item",
    };
    format!("{prefix}:{name}")
}
