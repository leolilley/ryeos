//! Bridge between `SubprocessSpec` and `lillux::SubprocessRequest`.
//!
//! The single point where the unified subprocess boundary struct
//! becomes a lillux subprocess call. The node-owned isolation stage runs
//! after this translation and attaches its resource limits there.

use ryeos_engine::subprocess_spec::SubprocessSpec;

/// Convert a `SubprocessSpec` into a `lillux::SubprocessRequest`.
///
/// The lillux contract: `envs` is authoritative; `env_clear()` is
/// applied before setting these vars. Callers MUST populate every
/// env var the subprocess needs.
pub fn to_lillux_request(spec: &SubprocessSpec) -> anyhow::Result<lillux::SubprocessRequest> {
    Ok(lillux::SubprocessRequest {
        cmd: spec
            .cmd
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("subprocess command path is not valid UTF-8"))?
            .to_owned(),
        args: spec.args.clone(),
        cwd: Some(
            spec.cwd
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("subprocess cwd is not valid UTF-8"))?
                .to_owned(),
        ),
        envs: spec.env.clone(),
        stdin_data: Some(
            String::from_utf8(spec.stdin.clone())
                .map_err(|_| anyhow::anyhow!("subprocess stdin protocol is not valid UTF-8"))?,
        ),
        timeout: spec.timeout.as_secs_f64(),
        limits: None,
        inherited_fds: Vec::new(),
        supervised_status: None,
    })
}
