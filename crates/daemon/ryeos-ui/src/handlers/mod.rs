//! UI service handler modules.

use ryeos_api::registry::ServiceDescriptor;

pub mod ui_actions_invoke;
pub mod ui_graph_topology;
pub mod ui_launch;
pub mod ui_launch_mint;
pub mod ui_session_current;
pub mod ui_studio_dimension;
pub mod ui_studio_files;
pub mod ui_studio_gc;
pub mod ui_studio_items;
pub mod ui_studio_node;
pub mod ui_studio_projects;
pub mod ui_studio_remotes;
pub mod ui_studio_schedules;
pub mod ui_studio_seat;
pub mod ui_studio_threads;

pub const ALL: &[ServiceDescriptor] = &[
    ui_launch::DESCRIPTOR,
    ui_launch_mint::DESCRIPTOR,
    ui_session_current::DESCRIPTOR,
    ui_actions_invoke::DESCRIPTOR,
    ui_graph_topology::DESCRIPTOR,
    ui_studio_dimension::DESCRIPTOR,
    ui_studio_items::ITEMS_LIST_DESCRIPTOR,
    ui_studio_items::ITEM_INSPECT_DESCRIPTOR,
    ui_studio_threads::DESCRIPTOR,
    ui_studio_threads::INSPECT_DESCRIPTOR,
    ui_studio_node::ACTIVITY_DESCRIPTOR,
    ui_studio_schedules::DESCRIPTOR,
    ui_studio_gc::DESCRIPTOR,
    ui_studio_seat::OPEN_DESCRIPTOR,
    ui_studio_seat::APPEND_DESCRIPTOR,
    ui_studio_seat::REPLAY_DESCRIPTOR,
    ui_studio_seat::CLOSE_DESCRIPTOR,
    ui_studio_files::FILES_LIST_DESCRIPTOR,
    ui_studio_files::FILES_READ_DESCRIPTOR,
    ui_studio_files::FILES_TREE_DESCRIPTOR,
    ui_studio_projects::PROJECTS_LIST_DESCRIPTOR,
    ui_studio_projects::PROJECTS_ADD_DESCRIPTOR,
    ui_studio_projects::PROJECTS_FORGET_DESCRIPTOR,
    ui_studio_projects::PROJECTS_RESOLVE_DESCRIPTOR,
    ui_studio_projects::PROJECTS_OPEN_DESCRIPTOR,
    ui_studio_projects::UI_PROJECTS_LIST_DESCRIPTOR,
    ui_studio_projects::UI_PROJECTS_ADD_DESCRIPTOR,
    ui_studio_projects::UI_PROJECTS_FORGET_DESCRIPTOR,
    ui_studio_projects::UI_PROJECTS_RESOLVE_DESCRIPTOR,
    ui_studio_projects::UI_PROJECTS_OPEN_DESCRIPTOR,
    ui_studio_projects::STUDIO_PROJECTS_LIST_DESCRIPTOR,
    ui_studio_projects::STUDIO_PROJECTS_ADD_DESCRIPTOR,
    ui_studio_projects::STUDIO_PROJECTS_FORGET_DESCRIPTOR,
    ui_studio_projects::STUDIO_PROJECTS_RESOLVE_DESCRIPTOR,
    ui_studio_projects::STUDIO_PROJECTS_OPEN_DESCRIPTOR,
    ui_studio_projects::RECENT_TOUCH_DESCRIPTOR,
    ui_studio_projects::RECENT_LIST_DESCRIPTOR,
    ui_studio_projects::CONFIG_GET_DESCRIPTOR,
    ui_studio_projects::CONFIG_UPDATE_DESCRIPTOR,
    ui_studio_remotes::REMOTES_LIST_DESCRIPTOR,
    ui_studio_remotes::REMOTES_PROBE_DESCRIPTOR,
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
