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
