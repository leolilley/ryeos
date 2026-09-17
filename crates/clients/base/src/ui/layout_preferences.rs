//! Untrusted, bounded presentation preferences. Never serialize RyeOsCore here:
//! grants, seat authority, observations, pending effects and drafts are excluded.
//! Restoring allocates fresh mounts and revalidates every ref against this session.

use super::model::{RyeOsCore, RyeOsDockContent, RyeOsDockSlotState};
use crate::layout::LayoutTree;
use crate::surface::workspaces::{LayoutSeedSpec, WorkspaceSeedSpec, validate_seeds};
use crate::surface::{SlotContentSpec, SlotSpec, SlotsSpec, ViewKindSpec};
use serde::{Deserialize, Serialize};

pub const MAX_LAYOUT_PREFERENCE_BYTES: usize = 256 * 1024;
const SCHEMA: &str = "ryeos.ui.layout-preferences.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    principal: String,
    surface: String,
    project: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    schema: String,
    scope: Scope,
    active_workspace: usize,
    workspaces: Vec<SavedWorkspace>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedWorkspace {
    seed: WorkspaceSeedSpec,
    focused_view: Option<usize>,
}

fn slot(slot: &Option<RyeOsDockSlotState>) -> Option<SlotSpec> {
    slot.as_ref().map(|slot| SlotSpec {
        content: match &slot.content {
            RyeOsDockContent::View { view_ref } => SlotContentSpec::View(view_ref.clone()),
        },
        open: slot.visible,
        size: slot.size,
    })
}

impl RyeOsCore {
    fn preference_scope(&self) -> Result<Scope, String> {
        let session = self
            .data
            .session
            .as_ref()
            .ok_or("layout preferences require a session")?;
        let principal = session
            .user_principal_id
            .as_ref()
            .filter(|id| !id.is_empty())
            .ok_or("layout preferences require an authenticated principal")?;
        Ok(Scope {
            principal: principal.clone(),
            surface: session.surface_ref.clone(),
            project: session.project_path.clone(),
        })
    }

    pub fn layout_preference_key(&self) -> Result<String, String> {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(&self.preference_scope()?).map_err(|e| e.to_string())?;
        Ok(format!("{SCHEMA}:{:x}", Sha256::digest(bytes)))
    }

