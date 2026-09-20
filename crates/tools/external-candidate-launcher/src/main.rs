//! Dedicated native launcher. All authority arrives on fixed inherited
//! descriptors; this process performs no project, credential or host-path
//! discovery.

use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

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
        let (spec, authority, candidate, output) = unsafe { prepare_from_inherited_bootstrap() }?;
        // SAFETY: the typed parent maps this exact connected channel and clears
        // CLOEXEC only for this launch.
        let channel = unsafe {
            lillux::take_inherited_duplex_channel_from_env("RYEOS_EXTERNAL_CANDIDATE_CONTROL_FD")
        }
        .map_err(anyhow::Error::msg)?;
        let output_state = Arc::new(AtomicU8::new(0));
        let _stdout = drain(
            output.stdout,
            "stdout",
            spec.max_stdout_bytes,
            1,
            channel.try_clone()?,
            output_state.clone(),
        )?;
        let _stderr = drain(
            output.stderr,
            "stderr",
            spec.max_stderr_bytes,
            2,
            channel.try_clone()?,
            output_state.clone(),
        )?;
        let remaining_ms = spec
            .binding
            .expires_at_ms
            .checked_sub(lillux::time::timestamp_millis())
            .context("external candidate channel already expired")?;
        let deadline = lillux::time::MonotonicDeadline::after(std::time::Duration::from_millis(
            u64::try_from(remaining_ms)?,
        ));
        let bootstrap_digest = spec.digest()?;
        let result = serve_native_candidate_launcher(
            channel,
            candidate,
            authority,
            bootstrap_digest,
            spec.binding,
            deadline,
        );
        match output_state.load(Ordering::Acquire) {
            0 => result,
            1 => anyhow::bail!("external candidate exceeded stdout bound"),
            2 => anyhow::bail!("external candidate exceeded stderr bound"),
            _ => anyhow::bail!("external candidate exceeded stdout and stderr bounds"),
        }
    }
}

fn drain(
    mut file: std::fs::File,
    label: &'static str,
    maximum_bytes: u64,
    state_bit: u8,
    channel: lillux::InheritedDuplexChannel,
    output_state: Arc<AtomicU8>,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("external-candidate-{label}"))
        .spawn(move || {
            let mut buffer = [0_u8; 64 * 1024];
            let mut observed = 0_u64;
            let mut limit_reported = false;
            loop {
                match file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        observed = observed.saturating_add(count as u64);
                        if observed > maximum_bytes && !limit_reported {
                            limit_reported = true;
                            output_state.fetch_or(state_bit, Ordering::AcqRel);
                            let _ = channel.shutdown();
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        })
        .with_context(|| format!("create external candidate {label} drainer"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Seek as _, Write as _};

    #[test]
    fn bounded_drainer_reports_limit_and_wakes_control_channel() {
        let mut reader = tempfile::tempfile().unwrap();
        reader.write_all(b"four").unwrap();
        reader.rewind().unwrap();
        let (control, _peer) = lillux::inherited_duplex_channel_pair().unwrap();
        let state = Arc::new(AtomicU8::new(0));
        let handle = drain(
            reader,
            "test",
            3,
            1,
            control.try_clone().unwrap(),
            state.clone(),
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(state.load(Ordering::Acquire), 1);
    }
}
