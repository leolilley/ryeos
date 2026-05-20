//! Bridge between `SubprocessSpec` and `lillux::SubprocessRequest`.
//!
//! The single point where the unified subprocess boundary struct
//! becomes a lillux subprocess call. Sandbox-wrap (future) lives
//! between `SubprocessSpec` construction and this translation.

use ryeos_engine::subprocess_spec::SubprocessSpec;

/// Convert a `SubprocessSpec` into a `lillux::SubprocessRequest`.
///
/// The lillux contract: `envs` is authoritative; `env_clear()` is
/// applied before setting these vars. Callers MUST populate every
/// env var the subprocess needs.
pub fn to_lillux_request(spec: &SubprocessSpec) -> lillux::SubprocessRequest {
    lillux::SubprocessRequest {
        cmd: spec.cmd.to_string_lossy().to_string(),
        args: spec.args.clone(),
        cwd: Some(spec.cwd.to_string_lossy().to_string()),
        envs: spec.env.clone(),
        stdin_data: Some(String::from_utf8_lossy(&spec.stdin).to_string()),
        timeout: spec.timeout.as_secs_f64(),
    }
}
