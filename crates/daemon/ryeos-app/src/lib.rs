/// Owner token carried by the secure temporaries this crate publishes.
pub(crate) const APP_TEMP_PURPOSE: lillux::TempPurpose = lillux::TempPurpose::new("app");

// ryeos-app: shared application state and domain services.
//
// Phase B: extraction in progress. Modules are added as files are moved
// out of ryeosd. Each move keeps the workspace compiling.

pub mod accounting_anchor;
pub mod accounting_db;
pub mod admission_events;
pub mod admitted_program_trust;
pub mod authoritative_root_fact;
pub mod build_info;
pub mod bundle_event_service;
pub mod bundle_publication;
pub mod bundle_set_transaction;
pub mod bundle_transaction;
pub mod callback_token;
pub mod cascade;
pub mod command_hub;
pub mod command_invocation;
pub mod command_service;
pub mod dedicated_session_service;
pub mod effective_program_preparation;
pub mod engine_cache;
pub mod env_contract;
pub mod event_store_service;
pub mod event_stream;
pub mod execution_comparison;
pub mod execution_history_reset;
#[cfg(test)]
mod no_forget_gate;
pub mod execution_identity_probe;
pub mod execution_policy;
pub mod execution_provenance;
pub mod execution_realization;
pub mod execution_resources;
pub mod extension_state;
pub mod external_artifacts;
pub mod external_candidate_import;
pub mod external_connector;
pub mod external_content_admission;
mod external_lifecycle_adapter;
pub mod external_placement;
pub(crate) mod external_supervisor_native;
pub mod external_qualification_command;
pub mod external_qualification_guest_scratch;
pub mod external_qualification_invocation;
pub mod external_qualification_local;
pub mod external_qualification_transcript;
pub mod federated_follow;
pub mod handler_context;
pub mod handler_error;
pub mod hosted_candidate_result;
pub mod hosted_operation;
pub mod ignore;
#[path = "io/mod.rs"]
pub mod io;
pub mod kind_profiles;
pub mod launch_metadata;
pub mod launch_stage_timings;
pub mod live_input_queue;
pub mod managed_external_content;
pub mod managed_external_content_operation;
pub mod node_config;
pub mod object_contracts;
pub mod operator_authority;
pub mod offline_native;
pub mod operator_external_content;
pub mod operator_guest_runtime_materialization;
pub mod operator_runtime_snapshot;
pub mod persistent_session;
pub mod persistent_session_admission;
pub mod prepared_evidence_attachment;
pub mod principal;
pub mod private_artifact_home;
pub mod process;
pub mod projection_health;
pub mod projection_repair;
pub mod prospective_admission;
mod qualification_consumer_observation;
pub mod qualification_workspace_custody;
pub mod recovery_execution_gate;
pub mod resolution_cache;
mod retained_bundle_evidence;
pub mod route_diagnostics;
pub mod route_raw;
pub mod runtime_capability_admission;
pub mod runtime_db;
pub mod runtime_item_author_service;
pub mod runtime_project_snapshot_service;
pub mod runtime_vault_service;
pub mod scheduled_execution_authority;
pub mod scheduler_boundary;
pub mod scoped_producer_authority;
pub mod scoped_producer_io;
pub mod scoped_producer_observe;
pub mod scoped_producer_process;
pub mod scoped_producer_start;
pub mod scoped_producer_stop;
mod selected_invocation_bookkeeping;
pub use selected_invocation_bookkeeping::{
    SelectedBorrowReturn, SelectedBorrowTicket, SelectedGeneration, SelectedTicket,
};
pub mod service_registry;
pub mod source_closure_admission;
pub mod standalone_audit;
pub mod state;
pub mod standalone_native;
pub mod state_store;
pub mod stream_envelope;
pub mod temp_dir_guard;
pub mod thread_lifecycle;
pub mod vault;
pub mod worker_handoff;
pub mod write_barrier;

// Original bearer custody only; opaque admission is issued separately.
pub use callback_token::{DedicatedWorkerCredentialTransport, SelectedDedicatedWorkerAdmission};

/// Native-gate test builders for other crates' tests: the Runtime store, the
/// node State and, through the State, its Operational store, each opened the
/// way a live node opens it and closed into a test host's census.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    pub use crate::runtime_db::test_native::{
        TestRuntime, live_open_refusal as runtime_live_open_refusal, open_live as open_live_runtime,
        read_existing as read_existing_runtime, retire as retire_runtime, seed_node,
    };
    pub use crate::state_store::test_native::{
        TEST_STATE_STORE_ENVELOPE, TestAccountingDb, TestStateStore, with_store_host,
    };
    pub use ryeos_state::test_support::{
        TEST_STATE_ENVELOPE, TestState, live_state_open_refusal, open_live_state,
        open_live_state_with_repair_sink, release_shared_test_host, retire_live_state, seed_node_state,
        settle_refused_open, shared_test_host,
    };
}
