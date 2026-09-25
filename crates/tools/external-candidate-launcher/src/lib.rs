//! Fixed-descriptor entrypoint for the dedicated external candidate launcher.

use anyhow::{Context as _, Result};
use ryeos_external_execution::backends::linux::NativeCandidateOutput;
use ryeos_external_execution::launcher::prepare_from_inherited_bootstrap;
use ryeos_external_execution::launcher_protocol::serve_native_candidate_launcher;

/// Adopt the exact inherited launch authority and serve one native candidate.
pub fn run_from_inherited() -> Result<()> {
    // SAFETY: this executable is launched only through the typed parent
    // builder which installs each fixed descriptor exactly once. Unsupported
    // host mechanics are refused by Lillux, not selected here.
    let (spec, authority, candidate, output) = unsafe { prepare_from_inherited_bootstrap() }?;
    // SAFETY: the typed parent maps this exact connected channel and clears
    // CLOEXEC only for this launch.
    let channel = unsafe {
        lillux::take_inherited_duplex_channel_from_env("RYEOS_EXTERNAL_CANDIDATE_CONTROL_FD")
    }
    .map_err(anyhow::Error::msg)?;
    let NativeCandidateOutput { stdout, stderr } = output;
    let remaining_ms = spec
        .binding
        .expires_at_ms
        .checked_sub(lillux::time::timestamp_millis())
        .context("external candidate channel already expired")?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(
        u64::try_from(remaining_ms)?,
    ));
    let bootstrap_digest = spec.digest()?;
    serve_native_candidate_launcher(
        channel,
        candidate,
        stdout,
        spec.max_stdout_bytes,
        stderr,
        spec.max_stderr_bytes,
        authority,
        bootstrap_digest,
        spec.binding,
        deadline,
    )
}
