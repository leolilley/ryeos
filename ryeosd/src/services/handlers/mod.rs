//! Per-endpoint service handler modules.
//!
//! Each submodule exports a typed `Request`, an async `handle()` body,
//! and a `DESCRIPTOR: ServiceDescriptor` registry record. `ALL` is the
//! canonical list consumed by `build_service_registry()` at daemon startup.
//!
//! Former in-process services `fetch`, `verify`, and
//! `identity/public_key` have been converted to external tools
//! (`tool:ryeos/core/fetch`, `tool:ryeos/core/verify`,
//! `tool:ryeos/core/identity/public_key`). Their handler modules have
//! been deleted; the tool YAMLs invoke `bin:ryos-core-tools`.

use crate::service_registry::ServiceDescriptor;

pub mod bundle_install;
pub mod bundle_list;
pub mod bundle_remove;
pub mod commands_submit;
pub mod events_chain_replay;
pub mod events_replay;
pub mod health_status;
pub mod identity_public_key;
pub mod maintenance_gc;
pub mod rebuild;
pub mod node_sign;
pub mod scheduler_deregister;
pub mod scheduler_list;
pub mod scheduler_pause;
pub mod scheduler_register;
pub mod scheduler_resume;
pub mod scheduler_show_fires;
pub mod system_status;
pub mod threads_cancel;
pub mod threads_chain;
pub mod threads_children;
pub mod threads_get;
pub mod threads_list;

pub(crate) fn default_list_limit() -> usize { 50 }
pub(crate) fn default_replay_limit() -> usize { 200 }

pub const ALL: &[ServiceDescriptor] = &[
    health_status::DESCRIPTOR,
    identity_public_key::DESCRIPTOR,
    system_status::DESCRIPTOR,
    threads_list::DESCRIPTOR,
    threads_get::DESCRIPTOR,
    threads_cancel::DESCRIPTOR,
    threads_children::DESCRIPTOR,
    threads_chain::DESCRIPTOR,
    events_replay::DESCRIPTOR,
    events_chain_replay::DESCRIPTOR,
    commands_submit::DESCRIPTOR,
    bundle_install::DESCRIPTOR,
    bundle_list::DESCRIPTOR,
    bundle_remove::DESCRIPTOR,
    maintenance_gc::DESCRIPTOR,
    rebuild::DESCRIPTOR,
    node_sign::DESCRIPTOR,
    scheduler_register::DESCRIPTOR,
    scheduler_deregister::DESCRIPTOR,
    scheduler_list::DESCRIPTOR,
    scheduler_show_fires::DESCRIPTOR,
    scheduler_pause::DESCRIPTOR,
    scheduler_resume::DESCRIPTOR,
];
