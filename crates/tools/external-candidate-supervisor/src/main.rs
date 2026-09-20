//! Protected external-candidate supervisor executable.
//!
//! A provider adapter may only launch this executable after arranging the
//! fixed descriptor contract below. It accepts no authority through argv,
//! ambient environment, project paths, provider credentials, or node grants.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_candidate_supervisor::runtime::{
    ExternalCandidateSupervisorInputs, ExternalCandidateSupervisorOutcome, SUPERVISOR_BOOTSTRAP_FD,
    SUPERVISOR_CANDIDATE_RUNTIME_FD, SUPERVISOR_LAUNCHER_FD, SUPERVISOR_PRIVATE_PARENT_FD,
    SUPERVISOR_RUNTIME_MOUNT_FD_BASE, SUPERVISOR_STATE_ROOT_FD, run_external_candidate_supervisor,
};
use ryeos_state::external_execution::transport::{
    ExternalSupervisorBootstrap, MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
};

fn main() {
    match run() {
        Ok(outcome) => {
            let code = match &outcome {
                ExternalCandidateSupervisorOutcome::ExportApplied => 0,
                ExternalCandidateSupervisorOutcome::ExecutionDeadline
                | ExternalCandidateSupervisorOutcome::ChannelExpired => 124,
                ExternalCandidateSupervisorOutcome::RevokedApplied
                | ExternalCandidateSupervisorOutcome::RevokedClaimedUnknown => 75,
                ExternalCandidateSupervisorOutcome::RecoveryOnly { .. } => 76,
            };
            if let Ok(value) = serde_json::to_value(&outcome)
                && let Ok(line) = lillux::canonical_json(&value)
            {
                let mut stdout = std::io::stdout();
                let _ = writeln!(stdout, "{line}");
            }
            std::process::exit(code);
        }
        Err(error) => {
            eprintln!("ryeos-external-candidate-supervisor: {error:#}");
            std::process::exit(126);
        }
    }
}

fn run() -> Result<ExternalCandidateSupervisorOutcome> {
    #[cfg(not(unix))]
    anyhow::bail!("external candidate supervisor requires Unix inherited descriptors");
    #[cfg(unix)]
    {
        let bootstrap_bytes = lillux::read_sealed_inherited_descriptor(
            SUPERVISOR_BOOTSTRAP_FD,
            MAX_EXTERNAL_SUPERVISOR_BOOTSTRAP_BYTES,
        )
        .map_err(anyhow::Error::msg)?;
        let bootstrap: ExternalSupervisorBootstrap = serde_json::from_slice(&bootstrap_bytes)
            .context("decode sealed external supervisor bootstrap")?;
        ensure!(
            bootstrap.canonical_bytes()? == bootstrap_bytes,
            "external supervisor bootstrap is not canonical"
        );
        let runtime_mount_count = 1_usize;
        // SAFETY: the admitted lifecycle adapter uniquely maps each fixed
        // descriptor and this executable adopts every coordinate exactly once.
        let state_root = unsafe {
            lillux::PinnedDirectory::take_inherited_directory(
                PathBuf::from("<external-supervisor-state>"),
                SUPERVISOR_STATE_ROOT_FD,
            )
        }?;
        let candidate_runtime = unsafe {
            lillux::PinnedDirectory::take_inherited_directory(
                PathBuf::from("<external-candidate-runtime>"),
                SUPERVISOR_CANDIDATE_RUNTIME_FD,
            )
        }?;
        let candidate_private_parent = unsafe {
            lillux::PinnedDirectory::take_inherited_directory(
                PathBuf::from("<external-candidate-private-parent>"),
                SUPERVISOR_PRIVATE_PARENT_FD,
            )
        }?;
        let launcher =
            unsafe { lillux::take_inherited_descriptor_authority(SUPERVISOR_LAUNCHER_FD) }
                .map_err(anyhow::Error::msg)?;
        let mut runtime_mounts = Vec::with_capacity(runtime_mount_count);
        for index in 0..runtime_mount_count {
            let descriptor = SUPERVISOR_RUNTIME_MOUNT_FD_BASE
                .checked_add(u32::try_from(index)?)
                .context("external supervisor runtime mount descriptor overflow")?;
            runtime_mounts.push(unsafe {
                lillux::PinnedDirectory::take_inherited_directory(
                    PathBuf::from("<external-candidate-runtime-mount>"),
                    descriptor,
                )
            }?);
        }
        run_external_candidate_supervisor(ExternalCandidateSupervisorInputs {
            bootstrap,
            state_root,
            candidate_runtime,
            candidate_private_parent,
            launcher,
            runtime_mounts,
        })
    }
}
