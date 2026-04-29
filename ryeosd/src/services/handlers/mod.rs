//! Per-endpoint service handler modules.
//!
//! Each submodule exports a typed `Request`, an async `handle()` body,
//! and a `DESCRIPTOR: ServiceDescriptor` registry record. `ALL` is the
//! canonical list consumed by `build_service_registry()` at daemon startup.
//!
//! Former in-process services `fetch`, `verify`, and
//! `identity/public_key` have been converted to external tools
//! (`tool:rye/core/fetch`, `tool:rye/core/verify`,
//! `tool:rye/core/identity/public_key`). Their handler modules have
//! been deleted; the tool YAMLs invoke `bin:rye-inspect`.

use crate::service_registry::ServiceDescriptor;

pub mod bundle_install;
pub mod bundle_list;
pub mod bundle_remove;
pub mod commands_submit;
pub mod events_chain_replay;
pub mod events_replay;
pub mod maintenance_gc;
pub mod rebuild;
pub mod node_sign;
pub mod system_status;
pub mod threads_chain;
pub mod threads_children;
pub mod threads_get;
pub mod threads_list;

pub(crate) fn default_list_limit() -> usize { 50 }
pub(crate) fn default_replay_limit() -> usize { 200 }

pub const ALL: &[ServiceDescriptor] = &[
    system_status::DESCRIPTOR,
    threads_list::DESCRIPTOR,
    threads_get::DESCRIPTOR,
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
];
