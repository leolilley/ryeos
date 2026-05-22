//! Root application model — AppModel ties Store, Workspace, and runtime state.

use crate::animation::AnimationState;
use crate::ids::ExecutionId;
use crate::layout::Rect;
use crate::store::{DaemonStatus, Store};
use crate::workspace::Workspace;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppModel {
    pub store: Store,
    pub workspace: Workspace,
    pub overlay: Option<OverlayState>,
    pub runtime: RuntimeStatus,
    pub visual: VisualState,
    pub generation: u64,
    pub dirty: bool,
}

impl AppModel {
    /// Create a default model for the given project path.
    pub fn new_default(_project_path: &str) -> Self {
        Self {
            store: Store::new(),
            workspace: Workspace::default_three_pane(),
            overlay: None,
            runtime: RuntimeStatus {
                daemon_status: DaemonStatus::Connecting,
                active_execution: None,
                viewport: Rect::new(0, 0, 200, 60),
                started_at_ms: now_ms(),
                last_render_at_ms: 0,
                last_daemon_poll_at_ms: 0,
            },
            visual: VisualState {
                animation: AnimationState::default(),
            },
            generation: 0,
            dirty: true,
        }
    }

    /// Mark the model as dirty (needs re-render).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        self.generation = self.generation.wrapping_add(1);
    }
}

/// Bootstrap status for progressive startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BootstrapStatus {
    Loading,
    Partial,
    Complete,
}

// ---------------------------------------------------------------------------
// Runtime status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    pub daemon_status: DaemonStatus,
    pub active_execution: Option<ExecutionId>,
    pub viewport: Rect,
    pub started_at_ms: i64,
    pub last_render_at_ms: i64,
    pub last_daemon_poll_at_ms: i64,
}

// ---------------------------------------------------------------------------
// Visual state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualState {
    pub animation: AnimationState,
}

// ---------------------------------------------------------------------------
// Overlay state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OverlayState {
    CommandPalette {
        query: String,
        cursor: usize,
    },
    Confirm {
        message: String,
        action: String,
    },
    Help,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_model_default_constructs() {
        let model = AppModel::new_default("/tmp/test");
        assert_eq!(model.store.threads.len(), 0);
        assert_eq!(model.workspace.tiles.len(), 3);
        assert!(model.dirty);
        assert_eq!(model.generation, 0);
    }

    #[test]
    fn mark_dirty_increments_generation() {
        let mut model = AppModel::new_default("/tmp/test");
        model.dirty = false;
        model.mark_dirty();
        assert!(model.dirty);
        assert_eq!(model.generation, 1);
    }
}
