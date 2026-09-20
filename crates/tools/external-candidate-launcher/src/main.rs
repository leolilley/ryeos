//! Dedicated native launcher. All authority arrives on fixed inherited
//! descriptors; this process performs no project, credential or host-path
//! discovery.

use std::io::Read as _;

use anyhow::{Context as _, Result};
use ryeos_executor::execution::external_candidate_launcher::prepare_from_inherited_bootstrap;
use ryeos_executor::execution::external_candidate_launcher_protocol::serve_native_candidate_launcher;

fn main() {
    if let Err(error) = run() {
        eprintln!("ryeos-external-candidate-launcher: {error:#}");
        std::process::exit(126);
    }
}

fn run() -> Result<()> {
    #[cfg(not(unix))]
    anyhow::bail!("external candidate launcher requires Unix inherited descriptors");
    #[cfg(unix)]
    {
        // SAFETY: this executable is launched only through the typed parent
        // builder which installs each fixed descriptor exactly once.
        let (spec, _authority, candidate, output) = unsafe { prepare_from_inherited_bootstrap() }?;
        let _stdout = drain(output.stdout, "stdout")?;
        let _stderr = drain(output.stderr, "stderr")?;
        // SAFETY: the typed parent maps this exact connected channel and clears
        // CLOEXEC only for this launch.
        let channel = unsafe {
            lillux::take_inherited_duplex_channel_from_env("RYEOS_EXTERNAL_CANDIDATE_CONTROL_FD")
        }
        .map_err(anyhow::Error::msg)?;
        let remaining_ms = spec
            .binding
            .expires_at_ms
            .checked_sub(lillux::time::timestamp_millis())
            .context("external candidate channel already expired")?;
        let deadline = lillux::time::MonotonicDeadline::after(std::time::Duration::from_millis(
            u64::try_from(remaining_ms)?,
        ));
        serve_native_candidate_launcher(channel, candidate, spec.binding, deadline)
    }
}

fn drain(mut file: std::fs::File, label: &'static str) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("external-candidate-{label}"))
        .spawn(move || {
            let mut buffer = [0_u8; 64 * 1024];
            while let Ok(count) = file.read(&mut buffer) {
                if count == 0 {
                    break;
                }
            }
        })
        .with_context(|| format!("create external candidate {label} drainer"))
}
