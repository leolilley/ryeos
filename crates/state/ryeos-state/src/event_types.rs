//! Canonical wire strings for runtime event types — the single source of truth.
//!
//! `ryeos-state` is the lowest layer that both the projection (here) and the
//! higher-level `RuntimeEventType` enum (`ryeos-runtime`, which depends on this
//! crate) share. Defining the strings here lets the enum map its variants to
//! these constants AND lets the projection match against them, so neither side
//! spells a literal and a wire-string change happens in exactly one place.

/// Defines each wire-string constant and `ALL`, the list of every one of them,
/// from a single declaration so a constant cannot exist without being listed.
macro_rules! event_types {
    ($($(#[$meta:meta])* $name:ident = $value:literal;)*) => {
        $($(#[$meta])* pub const $name: &str = $value;)*

        /// Every event type registered here, in declaration order.
        pub const ALL: &[&str] = &[$($name),*];
    };
}

event_types! {
    THREAD_CREATED = "thread_created";
    THREAD_STARTED = "thread_started";
    THREAD_COMPLETED = "thread_completed";
    THREAD_FAILED = "thread_failed";
    THREAD_CANCELLED = "thread_cancelled";
    THREAD_KILLED = "thread_killed";
    THREAD_TIMED_OUT = "thread_timed_out";
    THREAD_CONTINUED = "thread_continued";

    EDGE_RECORDED = "edge_recorded";
    CHILD_THREAD_SPAWNED = "child_thread_spawned";
    /// Written to a following parent when a followed child settles: the child,
    /// its follow, status, closed `outcome_kind`, a bounded result summary and
    /// cost. One per followed child, keyed by the child thread id.
    CHILD_THREAD_SETTLED = "child_thread_settled";
    /// Written to a worker placement admitted to continue a completed
    /// predecessor's upstream conversation: the predecessor placement and the
    /// upstream session id the successor was admitted with. One per successor.
    WORKER_CONTINUATION_ADMITTED = "worker_continuation_admitted";
    CONTINUATION_REQUESTED = "continuation_requested";
    CONTINUATION_ACCEPTED = "continuation_accepted";
    COMMAND_SUBMITTED = "command_submitted";
    COMMAND_CLAIMED = "command_claimed";
    COMMAND_COMPLETED = "command_completed";

    STREAM_OPENED = "stream_opened";
    TOKEN_DELTA = "token_delta";
    STREAM_SNAPSHOT = "stream_snapshot";
    STREAM_CLOSED = "stream_closed";
    /// Daemon projection of a validated subprocess frame. Multiple bounded pieces
    /// of one frame are committed atomically; this is observation, not exit authority.
    SUBPROCESS_OUTPUT_OBSERVED = "subprocess_output_observed";
    OBSERVATION = "observation";
    /// Daemon-authored durable result of a hook declared as `observation`.
    HOOK_OBSERVATION_RECORDED = "hook_observation_recorded";
    /// Daemon-authored classified failure for an observation hook.
    HOOK_FAILED = "hook_failed";
    /// Daemon-authored, source-scoped project observation with a stable identity.
    PROJECT_OBSERVATION_RECORDED = "project_observation_recorded";
    PROVIDER_CALL_OBSERVATION_RECORDED = "provider_call_observation_recorded";

    ARTIFACT_PUBLISHED = "artifact_published";
    AS_LAUNCHED_RESOLUTION = "as_launched_resolution";
    AS_LAUNCHED_REF_BINDINGS = "as_launched_ref_bindings";
    RUNTIME_LAUNCH_FACTS = "runtime_launch_facts";
    LAUNCH_AUGMENTATION_CACHE_HIT = "launch_augmentation_cache_hit";
    /// Daemon-authored, once per launch attempt: the kind of root the process's
    /// inputs were laid out in (`live`, `sparse_private`, `pinned`, ...). Names no path.
    PROCESS_INPUT_ROOT_SELECTED = "process_input_root_selected";
    THREAD_FACET_SET = "thread_facet_set";

    COGNITION_IN = "cognition_in";
    COGNITION_OUT = "cognition_out";
    COGNITION_REASONING = "cognition_reasoning";
    COGNITION_FRAME = "cognition_frame";
    COGNITION_COMPACTION = "cognition_compaction";

    TOOL_CALL_START = "tool_call_start";
    TOOL_CALL_RESULT = "tool_call_result";

    GRAPH_STARTED = "graph_started";
    GRAPH_COMPLETED = "graph_completed";
    GRAPH_STEP_STARTED = "graph_step_started";
    GRAPH_STEP_COMPLETED = "graph_step_completed";
    GRAPH_BRANCH_TAKEN = "graph_branch_taken";
    GRAPH_FOREACH_STARTED = "graph_foreach_started";
    GRAPH_FOREACH_ITERATION = "graph_foreach_iteration";
    GRAPH_FOLLOW_SUSPENDED = "graph_follow_suspended";
    GRAPH_NODE_RETRY = "graph_node_retry";

    PROVIDER_RETRY = "provider_retry";
    PROVIDER_ATTEMPT_BUDGET_TRANSITION_V1 = "provider_attempt_budget_transition_v1";
    RESOURCE_BUDGET_TRANSITION_V1 = "resource_budget_transition_v1";
    RESOURCE_PRECONTACT_BUDGET_TRANSITION_V1 = "resource_precontact_budget_transition_v1";
    COST_UNTRACKED = "cost_untracked";
    THREAD_USAGE = "thread_usage";
    /// Daemon-authored on the subject's chain when the operator acknowledges an
    /// attention item: the item id, kind and the acknowledging principal. The
    /// item stays hidden until it has activity newer than this event.
    ATTENTION_ACKNOWLEDGED = "attention_acknowledged";

    /// Generic domain-event channel: one engine event carrying a namespaced
    /// `kind` plus a free `payload`, emitted by the runtime on behalf of a
    /// tool/directive result. Content declares the kinds and styles them via
    /// view-yaml `projections.event_kinds`; the engine stays domain-agnostic.
    MILESTONE = "milestone";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadOutcomeKind {
    Success,
    Failure,
}

/// Terminal classification for a thread lifecycle event. The one
/// authority: clients must consume this, never re-type the sets.
pub fn thread_terminal_outcome(event_type: &str) -> Option<ThreadOutcomeKind> {
    match event_type {
        THREAD_COMPLETED | THREAD_CONTINUED => Some(ThreadOutcomeKind::Success),
        THREAD_FAILED | THREAD_CANCELLED | THREAD_KILLED | THREAD_TIMED_OUT => {
            Some(ThreadOutcomeKind::Failure)
        }
        _ => None,
    }
}

/// Whether an `outcome_code` names a failed run. Lives beside the event
/// vocabulary so outcome words have a single home.
pub fn outcome_code_is_failure(code: &str) -> bool {
    if let Some(suffix) = code.strip_prefix("exit:") {
        return suffix != "0";
    }
    matches!(
        code,
        "failure"
            | "failed"
            | "error"
            | "cancelled"
            | "canceled"
            | "timeout"
            | "timed_out"
            | "nonzero"
            | "not_ok"
    )
}

#[cfg(test)]
mod tests {
    use super::ALL;
    use std::collections::BTreeSet;

    #[test]
    fn all_has_no_duplicates() {
        let unique: BTreeSet<&str> = ALL.iter().copied().collect();
        assert_eq!(unique.len(), ALL.len(), "duplicate wire string in ALL");
    }
}
