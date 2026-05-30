//! UI service handler modules.

use ryeos_api::registry::ServiceDescriptor;

pub mod ui_actions_invoke;
pub mod ui_cockpit_files;
pub mod ui_cockpit_gc;
pub mod ui_cockpit_items;
pub mod ui_cockpit_remotes;
pub mod ui_cockpit_schedules;
pub mod ui_cockpit_snapshot;
pub mod ui_cockpit_threads;
pub mod ui_graph_topology;
pub mod ui_launch;
pub mod ui_launch_mint;
pub mod ui_session_current;

pub const ALL: &[ServiceDescriptor] = &[
    ui_launch::DESCRIPTOR,
    ui_launch_mint::DESCRIPTOR,
    ui_session_current::DESCRIPTOR,
    ui_actions_invoke::DESCRIPTOR,
    ui_graph_topology::DESCRIPTOR,
    ui_cockpit_snapshot::STUDIO_DESCRIPTOR,
    ui_cockpit_items::STUDIO_ITEMS_LIST_DESCRIPTOR,
    ui_cockpit_items::STUDIO_ITEM_INSPECT_DESCRIPTOR,
    ui_cockpit_threads::STUDIO_DESCRIPTOR,
    ui_cockpit_threads::STUDIO_INSPECT_DESCRIPTOR,
    ui_cockpit_schedules::STUDIO_DESCRIPTOR,
    ui_cockpit_gc::STUDIO_DESCRIPTOR,
    ui_cockpit_files::STUDIO_FILES_LIST_DESCRIPTOR,
    ui_cockpit_files::STUDIO_FILES_READ_DESCRIPTOR,
    ui_cockpit_snapshot::DESCRIPTOR,
    ui_cockpit_items::ITEMS_LIST_DESCRIPTOR,
    ui_cockpit_items::ITEM_INSPECT_DESCRIPTOR,
    ui_cockpit_threads::DESCRIPTOR,
    ui_cockpit_threads::INSPECT_DESCRIPTOR,
    ui_cockpit_schedules::DESCRIPTOR,
    ui_cockpit_gc::DESCRIPTOR,
    ui_cockpit_remotes::REMOTES_LIST_DESCRIPTOR,
    ui_cockpit_remotes::REMOTES_PROBE_DESCRIPTOR,
    ui_cockpit_files::FILES_LIST_DESCRIPTOR,
    ui_cockpit_files::FILES_READ_DESCRIPTOR,
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
