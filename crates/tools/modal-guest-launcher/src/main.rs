//! Guest entrypoint of a Modal lease: the one executable the facade uploads.
//!
//! The adapter has already staged and sealed the runtime archive, the signed
//! import and the guest package under `/ryeos/stage`, and delivers the signed
//! assignment as this process's stdin. The launcher installs those inputs where
//! the occurrence owner expects them and starts the owner with the fixed
//! `--assignment-b64` argument. It adds no trust: the owner re-verifies every
//! digest and signature against its installed controller root.

mod launch;

use std::ffi::OsString;
use std::io::Read as _;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution_contract::guest_import_authorization::MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES;

use launch::{Layout, launch, refuse_unprobed_account_transition};

const GUEST_ROOT: &str = "/ryeos";

/// Read the whole assignment, refusing an empty or oversize one.
fn read_assignment(input: &mut impl std::io::Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input
        .take(MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("read assignment from stdin")?;
    ensure!(!bytes.is_empty(), "assignment on stdin is empty");
    ensure!(
        bytes.len() <= MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES,
        "assignment on stdin exceeds its bound"
    );
    Ok(bytes)
}

/// Run the owner with exactly these arguments, no shell, no inherited
/// environment and no stdin, and report its exit code. The owner is run as a
/// child rather than replacing this process because Lillux has no
/// process-replacing exec helper yet.
fn run_owner(program: &Path, args: &[OsString]) -> Result<i32> {
    let status = std::process::Command::new(program)
        .args(args)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .status()
        .with_context(|| format!("start {}", program.display()))?;
    Ok(status.code().unwrap_or(1))
}

fn run() -> Result<i32> {
    ensure!(
        std::env::args_os().len() == 1,
        "the launcher takes no arguments"
    );
    let assignment = read_assignment(&mut std::io::stdin().lock())?;
    launch(
        &Layout::new(GUEST_ROOT),
        &assignment,
        &refuse_unprobed_account_transition,
        &mut run_owner,
    )
}

fn main() -> ExitCode {
    match run() {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("ryeos-modal-guest-launcher: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversize_or_empty_assignment_is_refused() {
        assert!(read_assignment(&mut std::io::empty()).is_err());
        let exact = vec![b'a'; MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES];
        assert_eq!(read_assignment(&mut exact.as_slice()).unwrap(), exact);
        let over = vec![b'a'; MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES + 1];
        assert!(read_assignment(&mut over.as_slice()).is_err());
    }
}
