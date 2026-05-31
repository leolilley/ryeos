use serde::{Deserialize, Serialize};

use super::event::StudioAction;
use super::model::{StudioCore, StudioInspectorState};
use super::scene_model::{build_scene_model, StudioSceneModel};
use crate::ids::TileId;
use crate::layout::{LayoutTree, SplitAxis};
use crate::workspace::{TileState, ViewLocalState, ViewSpec};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioViewModel {
    pub schema_version: String,
    pub generation: u64,
    pub session: StudioSessionVm,
    pub chrome: StudioChromeVm,
    pub workspace: StudioWorkspaceVm,
    pub launcher: StudioLauncherVm,
    pub overlays: Vec<StudioOverlayVm>,
    pub notices: Vec<StudioNoticeVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StudioSessionVm {
    pub session_id: String,
    pub project_path: Option<String>,
    pub surface_ref: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioChromeVm {
    pub title: String,
    pub subtitle: String,
    pub health_label: String,
    pub health_tone: StudioTone,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioWorkspaceVm {
    pub root: StudioLayoutNodeVm,
    pub focused_tile: String,
    pub is_home: bool,
    pub tile_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioLayoutNodeVm {
    Split {
        axis: StudioSplitAxisVm,
        ratio: f32,
        first: Box<StudioLayoutNodeVm>,
        second: Box<StudioLayoutNodeVm>,
    },
    Tile {
        tile_id: String,
        focused: bool,
        title: String,
        actions: Vec<StudioTileActionVm>,
        view: StudioViewVm,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudioSplitAxisVm {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioViewVm {
    Overview {
        metrics: Vec<StudioMetricVm>,
        sections: Vec<StudioSectionVm>,
    },
    ThreadList {
        rows: Vec<StudioRowVm>,
    },
    Thread {
        thread_id: Option<String>,
        sections: Vec<StudioSectionVm>,
        code_blocks: Vec<StudioCodeBlockVm>,
    },
    Items {
        filters: StudioPanelFiltersVm,
        rows: Vec<StudioRowVm>,
    },
    Files {
        root: String,
        path: String,
        rows: Vec<StudioRowVm>,
    },
    Rows {
        title: String,
        columns: Vec<String>,
        rows: Vec<StudioRowVm>,
    },
    Gc {
        running: bool,
        recent_events: Vec<serde_json::Value>,
    },
    Map {
        scene: StudioSceneModel,
    },
    Inspector(StudioInspectorVm),
    Placeholder {
        title: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioLauncherItemVm {
    pub label: String,
    pub hint: String,
    pub action: StudioAction,
    pub secondary_action: Option<StudioAction>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioLauncherVm {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    pub hint: String,
    pub items: Vec<StudioLauncherItemVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioTileActionVm {
    pub label: String,
    pub title: String,
    pub action: StudioAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StudioOverlayVm {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioMetricVm {
    pub label: String,
    pub value: String,
    pub hint: Option<String>,
    pub tone: StudioTone,
    pub action: Option<StudioAction>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioSectionVm {
    pub title: String,
    pub rows: Vec<(String, String)>,
    pub action: Option<StudioAction>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioRowVm {
    pub id: String,
    pub primary: String,
    pub secondary: Option<String>,
    pub meta: Option<String>,
    pub action: Option<StudioAction>,
    pub tone: StudioTone,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioPanelFiltersVm {
    pub tile_id: String,
    pub items_query: String,
    pub items_kind: String,
    pub item_kind_options: Vec<StudioFilterOptionVm>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioFilterOptionVm {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioInspectorVm {
    pub title: String,
    pub subtitle: Option<String>,
    pub sections: Vec<StudioSectionVm>,
    pub code_blocks: Vec<StudioCodeBlockVm>,
    pub empty: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioCodeBlockVm {
    pub label: String,
    pub language: Option<String>,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StudioNoticeVm {
    pub id: String,
    pub message: String,
    pub tone: StudioTone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StudioTone {
    Good,
    Warn,
    Danger,
    #[default]
    Neutral,
    Accent,
}

pub fn build_view_model(core: &StudioCore) -> StudioViewModel {
    let session = session_vm(core);
    let health = health_label(core);
    StudioViewModel {
        schema_version: "ryeos.studio.vm.v1".to_string(),
        generation: core.generation,
        session,
        chrome: StudioChromeVm {
            title: "RyeOS Studio".to_string(),
            subtitle: subtitle(core),
            health_label: health.clone(),
            health_tone: tone_for_health(&health),
        },
        workspace: workspace_vm(core),
        launcher: launcher(core),
        overlays: Vec::new(),
        notices: core.notices_vm(),
    }
}

fn workspace_vm(core: &StudioCore) -> StudioWorkspaceVm {
    StudioWorkspaceVm {
        root: layout_node_vm(&core.workspace.layout, core),
        focused_tile: tile_id_text(core.workspace.focused_tile),
        is_home: core.workspace.is_home(),
        tile_count: core.workspace.layout.tile_ids().len(),
    }
}

fn layout_node_vm(node: &LayoutTree, core: &StudioCore) -> StudioLayoutNodeVm {
    match node {
        LayoutTree::Leaf(tile_id) => {
            let view = core
                .workspace
                .tiles
                .get(tile_id)
                .map(|tile| view_vm(core, *tile_id, tile))
                .unwrap_or_else(|| StudioViewVm::Placeholder {
                    title: "Missing view".to_string(),
                    message: format!("Tile {} is not present in the workspace.", tile_id.0),
                });
            let title = core
                .workspace
                .tiles
                .get(tile_id)
                .map(|tile| tile.view.title())
                .unwrap_or_else(|| "Missing".to_string());
            StudioLayoutNodeVm::Tile {
                tile_id: tile_id_text(*tile_id),
                focused: *tile_id == core.workspace.focused_tile,
                title,
                actions: tile_actions(core, *tile_id),
                view,
            }
        }
        LayoutTree::Split {
            axis,
            ratio,
            first,
            second,
        } => StudioLayoutNodeVm::Split {
            axis: match axis {
                SplitAxis::Horizontal => StudioSplitAxisVm::Horizontal,
                SplitAxis::Vertical => StudioSplitAxisVm::Vertical,
            },
            ratio: *ratio,
            first: Box::new(layout_node_vm(first, core)),
            second: Box::new(layout_node_vm(second, core)),
        },
    }
}

fn view_vm(core: &StudioCore, tile_id: TileId, tile: &TileState) -> StudioViewVm {
    match &tile.view {
        ViewSpec::Overview => overview(core),
        ViewSpec::ThreadList => StudioViewVm::ThreadList {
            rows: rows_for(core, &ViewSpec::ThreadList, Some(tile_id)),
        },
        ViewSpec::Thread { thread_id } => StudioViewVm::Thread {
            thread_id: thread_id.map(|id| id.0.to_string()),
            sections: Vec::new(),
            code_blocks: Vec::new(),
        },
        ViewSpec::SpaceBrowser { .. } => StudioViewVm::Items {
            filters: StudioPanelFiltersVm {
                tile_id: tile_id_text(tile_id),
                items_query: item_filter_state(tile).0,
                items_kind: item_filter_state(tile).1,
                item_kind_options: item_kind_options(),
            },
            rows: rows_for(core, &tile.view, Some(tile_id)),
        },
        ViewSpec::Files => files(core, tile_id, tile),
        ViewSpec::Services => StudioViewVm::Rows {
            title: "Services".to_string(),
            columns: vec!["Name".to_string(), "Detail".to_string()],
            rows: rows_for(core, &tile.view, None),
        },
        ViewSpec::Remotes => StudioViewVm::Rows {
            title: "Remotes".to_string(),
            columns: vec!["Name".to_string(), "Detail".to_string()],
            rows: rows_for(core, &tile.view, None),
        },
        ViewSpec::Schedules => StudioViewVm::Rows {
            title: "Schedules".to_string(),
            columns: vec!["Name".to_string(), "Detail".to_string()],
            rows: rows_for(core, &tile.view, None),
        },
        ViewSpec::GcStatus => StudioViewVm::Gc {
            running: core
                .data
                .gc_status
                .as_ref()
                .map(|gc| gc.running)
                .unwrap_or(false),
            recent_events: core
                .data
                .gc_status
                .as_ref()
                .map(|gc| gc.recent_events.clone())
                .unwrap_or_default(),
        },
        ViewSpec::Graph { .. } => StudioViewVm::Map {
            scene: build_scene_model(core),
        },
        ViewSpec::ItemInspector | ViewSpec::EventInspector => {
            StudioViewVm::Inspector(inspector(core))
        }
        ViewSpec::Projects => StudioViewVm::Rows {
            title: "Projects".to_string(),
            columns: vec!["Name".to_string(), "Root".to_string(), "Status".to_string()],
            rows: rows_for(core, &tile.view, Some(tile_id)),
        },
        ViewSpec::Trust => StudioViewVm::Placeholder {
            title: "Trust".to_string(),
            message: "Trust view is not wired yet.".to_string(),
        },
    }
}

fn session_vm(core: &StudioCore) -> StudioSessionVm {
    let browser = core.data.session.as_ref();
    let snapshot = core.data.snapshot.as_ref();
    StudioSessionVm {
        session_id: browser
            .map(|session| session.session_id.clone())
            .or_else(|| snapshot.map(|snapshot| snapshot.session.session_id.clone()))
            .unwrap_or_default(),
        project_path: browser
            .and_then(|session| session.project_path.clone())
            .or_else(|| {
                snapshot.and_then(|snapshot| snapshot.project.as_ref().map(|p| p.path.clone()))
            }),
        surface_ref: browser
            .map(|session| session.surface_ref.clone())
            .or_else(|| snapshot.map(|snapshot| snapshot.session.surface_ref.clone()))
            .unwrap_or_default(),
        read_only: browser
            .map(|session| session.read_only)
            .or_else(|| snapshot.map(|snapshot| snapshot.session.read_only))
            .unwrap_or(true),
    }
}

pub(crate) fn launcher_items() -> Vec<StudioLauncherItemVm> {
    launcher_specs()
        .into_iter()
        .map(|(label, hint, view)| StudioLauncherItemVm {
            label: label.to_string(),
            hint: hint.to_string(),
            action: StudioAction::OpenView { view: view.clone() },
            secondary_action: Some(StudioAction::OpenNewView { view }),
            enabled: true,
        })
        .collect()
}

fn launcher_specs() -> [(&'static str, &'static str, ViewSpec); 8] {
    [
        (
            "Graph",
            "Workspace topology",
            ViewSpec::Graph { graph_id: None },
        ),
        (
            "Items",
            "RyeOS objects",
            ViewSpec::SpaceBrowser { project: None },
        ),
        ("Projects", "Known local roots", ViewSpec::Projects),
        ("Files", "Project files", ViewSpec::Files),
        ("Threads", "Runs and events", ViewSpec::ThreadList),
        ("Services", "Daemon endpoints", ViewSpec::Services),
        ("Schedules", "Timed work", ViewSpec::Schedules),
        ("GC", "State cleanup", ViewSpec::GcStatus),
    ]
}

fn launcher(core: &StudioCore) -> StudioLauncherVm {
    let query = core.ui.launcher.query.trim().to_lowercase();
    let items: Vec<_> = launcher_items()
        .into_iter()
        .filter(|item| {
            let haystack = format!("{} {}", item.label, item.hint).to_lowercase();
            query.is_empty() || haystack.contains(&query)
        })
        .collect();
    let selected = core.ui.launcher.selected.min(items.len().saturating_sub(1));
    StudioLauncherVm {
        open: core.ui.launcher.open,
        query: core.ui.launcher.query.clone(),
        selected,
        hint: "Alt+K open · ↑/↓ select · Enter choose · Shift+Enter new tile · Esc close"
            .to_string(),
        items,
    }
}

fn tile_actions(core: &StudioCore, tile_id: TileId) -> Vec<StudioTileActionVm> {
    let tile_id = tile_id_text(tile_id);
    let mut actions = vec![
        StudioTileActionVm {
            label: "↔".to_string(),
            title: "Split right".to_string(),
            action: StudioAction::SplitTile {
                tile_id: tile_id.clone(),
                axis: SplitAxis::Horizontal,
            },
        },
        StudioTileActionVm {
            label: "↕".to_string(),
            title: "Split down".to_string(),
            action: StudioAction::SplitTile {
                tile_id: tile_id.clone(),
                axis: SplitAxis::Vertical,
            },
        },
    ];
    if core.workspace.layout.tile_ids().len() > 1 {
        actions.push(StudioTileActionVm {
            label: "×".to_string(),
            title: "Close tile".to_string(),
            action: StudioAction::CloseTile { tile_id },
        });
    }
    actions
}

fn overview(core: &StudioCore) -> StudioViewVm {
    let health = health_label(core);
    StudioViewVm::Overview {
        metrics: vec![
            metric(
                "Health",
                &health,
                "Local node",
                tone_for_health(&health),
                None,
            ),
            metric(
                "Threads",
                &core
                    .data
                    .threads
                    .as_ref()
                    .map(|x| x.threads.len())
                    .unwrap_or(0)
                    .to_string(),
                "Recent runs",
                StudioTone::Neutral,
                Some(StudioAction::OpenView {
                    view: ViewSpec::ThreadList,
                }),
            ),
            metric(
                "Items",
                &core
                    .data
                    .items
                    .as_ref()
                    .map(|x| x.items.len())
                    .unwrap_or(0)
                    .to_string(),
                "Loaded objects",
                StudioTone::Neutral,
                Some(StudioAction::OpenView {
                    view: ViewSpec::SpaceBrowser { project: None },
                }),
            ),
            metric(
                "Projects",
                &core
                    .data
                    .projects
                    .as_ref()
                    .map(|x| x.projects.len())
                    .unwrap_or(0)
                    .to_string(),
                "Known local roots",
                StudioTone::Neutral,
                Some(StudioAction::OpenView {
                    view: ViewSpec::Projects,
                }),
            ),
            metric(
                "Services",
                &core
                    .data
                    .snapshot
                    .as_ref()
                    .map(|x| x.local_node.services.len())
                    .unwrap_or(0)
                    .to_string(),
                "Daemon endpoints",
                StudioTone::Neutral,
                Some(StudioAction::OpenView {
                    view: ViewSpec::Services,
                }),
            ),
        ],
        sections: vec![StudioSectionVm {
            title: "Project".to_string(),
            rows: vec![
                (
                    "Path".to_string(),
                    session_vm(core)
                        .project_path
                        .unwrap_or_else(|| "No project bound".to_string()),
                ),
                ("Mode".to_string(), "RyeOS Studio".to_string()),
            ],
            action: Some(StudioAction::SelectSnapshot),
        }],
    }
}

fn project_rows(core: &StudioCore) -> Vec<StudioRowVm> {
    let mut rows = Vec::new();
    let current_project = session_vm(core).project_path;
    if let Some(root) = current_project.as_ref().filter(|root| {
        !core.data.projects.as_ref().is_some_and(|projects| {
            projects
                .projects
                .iter()
                .any(|project| project.root == **root)
        })
    }) {
        rows.push(row(
            "__add_current_project".to_string(),
            "Register current project".to_string(),
            Some(root.clone()),
            Some("not registered".to_string()),
            Some(StudioAction::AddCurrentProject),
        ));
    }
    rows.extend(
        core.data
            .projects
            .as_ref()
            .map(|projects| projects.projects.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|project| {
                let local_id = project.local_id.clone();
                row(
                    local_id.clone(),
                    if project.name.is_empty() {
                        project.root.clone()
                    } else {
                        project.name
                    },
                    Some(project.root),
                    Some(
                        if project.exists {
                            "available"
                        } else {
                            "missing"
                        }
                        .to_string(),
                    ),
                    project
                        .exists
                        .then_some(StudioAction::OpenProject { local_id }),
                )
            }),
    );
    rows
}

fn rows_for(core: &StudioCore, view: &ViewSpec, tile_id: Option<TileId>) -> Vec<StudioRowVm> {
    let mut rows: Vec<StudioRowVm> = match view {
        ViewSpec::ThreadList | ViewSpec::Thread { .. } => core
            .data
            .threads
            .as_ref()
            .map(|x| x.threads.clone())
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let id =
                    field_text(&value, &["thread_id", "id"]).unwrap_or_else(|| index.to_string());
                row(
                    id.clone(),
                    id.clone(),
                    field_text(&value, &["item_ref", "item"]),
                    field_text(&value, &["status", "state"]),
                    Some(StudioAction::InspectThread { thread_id: id }),
                )
            })
            .collect(),
        ViewSpec::SpaceBrowser { .. } => tile_id
            .and_then(|tile_id| core.data.tile_items.get(&tile_id_text(tile_id)))
            .or(core.data.items.as_ref())
            .as_ref()
            .map(|x| x.items.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|item| {
                row(
                    item.canonical_ref.clone(),
                    if item.label.is_empty() {
                        item.bare_id
                    } else {
                        item.label
                    },
                    Some(item.canonical_ref.clone()),
                    Some(item.item_kind),
                    Some(StudioAction::InspectItem {
                        canonical_ref: item.canonical_ref,
                    }),
                )
            })
            .collect(),
        ViewSpec::Schedules => core
            .data
            .schedules
            .as_ref()
            .map(|x| x.schedules.clone())
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                row(
                    index.to_string(),
                    field_text(&value, &["schedule_id", "id", "name"])
                        .unwrap_or_else(|| "schedule".to_string()),
                    field_text(&value, &["item_ref", "target"]),
                    field_text(&value, &["enabled"]),
                    Some(StudioAction::InspectSummary {
                        title: "Schedule".to_string(),
                        detail: value,
                    }),
                )
            })
            .collect(),
        ViewSpec::Remotes => core
            .data
            .snapshot
            .as_ref()
            .map(|x| x.remotes.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|remote| {
                row(
                    remote.name.clone(),
                    remote.name.clone(),
                    Some(remote.url.clone()),
                    Some(remote.principal_id.clone()),
                    Some(StudioAction::InspectSummary {
                        title: remote.name.clone(),
                        detail: serde_json::to_value(remote).unwrap_or_default(),
                    }),
                )
            })
            .collect(),
        ViewSpec::Services => core
            .data
            .snapshot
            .as_ref()
            .map(|x| x.local_node.services.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|service| {
                row(
                    service.endpoint.clone(),
                    service.endpoint.clone(),
                    Some(service.service_ref.clone()),
                    Some(service.availability.clone()),
                    Some(StudioAction::InspectSummary {
                        title: service.endpoint.clone(),
                        detail: serde_json::to_value(service).unwrap_or_default(),
                    }),
                )
            })
            .collect(),
        ViewSpec::Projects => project_rows(core),
        _ => Vec::new(),
    };
    if let Some(cursor) = tile_id.and_then(|tile_id| selected_cursor(core, tile_id)) {
        if let Some(row) = rows.get_mut(cursor) {
            row.selected = true;
        }
    }
    rows
}

fn files(core: &StudioCore, tile_id: TileId, tile: &TileState) -> StudioViewVm {
    let tile_id_text = tile_id_text(tile_id);
    let (root, path) = file_state(tile);
    StudioViewVm::Files {
        root: root.clone(),
        path: path.clone(),
        rows: mark_selected(
            core.data
                .tile_files
                .get(&tile_id_text)
                .or(core.data.files.as_ref())
                .as_ref()
                .map(|x| x.entries.clone())
                .unwrap_or_default()
                .into_iter()
                .map(|entry| {
                    let path = join_path(&path, &entry.name);
                    row(
                        path.clone(),
                        entry.name,
                        if entry.is_dir {
                            Some("directory".to_string())
                        } else {
                            None
                        },
                        entry.size.map(|size| size.to_string()),
                        Some(if entry.is_dir {
                            StudioAction::ListFiles {
                                tile_id: tile_id_text.clone(),
                                root: root.clone(),
                                path,
                            }
                        } else {
                            StudioAction::ReadFile {
                                root: root.clone(),
                                path,
                            }
                        }),
                    )
                })
                .collect(),
            selected_cursor(core, tile_id),
        ),
    }
}

pub(crate) fn action_for_focused_row(core: &StudioCore) -> Option<StudioAction> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    let cursor = selected_cursor(core, tile_id).unwrap_or(0);
    let rows = rows_for(core, view, Some(tile_id));
    rows.get(cursor).and_then(|row| row.action.clone())
}

fn selected_cursor(core: &StudioCore, tile_id: TileId) -> Option<usize> {
    let tile = core.workspace.tiles.get(&tile_id)?;
    match &tile.local {
        ViewLocalState::ThreadList { cursor, .. }
        | ViewLocalState::SpaceBrowser { cursor, .. }
        | ViewLocalState::Files { cursor, .. }
        | ViewLocalState::GenericList { cursor, .. } => Some(*cursor),
        ViewLocalState::Thread(state) => Some(state.timeline_cursor),
        ViewLocalState::None => None,
    }
}

fn mark_selected(mut rows: Vec<StudioRowVm>, cursor: Option<usize>) -> Vec<StudioRowVm> {
    if let Some(row) = cursor.and_then(|cursor| rows.get_mut(cursor)) {
        row.selected = true;
    }
    rows
}

fn item_filter_state(tile: &TileState) -> (String, String) {
    match &tile.local {
        ViewLocalState::SpaceBrowser { query, kind, .. } => (query.clone(), kind.clone()),
        _ => (String::new(), String::new()),
    }
}

fn file_state(tile: &TileState) -> (String, String) {
    match &tile.local {
        ViewLocalState::Files { root, path, .. } => (root.clone(), path.clone()),
        _ => ("project_ai".to_string(), String::new()),
    }
}

fn inspector(core: &StudioCore) -> StudioInspectorVm {
    match &core.ui.inspector {
        StudioInspectorState::Snapshot => StudioInspectorVm {
            title: "Studio".to_string(),
            subtitle: Some("Current project and local node state".to_string()),
            sections: vec![StudioSectionVm {
                title: "Project".to_string(),
                rows: vec![
                    (
                        "Path".to_string(),
                        session_vm(core)
                            .project_path
                            .unwrap_or_else(|| "No project bound".to_string()),
                    ),
                    ("Mode".to_string(), "RyeOS Studio".to_string()),
                    ("Health".to_string(), health_label(core)),
                ],
                action: None,
            }],
            code_blocks: Vec::new(),
            empty: false,
        },
        StudioInspectorState::Summary { title, detail } => {
            code_or_empty(title, None, serde_json::to_string_pretty(detail).ok())
        }
        StudioInspectorState::Item { canonical_ref } => code_or_empty(
            canonical_ref,
            Some("Item"),
            core.data
                .item_inspection
                .as_ref()
                .and_then(|x| serde_json::to_string_pretty(x).ok()),
        ),
        StudioInspectorState::Thread { thread_id } => code_or_empty(
            thread_id,
            Some("Thread"),
            core.data
                .thread_inspection
                .as_ref()
                .and_then(|x| serde_json::to_string_pretty(x).ok()),
        ),
        StudioInspectorState::File { root, path } => code_or_empty(
            path,
            Some(root),
            core.data.file_read.as_ref().map(|x| x.content.clone()),
        ),
        StudioInspectorState::Empty => StudioInspectorVm {
            title: "Studio".to_string(),
            subtitle: Some("Select an object to inspect it.".to_string()),
            sections: Vec::new(),
            code_blocks: Vec::new(),
            empty: true,
        },
    }
}

fn code_or_empty(
    title: &str,
    subtitle: Option<&str>,
    content: Option<String>,
) -> StudioInspectorVm {
    let empty = content.is_none();
    StudioInspectorVm {
        title: title.to_string(),
        subtitle: subtitle.map(str::to_string),
        sections: Vec::new(),
        code_blocks: content
            .map(|content| {
                vec![StudioCodeBlockVm {
                    label: "Detail".to_string(),
                    language: Some("json".to_string()),
                    content,
                }]
            })
            .unwrap_or_default(),
        empty,
    }
}

fn metric(
    label: &str,
    value: &str,
    hint: &str,
    tone: StudioTone,
    action: Option<StudioAction>,
) -> StudioMetricVm {
    StudioMetricVm {
        label: label.to_string(),
        value: value.to_string(),
        hint: Some(hint.to_string()),
        tone,
        action,
    }
}

fn row(
    id: String,
    primary: String,
    secondary: Option<String>,
    meta: Option<String>,
    action: Option<StudioAction>,
) -> StudioRowVm {
    StudioRowVm {
        id,
        primary,
        secondary,
        meta,
        action,
        tone: StudioTone::Neutral,
        selected: false,
    }
}

fn field_text(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| value.get(*key)).map(|v| {
        v.as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string())
    })
}

fn join_path(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else {
        format!("{base}/{name}")
    }
}

fn health_label(core: &StudioCore) -> String {
    core.data
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.local_node.health.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("connecting")
        .to_string()
}

fn tone_for_health(value: &str) -> StudioTone {
    let lower = value.to_ascii_lowercase();
    if lower.contains("healthy") {
        StudioTone::Good
    } else if lower.contains("degraded") {
        StudioTone::Warn
    } else if lower.contains("error") || lower.contains("failed") {
        StudioTone::Danger
    } else {
        StudioTone::Neutral
    }
}

fn subtitle(core: &StudioCore) -> String {
    session_vm(core)
        .project_path
        .unwrap_or_else(|| "Tiled RyeOS workspace".to_string())
}

fn item_kind_options() -> Vec<StudioFilterOptionVm> {
    [
        ("", "All kinds"),
        ("directive", "Directives"),
        ("tool", "Tools"),
        ("knowledge", "Knowledge"),
        ("config", "Config"),
    ]
    .into_iter()
    .map(|(value, label)| StudioFilterOptionVm {
        value: value.to_string(),
        label: label.to_string(),
    })
    .collect()
}

fn tile_id_text(id: TileId) -> String {
    id.0.to_string()
}