    pub fn export_layout_preferences(&self) -> Result<String, String> {
        fn capture(
            tree: &LayoutTree,
            workspace: &crate::workspace::Workspace,
        ) -> Result<LayoutSeedSpec, String> {
            Ok(match tree {
                LayoutTree::Group { tabs, active, .. } => LayoutSeedSpec::Group {
                    views: tabs
                        .iter()
                        .map(|id| {
                            let tile = workspace
                                .tiles
                                .get(id)
                                .ok_or("layout references an unmounted view")?;
                            serde_json::from_value::<ViewKindSpec>(serde_json::Value::String(
                                tile.view.view_ref.clone(),
                            ))
                            .map_err(|e| e.to_string())
                        })
                        .collect::<Result<_, String>>()?,
                    active: tabs
                        .iter()
                        .position(|id| id == active)
                        .ok_or("invalid active view")?,
                },
                LayoutTree::Split {
                    axis,
                    ratio,
                    first,
                    second,
                } => LayoutSeedSpec::Split {
                    axis: *axis,
                    ratio: *ratio,
                    first: Box::new(capture(first, workspace)?),
                    second: Box::new(capture(second, workspace)?),
                },
            })
        }
        let workspaces = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                Ok(SavedWorkspace {
                    seed: WorkspaceSeedSpec {
                        id: format!("workspace-{index}"),
                        title: workspace.title.clone(),
                        root: workspace
                            .root
                            .as_ref()
                            .map(|root| capture(root, workspace))
                            .transpose()?,
                        slots: SlotsSpec {
                            top: slot(&workspace.docks.top),
                            bottom: slot(&workspace.docks.bottom),
                            left: slot(&workspace.docks.left),
                            right: slot(&workspace.docks.right),
                        },
                    },
                    focused_view: workspace
                        .tile_ids()
                        .iter()
                        .position(|id| *id == workspace.focused_tile),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let snapshot = Preferences {
            schema: SCHEMA.into(),
            scope: self.preference_scope()?,
            active_workspace: self.active_workspace,
            workspaces,
        };
        let encoded = serde_json::to_string(&snapshot).map_err(|e| e.to_string())?;
        if encoded.len() > MAX_LAYOUT_PREFERENCE_BYTES {
            return Err("layout preferences exceed byte limit".into());
        }
        Ok(encoded)
    }

    pub fn restore_layout_preferences(
        &mut self,
        encoded: &str,
    ) -> Result<Vec<super::effect::RyeOsEffect>, String> {
        // This restores presentation only; it has no authority to discard live
        // input. In particular, a later browser/client caller must not turn the
        // startup restore API into an implicit "discard all drafts" action.
        if self.workspaces.iter().any(|workspace| {
            workspace
                .input_buffers
                .values()
                .any(|input| !input.text.is_empty())
        }) {
            return Err(
                "layout restoration would discard current input; clear it explicitly first".into(),
            );
        }
        if encoded.len() > MAX_LAYOUT_PREFERENCE_BYTES {
            return Err("layout preferences exceed byte limit".into());
        }
        let snapshot: Preferences = serde_json::from_str(encoded)
            .map_err(|e| format!("invalid layout preferences: {e}"))?;
        if snapshot.schema != SCHEMA || snapshot.scope != self.preference_scope()? {
            return Err("layout preferences have a different schema or session scope".into());
        }
        if snapshot.workspaces.is_empty() || snapshot.active_workspace >= snapshot.workspaces.len()
        {
            return Err("layout preferences have no valid active workspace".into());
        }
        validate_seeds(
            &snapshot
                .workspaces
                .iter()
                .map(|saved| saved.seed.clone())
                .collect::<Vec<_>>(),
        )?;
        let tiling = self.workspaces[self.active_workspace].tiling.clone();
        let mut restored = Vec::with_capacity(snapshot.workspaces.len());
        for saved in snapshot.workspaces {
            let mut workspace = saved.seed.instantiate(&tiling)?;
            for tile in workspace.tiles.values() {
                if !self.views.contains_key(&tile.view.view_ref) {
                    return Err(format!(
                        "saved view is not admitted: {}",
                        tile.view.view_ref
                    ));
                }
            }
            // Closed slots need the same check as visible slots: they may be
            // revealed later, so hiding cannot smuggle an unadmitted view in.
            for slot in [
                &workspace.docks.top,
                &workspace.docks.bottom,
                &workspace.docks.left,
                &workspace.docks.right,
            ]
            .into_iter()
            .flatten()
            {
                let RyeOsDockContent::View { view_ref } = &slot.content;
                if !self.views.contains_key(view_ref) {
                    return Err(format!("saved slot is not admitted: {view_ref}"));
                }
            }
            if let Some(index) = saved.focused_view {
                let id = *workspace
                    .tile_ids()
                    .get(index)
                    .ok_or("saved focus is outside the workspace")?;
                workspace.focus_tile(id);
            }
            restored.push(workspace);
        }
        // Complete validation precedes replacement. This operation never replays
        // an input/command, mutates the seat route or imports saved privileges.
        self.workspaces = restored;
        self.active_workspace = snapshot.active_workspace;
        Ok(self.refresh_workspace_sources())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::model::{BrowserSession, BrowserViewport, RyeOsInputState};
    use serde_json::json;

    fn core() -> RyeOsCore {
        RyeOsCore::new(
            BrowserSession {
                ui_binding_contract_revision: crate::UI_BINDING_CONTRACT_REVISION.into(),
                session_id: "session:test".into(),
                user_principal_id: Some("fp:operator".into()),
                surface_ref: "surface:test/work".into(),
                effective_surface: Some(
                    json!({ "name": "Work", "tiles": ["view:test/one", "view:test/two"],
                "views": { "view:test/one": { "widget": "text" }, "view:test/two": { "widget": "text" } } }),
                ),
                ..Default::default()
            },
            BrowserViewport::default(),
            0,
        )
    }

    #[test]
    fn arrangement_round_trip_allocates_fresh_ids_and_never_saves_drafts() {
        let mut source = core();
        source.workspaces[0].input_buffers.insert(
            "test".into(),
            RyeOsInputState {
                text: "private-unsent-draft".into(),
                ..Default::default()
            },
        );
        let ids = source.workspaces[0].tile_ids();
        source.workspaces[0].move_tile_to_group(ids[1], ids[0], 1);
        let encoded = source.export_layout_preferences().unwrap();
        assert!(!encoded.contains("private-unsent-draft"));
        assert!(!encoded.contains("binding_digest"));
        let mut target = core();
        let seat_before = target.seat.fold().snapshot();
        let effects = target.restore_layout_preferences(&encoded).unwrap();
        assert!(effects.iter().all(|effect| !matches!(
            effect.kind,
            super::super::effect::RyeOsEffectKind::InvokeBinding { .. }
        )));
        assert_eq!(target.seat.fold().snapshot(), seat_before);
        assert_eq!(
            target.workspaces[0]
                .root
                .as_ref()
                .unwrap()
                .active_tile_ids()
                .len(),
            1
        );
        assert!(target.workspaces[0].input_buffers.is_empty());
        assert!(
            target.workspaces[0]
                .tile_ids()
                .iter()
                .all(|id| !ids.contains(id))
        );
    }

    #[test]
    fn refused_preferences_leave_live_layout_unchanged() {
        let mut target = core();
        let original = target.workspaces[0].root.clone();
        let mut saved: serde_json::Value =
            serde_json::from_str(&target.export_layout_preferences().unwrap()).unwrap();
        saved["scope"]["principal"] = json!("fp:someone-else");
        assert!(
            target
                .restore_layout_preferences(&saved.to_string())
                .is_err()
        );
        assert_eq!(target.workspaces[0].root, original);
        let mut saved: serde_json::Value =
            serde_json::from_str(&target.export_layout_preferences().unwrap()).unwrap();
        saved["workspaces"][0]["seed"]["slots"] =
            json!({"left": {"content": "view:unadmitted", "open": false, "size": 20}});
        assert!(
            target
                .restore_layout_preferences(&saved.to_string())
                .is_err()
        );
        assert_eq!(target.workspaces[0].root, original);
        assert!(
            target
                .restore_layout_preferences(&" ".repeat(MAX_LAYOUT_PREFERENCE_BYTES + 1))
                .is_err()
        );
    }

    #[test]
    fn restore_does_not_discard_input_in_an_inactive_workspace() {
        let mut target = core();
        let saved = target.export_layout_preferences().unwrap();
        target.workspaces[0].input_buffers.insert(
            "draft".into(),
            RyeOsInputState {
                text: "keep me".into(),
                ..Default::default()
            },
        );
        target.new_workspace();
        let ids: Vec<_> = target
            .workspaces
            .iter()
            .map(|workspace| workspace.id)
            .collect();
        assert!(target.restore_layout_preferences(&saved).is_err());
        assert_eq!(
            target
                .workspaces
                .iter()
                .map(|workspace| workspace.id)
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(target.workspaces[0].input_buffers["draft"].text, "keep me");
    }
}
