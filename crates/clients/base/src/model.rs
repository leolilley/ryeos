//! Root application model — AppModel ties Store, Workspace, and runtime state.

use crate::animation::AnimationState;
use crate::atlas::AtlasUiStateVm;
use crate::ids::ExecutionId;
use crate::layout::Rect;
use crate::store::{DaemonStatus, Store};
use crate::surface::LoadedSurface;
use crate::surface::SurfaceSpec;
use crate::workspace::Workspace;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppModel {
    pub store: Store,
    pub workspace: Workspace,
    pub surface: LoadedSurfaceSerde,
    pub overlay: Option<OverlayState>,
    pub runtime: RuntimeStatus,
    pub visual: VisualState,
    pub generation: u64,
    pub dirty: bool,
    /// Active keymap (built from defaults + config overrides).
    /// Not serialized — rebuilt at startup from config.
    #[serde(skip)]
    pub keymap: crate::commands::Keymap,
}

/// Serializable wrapper for LoadedSurface.
/// Retains the SurfaceSpec so reset_layout can rebuild from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedSurfaceSerde {
    pub spec: SurfaceSpec,
    pub name: String,
    pub source_label: String,
    pub is_trusted: bool,
    pub is_local_preview: bool,
    /// Canonical ref or path the surface was resolved from.
    #[serde(default)]
    pub requested_ref: Option<String>,
}

impl LoadedSurfaceSerde {
    pub fn from_loaded(loaded: &LoadedSurface) -> Self {
        Self {
            spec: loaded.spec().clone(),
            name: loaded.spec().name.clone(),
            source_label: loaded.source_label().to_string(),
            is_trusted: loaded.is_trusted(),
            is_local_preview: loaded.is_local_preview(),
            requested_ref: loaded.requested_ref().map(String::from),
        }
    }

    /// Rebuild the workspace from the retained surface spec.
    pub fn rebuild_workspace(&self) -> Workspace {
        self.spec.to_workspace()
    }
}

impl AppModel {
    /// Create a default model for the given project path.
    pub fn new_default(_project_path: &str) -> Self {
        let spec = crate::surface::builtin_default();
        Self {
            store: Store::new(),
            workspace: spec.to_workspace(),
            surface: LoadedSurfaceSerde {
                spec,
                name: "studio-base".into(),
                source_label: "builtin".into(),
                is_trusted: false,
                is_local_preview: false,
                requested_ref: None,
            },
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
                atlas: AtlasUiStateVm::default(),
            },
            generation: 0,
            dirty: true,
            keymap: crate::commands::Keymap::defaults(),
        }
    }

    /// Create a model with a loaded surface.
    pub fn from_surface(_project_path: &str, loaded: &LoadedSurface) -> Self {
        let workspace = loaded.spec().to_workspace();
        let surface_info = LoadedSurfaceSerde::from_loaded(loaded);
        Self {
            store: Store::new(),
            workspace,
            surface: surface_info,
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
                atlas: AtlasUiStateVm::default(),
            },
            generation: 0,
            dirty: true,
            keymap: crate::commands::Keymap::defaults(),
        }
    }

    /// Mark the model as dirty (needs re-render).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Effective command registry for the active surface.
    ///
    /// Built-ins are the baseline; surface-declared `commands` can
    /// override by id or add new entries. Palette rendering and command
    /// dispatch both use this merged registry.
    pub fn active_affordances(&self) -> Vec<crate::commands::Affordance> {
        let (affordances, _warnings) =
            crate::commands::merge_affordances(&self.surface.spec.affordances);
        affordances
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
    #[serde(default)]
    pub atlas: AtlasUiStateVm,
}

// ---------------------------------------------------------------------------
// Overlay state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OverlayState {
    CommandPalette { query: String, selected: usize },
    Confirm { message: String, action: String },
    Help,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        0
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
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

    #[test]
    fn from_surface_uses_loaded_workspace() {
        let loaded = crate::surface::LoadedSurface::Builtin {
            spec: crate::surface::builtin_default(),
        };
        let model = AppModel::from_surface("/tmp/test", &loaded);
        assert_eq!(model.workspace.tiles.len(), 3);
        assert_eq!(model.surface.name, "studio-base");
    }

    #[test]
    fn active_affordances_use_surface_commands() {
        let mut spec = crate::surface::builtin_default();
        spec.affordances.push(crate::surface::SurfaceCommandSpec {
            id: "surface.only".into(),
            label: "Surface Only".into(),
            category: "Surface".into(),
            description: "Declared by the effective surface".into(),
            invoke: Some(crate::commands::InvocationSpec::Ui(
                crate::commands::UiInvocation {
                    verb: crate::commands::UiVerb::ToggleHelp,
                    args: serde_json::Value::Null,
                },
            )),
            requires_capabilities: Vec::new(),
        });
        let loaded = crate::surface::LoadedSurface::Builtin { spec };
        let model = AppModel::from_surface("/tmp/test", &loaded);

        let affordances = model.active_affordances();
        let surface_command = affordances
            .iter()
            .find(|affordance| affordance.id == "surface.only")
            .expect("surface command should be in active affordance registry");
        assert_eq!(surface_command.label, "Surface Only");
    }
}
