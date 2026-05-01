//! The unified subprocess invocation boundary.
//!
//! Both the tool-style subprocess path and the runtime-style spawn path
//! build the SAME struct. The struct is then translated into a
//! `lillux::SubprocessRequest` at the lillux boundary.
//!
//! This struct also IS the input to the future `sandbox_wrap()` stage
//! — see docs/future/node-sandboxed-execution.md. That stage is a
//! pass-through in this wave.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

use crate::canonical_ref::CanonicalRef;
use crate::error::EngineError;
use crate::protocol_vocabulary::{CallbackChannel, StdoutShape};
use crate::resolution::ResolutionOutput;

/// The unified subprocess invocation boundary. Both the tool-style
/// subprocess path and the runtime-style spawn path build this SAME
/// struct. Translated into a `lillux::SubprocessRequest` at the
/// lillux boundary via `to_lillux_request`.
#[derive(Debug, Clone)]
pub struct SubprocessSpec {
    /// Absolute path to the binary to spawn.
    pub cmd: PathBuf,
    /// Argv excluding cmd[0]. Empty vec is allowed.
    pub args: Vec<String>,
    /// Working directory. Required, no default.
    pub cwd: PathBuf,
    /// Env vars to set on the child. Daemon scrubs first then sets.
    pub env: Vec<(String, String)>,
    /// Bytes written to child's stdin and then close-stdin.
    pub stdin: Vec<u8>,
    /// Hard timeout; child is killed on exceed.
    pub timeout: Duration,

    /// Stdout shape declared by the protocol descriptor; the lillux
    /// bridge consults this to choose buffered vs streaming decode.
    /// Default: `OpaqueBytes` (backward compatible with specs not yet
    /// routed through the builder).
    pub stdout_shape: StdoutShape,

    /// Callback channel kind; the launcher consults this to know
    /// whether to register a callback token before spawn.
    /// Default: `None` (backward compatible).
    pub callback_channel: CallbackChannel,

    /// Provenance fields — used by tracing, callback wiring, and the
    /// future sandbox-wrap stage. Not passed to lillux directly.
    pub item_ref: CanonicalRef,
    pub thread_id: String,
    pub project_path: PathBuf,
}

/// All inputs needed by the vocabulary builders to produce a
/// `SubprocessSpec`. Carries every value any builder might need;
/// builders read only the fields their shape needs.
pub struct SubprocessBuildRequest {
    pub cmd: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub item_ref: CanonicalRef,
    pub thread_id: String,
    pub project_path: PathBuf,
    pub acting_principal: String,
    pub cas_root: PathBuf,
    pub callback_token: Option<String>,
    pub callback_socket_path: Option<String>,
    pub vault_handle: Option<String>,
    pub state_dir: PathBuf,
    pub params: Value,
    pub resolution_output: Option<ResolutionOutput>,
}

/// Future seam for node-level sandboxing (see
/// docs/future/node-sandboxed-execution.md). Today: identity. The
/// pipeline-stage shape is established here so the sandbox wave can
/// fill it in without re-cutting the dispatch path.
pub fn sandbox_wrap(spec: SubprocessSpec) -> Result<SubprocessSpec, EngineError> {
    Ok(spec)
}
