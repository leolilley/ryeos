//! Persistence — config and session state.
//!
//! Config: theme, keybindings, default layout, animation settings.
//! Session: layout tree, tile IDs, view specs, focused tile.

use ryeos_tui_core::ids::TileId;
use ryeos_tui_core::layout::{LayoutTree, SplitAxis};
use ryeos_tui_core::workspace::{TileState, ViewSpec};
use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    pub animation_enabled: bool,
    pub content_max_width: usize,
    pub tick_interval_ms: u64,
    pub poll_interval_secs: u64,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            animation_enabled: true,
            content_max_width: 160,
            tick_interval_ms: 50,
            poll_interval_secs: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiSession {
    pub layout: LayoutTreeSer,
    pub tiles: HashMap<u64, TileStateSer>,
    pub focused_tile_id: u64,
}

/// Serializable layout tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LayoutTreeSer {
    Leaf(u64),
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<LayoutTreeSer>,
        second: Box<LayoutTreeSer>,
    },
}

/// Serializable tile state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TileStateSer {
    pub view: ViewSpecSer,
}

/// Serializable view spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ViewSpecSer {
    Thread { thread_id: Option<u64> },
    ThreadList,
    Remotes,
    Projects,
    SpaceBrowser,
    Trust,
    Graph,
    EventInspector,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

impl From<&LayoutTree> for LayoutTreeSer {
    fn from(tree: &LayoutTree) -> Self {
        match tree {
            LayoutTree::Leaf(id) => LayoutTreeSer::Leaf(id.0),
            LayoutTree::Split {
                axis,
                ratio,
                first,
                second,
            } => LayoutTreeSer::Split {
                axis: *axis,
                ratio: *ratio,
                first: Box::new(LayoutTreeSer::from(first.as_ref())),
                second: Box::new(LayoutTreeSer::from(second.as_ref())),
            },
        }
    }
}

impl From<&TileState> for TileStateSer {
    fn from(ts: &TileState) -> Self {
        TileStateSer {
            view: ViewSpecSer::from(&ts.view),
        }
    }
}

impl From<&ViewSpec> for ViewSpecSer {
    fn from(vs: &ViewSpec) -> Self {
        match vs {
            ViewSpec::Thread { thread_id } => ViewSpecSer::Thread {
                thread_id: thread_id.map(|id| id.0),
            },
            ViewSpec::ThreadList => ViewSpecSer::ThreadList,
            ViewSpec::Remotes => ViewSpecSer::Remotes,
            ViewSpec::Projects => ViewSpecSer::Projects,
            ViewSpec::SpaceBrowser { .. } => ViewSpecSer::SpaceBrowser,
            ViewSpec::Trust => ViewSpecSer::Trust,
            ViewSpec::Graph { .. } => ViewSpecSer::Graph,
            ViewSpec::EventInspector => ViewSpecSer::EventInspector,
        }
    }
}

// ---------------------------------------------------------------------------
// File I/O
// ---------------------------------------------------------------------------

fn config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ai").join("config").join("tui"))
}

/// Load config from disk. Returns default if not found.
pub fn load_config() -> TuiConfig {
    let path = config_dir().map(|d| d.join("config.json"));
    match path {
        Some(p) if p.exists() => {
            let data = std::fs::read_to_string(&p).unwrap_or_default();
            serde_json::from_str(&data).unwrap_or_default()
        }
        _ => TuiConfig::default(),
    }
}

/// Save config to disk.
pub fn save_config(config: &TuiConfig) -> std::io::Result<()> {
    let dir = config_dir()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("config.json");
    let data = serde_json::to_string_pretty(config)?;
    std::fs::write(path, data)
}

/// Save session to disk.
pub fn save_session(
    layout: &LayoutTree,
    tiles: &HashMap<TileId, TileState>,
    focused: TileId,
) -> std::io::Result<()> {
    let dir = config_dir()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"))?;
    std::fs::create_dir_all(&dir)?;

    let session = TuiSession {
        layout: LayoutTreeSer::from(layout),
        tiles: tiles
            .iter()
            .map(|(id, ts)| (id.0, TileStateSer::from(ts)))
            .collect(),
        focused_tile_id: focused.0,
    };

    let path = dir.join("session.json");
    let data = serde_json::to_string_pretty(&session)?;
    std::fs::write(path, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_tui_core::workspace::Workspace;
    use tempfile::TempDir;

    #[test]
    fn session_roundtrip_preserves_layout_and_focus() {
        let ws = Workspace::default_three_pane();
        let layout_ser = LayoutTreeSer::from(&ws.layout);
        let tiles_ser: HashMap<u64, TileStateSer> = ws
            .tiles
            .iter()
            .map(|(id, ts)| (id.0, TileStateSer::from(ts)))
            .collect();

        let json =
            serde_json::to_string_pretty(&(&layout_ser, &tiles_ser, ws.focused_tile.0)).unwrap();

        let (layout_back, tiles_back, focused_back): (
            LayoutTreeSer,
            HashMap<u64, TileStateSer>,
            u64,
        ) = serde_json::from_str(&json).unwrap();

        assert_eq!(focused_back, ws.focused_tile.0);
        assert_eq!(tiles_back.len(), 3);
    }
}
