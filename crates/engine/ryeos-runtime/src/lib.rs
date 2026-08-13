pub mod arg_binder;
pub mod authorizer;
pub mod callback;
pub mod callback_client;
pub mod callback_contract;
pub mod callback_uds;
pub mod checkpoint;
pub mod command;
pub mod compiled_template;
pub mod daemon_rpc;
pub mod effect_answer;
pub mod envelope;
pub mod events;
pub use ryeos_expression as expression;
pub mod framing;
pub mod hooks_eval;
pub mod hooks_loader;
pub mod method_wire;
pub mod paths;
pub mod progress;
pub mod resolver;
pub mod scalar_or_vec;
pub mod template;
pub mod verified_loader;

pub use arg_binder::bind_argv;
pub use authorizer::{
    AuthorizationError, AuthorizationPolicy, Authorizer, Capability, CapabilityClause,
    CapabilityParseError, canonical_cap, cap_matches,
};
pub use callback::{
    ActionPayload, CallbackError, DispatchActionRequest, ProjectObservationPublishParams,
    RUNTIME_FAILURE_KIND, ReplayResponse, ReplayedEventRecord, RuntimeCallbackAPI, RuntimeFailure,
    RuntimeFailureDiagnosticLocator, TerminalCompletion, client_from_env, parse_hook_action,
    validate_runtime_thread_id,
};
pub use checkpoint::CheckpointWriter;
pub use command::{
    CommandAliasDef, CommandArgumentArity, CommandArgumentDef, CommandArgumentForm,
    CommandArgumentKind, CommandArgumentSlot, CommandAvailability, CommandControlFlag, CommandDef,
    CommandDispatch, CommandHelpDef, CommandOrigin, CommandParameterBinding,
    CommandParameterBindingMode, CommandProjectDefault, CommandProjectPolicy,
    CommandProjectResolution, CommandProvenance, CommandRegistrationClaim,
    CommandRegistrationClaimPattern, CommandRegistrationPolicy, CommandRegistrationRule,
    CommandRegistry, CommandRegistryError, ControlFlagBinding, FlagKeyNormalization,
    InvocationInputContract, InvocationInputField, InvocationInputType, MatchedCommand,
};
pub use compiled_template::{CompiledActionTemplate, CompiledJsonTemplate, CompiledTemplateError};
pub use daemon_rpc::{DaemonRpcClient, RpcError, resolve_daemon_socket_path};
pub use effect_answer::{NormalizedDispatchEffect, normalize_dispatch_effect};
pub use events::{
    CognitionInAssembler, CognitionInAssembly, CognitionInChunk, HOOK_FAILURE_SCHEMA,
    HOOK_OBSERVATION_SCHEMA, HookEvidenceDescriptor, HookFailedPayload, HookFailureClass,
    HookObservationRecordedPayload, MAX_RUNTIME_EVENT_BATCH_BYTES, MAX_RUNTIME_EVENT_BATCH_ITEMS,
    MAX_RUNTIME_EVENT_PAYLOAD_BYTES, RuntimeEventType, StorageClass, encode_cognition_in_payloads,
};
pub use expression::{
    CompilationLimits, CompiledExpression, CompiledTemplate, ErrorPhase, EvaluationContext,
    EvaluationLimits, EvaluationSession, ExpressionError, ExpressionValueType, Reference,
    ReferenceSegment, ReferenceSet, RuntimeJsonArrayBudget, RuntimeJsonObjectBudget, SourceSpan,
    TemplatePart, compile_and_render, compile_condition_for, compile_expression,
    compile_expression_for, compile_template, compile_template_for, evaluate, evaluate_bool,
    reject_removed_single_brace_interpolation, render_template,
};
pub use framing::{recv_frame, send_frame};
pub use hooks_eval::{HookDispatcher, HookRunResult, run_hooks};
pub use hooks_loader::{
    CompiledHook, CompiledHookCondition, ExpressionCondition, HookCompilationError,
    HookContextSchema, HookDefinition, HookLayer, HookResultMode, HookSources,
    compile_effective_hook_plan, compile_hooks,
};
pub use lillux::crypto::SigningKey;
pub use paths::AI_DIR;
pub use progress::{ProgressEvent, StatusEvent};
pub use resolver::{ResolveError, ResolvedCommand, resolve_command};
pub use ryeos_engine::contracts::ThreadTerminalStatus;
pub use ryeos_state::{
    MAX_PROJECT_OBSERVATION_JSON_DEPTH, MAX_PROJECT_OBSERVATION_JSON_VALUES,
    MAX_PROJECT_OBSERVATION_NAMESPACE_BYTES, MAX_PROJECT_OBSERVATION_PAYLOAD_BYTES,
    MAX_PROJECT_OBSERVATION_STABLE_ID_BYTES, MAX_PROJECT_OBSERVATIONS_PER_ACTION,
    PROJECT_OBSERVATION_SCHEMA, ProjectObservationOccurrence, ProjectObservationRecordedPayload,
    ProjectObservationRequest, project_observation_id,
};

/// Default daemon-enforced width for a fanout whose author omitted a narrower
/// concurrency bound. All runtime and executor producers share this value so
/// omission cannot create an unwindowed cohort or divergent defaults.
pub const DEFAULT_LIVE_FANOUT_WINDOW_WIDTH: u32 = 8;
