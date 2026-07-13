pub mod atomic_fs;
pub mod cas;
pub mod crypto;
pub mod exec;
pub mod identity;
pub mod locks;
pub mod signature;
pub mod time;
pub mod vault;

pub use exec::{RunningProcess, SpawnResult, SubprocessRequest, SubprocessResult};

pub use atomic_fs::{
    atomic_exchange_paths, atomic_write, atomic_write_private, atomic_write_with_mode,
    remove_dir_all_durable, remove_file_durable, rename_path_durable, sync_tree_durable,
    AtomicMutationError, AtomicMutationResult,
};
pub use cas::{atomic_write_batch, canonical_json, sha256_hex, shard_path, valid_hash, CasStore};
pub use locks::{with_exclusive_file_lock, ExclusiveFileLock};

pub use identity::envelope::{
    inspect_envelope, open_envelope, seal_envelope, validate_envelope_env, AadFields, Envelope,
    InspectResult, OpenResult, ValidateResult,
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
