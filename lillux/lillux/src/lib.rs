pub mod cas;
pub mod exec;
pub mod identity;
pub mod signature;
pub mod time;

pub use exec::{RunningProcess, SpawnResult, SubprocessRequest, SubprocessResult};

pub use cas::{atomic_write, canonical_json, sha256_hex, shard_path, valid_hash, CasStore};

pub use identity::envelope::{
    inspect_envelope, open_envelope, seal_envelope, validate_envelope_env,
    AadFields, Envelope, InspectResult, OpenResult, ValidateResult,
};

pub fn run(request: SubprocessRequest) -> SubprocessResult {
    exec::lib_run(request)
}

pub fn spawn(request: SubprocessRequest) -> Result<RunningProcess, SubprocessResult> {
    exec::lib_spawn(request)
}

pub fn spawn_detached(
    cmd: &str,
    args: &[String],
    log: Option<&str>,
    envs: &[(String, String)],
) -> Result<SpawnResult, String> {
    exec::lib_spawn_detached(cmd, args, log, envs)
}

pub fn kill(pid: u32, grace: f64) -> Result<String, String> {
    exec::lib_kill(pid, grace)
}

pub fn is_alive(pid: u32) -> bool {
    exec::lib_is_alive(pid)
}
