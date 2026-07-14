pub mod arg_binder;
pub mod authorizer;
pub mod callback;
pub mod callback_client;
pub mod callback_contract;
pub mod callback_uds;
pub mod checkpoint;
pub mod command;
pub mod condition;
pub mod daemon_rpc;
pub mod envelope;
pub mod events;
pub mod framing;
pub mod hooks_eval;
pub mod hooks_loader;
pub mod interpolation;
pub mod method_wire;
pub mod model_resolution;
pub mod paths;
pub mod progress;
pub mod provider_snapshot;
pub mod resolver;
pub mod scalar_or_vec;
pub mod template;
pub mod verified_loader;

pub use arg_binder::bind_argv;
pub use authorizer::{
    canonical_cap, cap_matches, AuthorizationError, AuthorizationPolicy, Authorizer, Capability,
    CapabilityClause, CapabilityParseError,
};
pub use callback::{
    client_from_env, ActionPayload, CallbackError, DispatchActionRequest, ReplayResponse,
    ReplayedEventRecord, RuntimeCallbackAPI, TerminalCompletion,
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
pub use condition::{apply_operator, matches, resolve_path};
pub use daemon_rpc::{resolve_daemon_socket_path, DaemonRpcClient, RpcError};
pub use events::{RuntimeEventType, StorageClass};
pub use framing::{recv_frame, send_frame};
pub use hooks_eval::{merge_hooks, run_hooks, HookDispatcher};
pub use hooks_loader::{HookDefinition, HooksLoader};
pub use interpolation::{interpolate, interpolate_action, referenced_input_keys};
pub use paths::AI_DIR;
pub use progress::{ProgressEvent, StatusEvent};
pub use provider_snapshot::ResolvedProviderSnapshot;
pub use resolver::{resolve_command, ResolveError, ResolvedCommand};
