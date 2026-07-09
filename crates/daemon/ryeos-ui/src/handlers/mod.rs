//! UI service handler modules.

use ryeos_api::registry::ServiceDescriptor;

pub mod ui_dimension;
pub mod ui_files;
pub mod ui_gc;
pub mod ui_graph_topology;
pub mod ui_intents_apply;
pub mod ui_invocations_dispatch;
pub mod ui_items;
pub mod ui_launch;
pub mod ui_launch_mint;
pub mod ui_node;
pub mod ui_projects;
pub mod ui_remotes;
pub mod ui_schedules;
pub mod ui_seat;
pub mod ui_session_current;
pub mod ui_threads;

pub const ALL: &[ServiceDescriptor] = &[
    ui_launch::DESCRIPTOR,
    ui_launch_mint::DESCRIPTOR,
    ui_session_current::DESCRIPTOR,
    ui_intents_apply::DESCRIPTOR,
    ui_invocations_dispatch::DESCRIPTOR,
    ui_graph_topology::DESCRIPTOR,
    ui_dimension::DESCRIPTOR,
    ui_items::ITEMS_LIST_DESCRIPTOR,
    ui_items::ITEM_INSPECT_DESCRIPTOR,
    ui_threads::DESCRIPTOR,
    ui_threads::INSPECT_DESCRIPTOR,
    ui_node::ACTIVITY_DESCRIPTOR,
    ui_schedules::DESCRIPTOR,
    ui_gc::DESCRIPTOR,
    ui_seat::OPEN_DESCRIPTOR,
    ui_seat::APPEND_DESCRIPTOR,
    ui_seat::REPLAY_DESCRIPTOR,
    ui_seat::CLOSE_DESCRIPTOR,
    ui_files::FILES_LIST_DESCRIPTOR,
    ui_files::FILES_READ_DESCRIPTOR,
    ui_files::FILES_TREE_DESCRIPTOR,
    ui_projects::PROJECTS_LIST_DESCRIPTOR,
    ui_projects::PROJECTS_ADD_DESCRIPTOR,
    ui_projects::PROJECTS_FORGET_DESCRIPTOR,
    ui_projects::PROJECTS_RESOLVE_DESCRIPTOR,
    ui_projects::PROJECTS_OPEN_DESCRIPTOR,
    ui_projects::UI_PROJECTS_LIST_DESCRIPTOR,
    ui_projects::UI_PROJECTS_ADD_DESCRIPTOR,
    ui_projects::UI_PROJECTS_FORGET_DESCRIPTOR,
    ui_projects::UI_PROJECTS_RESOLVE_DESCRIPTOR,
    ui_projects::UI_PROJECTS_OPEN_DESCRIPTOR,
    ui_projects::RYEOS_UI_PROJECTS_LIST_DESCRIPTOR,
    ui_projects::RYEOS_UI_PROJECTS_ADD_DESCRIPTOR,
    ui_projects::RYEOS_UI_PROJECTS_FORGET_DESCRIPTOR,
    ui_projects::RYEOS_UI_PROJECTS_RESOLVE_DESCRIPTOR,
    ui_projects::RYEOS_UI_PROJECTS_OPEN_DESCRIPTOR,
    ui_projects::RECENT_TOUCH_DESCRIPTOR,
    ui_projects::RECENT_LIST_DESCRIPTOR,
    ui_projects::CONFIG_GET_DESCRIPTOR,
    ui_projects::CONFIG_UPDATE_DESCRIPTOR,
    ui_remotes::REMOTES_LIST_DESCRIPTOR,
    ui_remotes::REMOTES_PROBE_DESCRIPTOR,
];

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_api::registry::ServiceAvailability;

    #[test]
    fn ui_launch_descriptor_is_daemon_only() {
        let desc = ui_launch::DESCRIPTOR;
        assert!(
            matches!(desc.availability, ServiceAvailability::DaemonOnly),
            "ui.launch must be DaemonOnly (requires UiState from daemon composition)"
        );
    }
}
