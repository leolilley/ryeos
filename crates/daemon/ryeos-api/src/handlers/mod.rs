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
//! been deleted; the tool YAMLs invoke `bin:ryeos-core-tools`.

use crate::registry::ServiceDescriptor;

pub mod admission_attestations_for_subject;
pub mod admission_claim;
pub mod admission_status;
pub mod admission_submit;
pub mod authorize_key;
pub mod bundle_export;
pub mod bundle_install;
pub mod bundle_list;
pub mod bundle_remove;
pub mod commands_dispatch;
pub mod commands_list;
pub mod commands_submit;
pub mod events_chain_replay;
pub mod events_replay;
pub mod federation_capabilities;
pub mod federation_heads_list;
pub mod health_status;
pub mod identity_public_key;
pub mod ingest_ignore;
pub mod items_effective;
pub mod maintenance_gc;
pub mod node_sign;
pub mod objects_closure_describe;
pub mod objects_closure_get;
pub mod objects_get;
pub mod objects_has;
pub mod objects_put;
pub mod project_apply_snapshot;
pub mod project_status;
pub mod push_head;
pub mod rebuild;
pub mod remote_admit;
pub mod remote_authorize;
pub mod remote_bind_project;
pub mod remote_bundle_install;
pub mod remote_configure;
pub mod remote_doctor;
pub mod remote_execute;
pub mod remote_import_admitted_head;
pub mod remote_import_admitted_root;
pub mod remote_list;
pub mod remote_project_status;
pub mod remote_pull;
pub mod remote_push;
pub mod remote_run;
pub mod remote_status;
pub mod remote_sync_admitted_heads;
pub mod remote_sync_project_ai;
pub mod remote_thread_status;
pub mod remote_threads;
pub mod remote_vault_delete;
pub mod remote_vault_list;
pub mod remote_vault_set;
pub mod scheduler_deregister;
pub mod scheduler_explain;
pub mod scheduler_list;
pub mod scheduler_pause;
pub mod scheduler_register;
pub mod scheduler_resume;
pub mod scheduler_show_fires;
pub mod seat;
pub mod sync_jobs_inspect;
pub mod sync_jobs_list;
pub mod system_routes;
pub mod system_status;
pub mod threads_cancel;
pub mod threads_chain;
pub mod threads_children;
pub mod threads_get;
pub mod threads_input;
pub mod threads_list;
pub mod usage_summary;
pub mod vault_delete;
pub mod vault_list;
pub mod vault_set;

pub(crate) fn default_list_limit() -> usize {
    50
}
pub(crate) fn default_replay_limit() -> usize {
    200
}

pub const ALL: &[ServiceDescriptor] = &[
    admission_claim::DESCRIPTOR,
    admission_submit::DESCRIPTOR,
    admission_status::DESCRIPTOR,
    admission_attestations_for_subject::DESCRIPTOR,
    federation_capabilities::DESCRIPTOR,
    federation_heads_list::DESCRIPTOR,
    health_status::DESCRIPTOR,
    identity_public_key::DESCRIPTOR,
    system_status::DESCRIPTOR,
    system_routes::DESCRIPTOR,
    ingest_ignore::DESCRIPTOR,
    objects_has::DESCRIPTOR,
    objects_put::DESCRIPTOR,
    objects_get::DESCRIPTOR,
    objects_closure_describe::DESCRIPTOR,
    objects_closure_get::DESCRIPTOR,
    push_head::DESCRIPTOR,
    project_apply_snapshot::DESCRIPTOR,
    project_status::DESCRIPTOR,
    threads_list::DESCRIPTOR,
    threads_get::DESCRIPTOR,
    threads_cancel::DESCRIPTOR,
    threads_children::DESCRIPTOR,
    commands_dispatch::DESCRIPTOR,
    commands_list::DESCRIPTOR,
    threads_chain::DESCRIPTOR,
    seat::OPEN_DESCRIPTOR,
    seat::APPEND_DESCRIPTOR,
    seat::CLOSE_DESCRIPTOR,
    threads_input::DESCRIPTOR,
    usage_summary::DESCRIPTOR,
    events_replay::DESCRIPTOR,
    events_chain_replay::DESCRIPTOR,
    commands_submit::DESCRIPTOR,
    bundle_install::DESCRIPTOR,
    bundle_export::DESCRIPTOR,
    bundle_list::DESCRIPTOR,
    bundle_remove::DESCRIPTOR,
    maintenance_gc::DESCRIPTOR,
    rebuild::DESCRIPTOR,
    node_sign::DESCRIPTOR,
    authorize_key::DESCRIPTOR,
    scheduler_register::DESCRIPTOR,
    scheduler_deregister::DESCRIPTOR,
    scheduler_explain::DESCRIPTOR,
    scheduler_list::DESCRIPTOR,
    scheduler_show_fires::DESCRIPTOR,
    scheduler_pause::DESCRIPTOR,
    scheduler_resume::DESCRIPTOR,
    sync_jobs_list::DESCRIPTOR,
    sync_jobs_inspect::DESCRIPTOR,
    remote_configure::DESCRIPTOR,
    remote_bind_project::DESCRIPTOR,
    remote_doctor::DESCRIPTOR,
    remote_list::DESCRIPTOR,
    remote_status::DESCRIPTOR,
    remote_push::DESCRIPTOR,
    remote_sync_project_ai::DESCRIPTOR,
    remote_project_status::DESCRIPTOR,
    remote_pull::DESCRIPTOR,
    remote_execute::DESCRIPTOR,
    remote_import_admitted_head::DESCRIPTOR,
    remote_import_admitted_root::DESCRIPTOR,
    remote_sync_admitted_heads::DESCRIPTOR,
    remote_run::DESCRIPTOR,
    remote_admit::DESCRIPTOR,
    remote_authorize::DESCRIPTOR,
    remote_threads::DESCRIPTOR,
    remote_thread_status::DESCRIPTOR,
    remote_bundle_install::DESCRIPTOR,
    remote_vault_set::DESCRIPTOR,
    remote_vault_list::DESCRIPTOR,
    remote_vault_delete::DESCRIPTOR,
    vault_set::DESCRIPTOR,
    vault_list::DESCRIPTOR,
    vault_delete::DESCRIPTOR,
    items_effective::DESCRIPTOR,
];
