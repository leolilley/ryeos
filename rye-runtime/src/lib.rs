pub mod callback;
pub mod callback_uds;
pub mod capability_tokens;
pub mod condition;
pub mod daemon_rpc;
pub mod framing;
pub mod hooks_eval;
pub mod hooks_loader;
pub mod interpolation;
pub mod paths;
pub mod transcript;

pub use callback::{
    client_from_env, ActionPayload, CallbackError, DispatchActionRequest, RuntimeCallbackAPI,
};
pub use capability_tokens::{cap_matches, check_capability, expand_capabilities};
pub use condition::{apply_operator, matches, resolve_path};
pub use daemon_rpc::{
    resolve_daemon_socket_path, DaemonRpcClient, RpcError, ThreadLifecycleClient,
};
pub use ed25519_dalek::SigningKey;
pub use framing::{recv_frame, send_frame};
pub use hooks_eval::{merge_hooks, run_hooks, HookDispatcher};
pub use hooks_loader::{HookDefinition, HooksLoader};
pub use interpolation::{interpolate, interpolate_action};
pub use paths::{
    safe_rel_path, thread_knowledge_path, thread_state_dir, thread_transcript_path, AI_DIR,
};
pub use transcript::{KnowledgeRenderOptions, Transcript};
