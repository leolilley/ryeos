//! UI service handler modules.

use ryeos_api::registry::ServiceDescriptor;

pub mod ui_actions_invoke;
pub mod ui_launch;
pub mod ui_launch_mint;
pub mod ui_session_current;

pub const ALL: &[ServiceDescriptor] = &[
    ui_launch::DESCRIPTOR,
    ui_launch_mint::DESCRIPTOR,
    ui_session_current::DESCRIPTOR,
    ui_actions_invoke::DESCRIPTOR,
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
