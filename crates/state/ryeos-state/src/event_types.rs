//! Canonical wire strings for runtime event types — the single source of truth.
//!
//! `ryeos-state` is the lowest layer that both the projection (here) and the
//! higher-level `RuntimeEventType` enum (`ryeos-runtime`, which depends on this
//! crate) share. Defining the strings here lets the enum map its variants to
//! these constants AND lets the projection match against them, so neither side
//! spells a literal and a wire-string change happens in exactly one place.

pub const THREAD_CREATED: &str = "thread_created";
pub const THREAD_STARTED: &str = "thread_started";
pub const THREAD_COMPLETED: &str = "thread_completed";
pub const THREAD_FAILED: &str = "thread_failed";
pub const THREAD_CANCELLED: &str = "thread_cancelled";
pub const THREAD_KILLED: &str = "thread_killed";
pub const THREAD_TIMED_OUT: &str = "thread_timed_out";
pub const THREAD_CONTINUED: &str = "thread_continued";

pub const EDGE_RECORDED: &str = "edge_recorded";
pub const CHILD_THREAD_SPAWNED: &str = "child_thread_spawned";
pub const CONTINUATION_REQUESTED: &str = "continuation_requested";
pub const CONTINUATION_ACCEPTED: &str = "continuation_accepted";
pub const COMMAND_SUBMITTED: &str = "command_submitted";
pub const COMMAND_CLAIMED: &str = "command_claimed";
pub const COMMAND_COMPLETED: &str = "command_completed";

pub const STREAM_OPENED: &str = "stream_opened";
pub const TOKEN_DELTA: &str = "token_delta";
pub const STREAM_SNAPSHOT: &str = "stream_snapshot";
pub const STREAM_CLOSED: &str = "stream_closed";

pub const ARTIFACT_PUBLISHED: &str = "artifact_published";
pub const AS_LAUNCHED_RESOLUTION: &str = "as_launched_resolution";
pub const THREAD_FACET_SET: &str = "thread_facet_set";
pub const THREAD_RECONCILED: &str = "thread_reconciled";
pub const ORPHAN_PROCESS_KILLED: &str = "orphan_process_killed";

pub const SYSTEM_PROMPT: &str = "system_prompt";
pub const CONTEXT_INJECTED: &str = "context_injected";
pub const COGNITION_IN: &str = "cognition_in";
pub const COGNITION_OUT: &str = "cognition_out";
pub const COGNITION_REASONING: &str = "cognition_reasoning";

pub const TOOL_CALL_START: &str = "tool_call_start";
pub const TOOL_CALL_RESULT: &str = "tool_call_result";

pub const GRAPH_STARTED: &str = "graph_started";
pub const GRAPH_COMPLETED: &str = "graph_completed";
pub const GRAPH_STEP_STARTED: &str = "graph_step_started";
pub const GRAPH_STEP_COMPLETED: &str = "graph_step_completed";
pub const GRAPH_BRANCH_TAKEN: &str = "graph_branch_taken";
pub const GRAPH_FOREACH_ITERATION: &str = "graph_foreach_iteration";
pub const GRAPH_FOLLOW_SUSPENDED: &str = "graph_follow_suspended";
pub const GRAPH_NODE_RETRY: &str = "graph_node_retry";

pub const PROVIDER_RETRY: &str = "provider_retry";
pub const COST_UNTRACKED: &str = "cost_untracked";
pub const THREAD_USAGE: &str = "thread_usage";

/// Generic domain-event channel: one engine event carrying a namespaced `kind`
/// + free `payload`, emitted by the runtime on behalf of a tool/directive
/// result. Content declares the kinds and styles them via view-yaml
/// `projections.event_kinds`; the engine stays domain-agnostic.
pub const MILESTONE: &str = "milestone";
