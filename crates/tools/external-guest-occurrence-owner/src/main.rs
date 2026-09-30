//! Dedicated, one-shot owner below the existing external Worker workflow.
//!
//! A qualified guest snapshot must install this executable and the exact
//! controller-root/profile tree. The Render adapter has not yet qualified
//! that snapshot or proved this process survives a lost run stream; it must
//! remain fail-closed until those separate gates are complete.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime;
use ryeos_external_execution::guest_installation::prepare_authorized_held_guest_supervisor_once;
use ryeos_external_execution_contract::guest_import_authorization::{
    MAX_GUEST_IMPORT_AUTHORIZATION_BYTES, MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES,
};

const RUNTIME_ROOT: &str = "/ryeos/guest-runtime";
const ACTIVATION_ROOT: &str = "/ryeos/activation";
const OCCURRENCES_ROOT: &str = "/ryeos/occurrences";
const IMPORT_NAME: &str = "signed-import.json";
const PACKAGE_NAME: &str = "guest-package";
const POLL_INTERVAL: lillux::time::Duration = lillux::time::Duration::from_millis(50);
const CLEANUP_TIMEOUT: lillux::time::Duration = lillux::time::Duration::from_secs(30);

fn decode_assignment_argument(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Vec<u8>> {
    let mut args = args.into_iter();
    ensure!(
        args.next().as_deref() == Some(OsStr::new("--assignment-b64")),
        "guest owner requires only --assignment-b64"
    );
    let encoded = args.next().context("guest owner assignment is absent")?;
    ensure!(
        args.next().is_none(),
        "guest owner received extra arguments"
    );
    let encoded = encoded
        .to_str()
        .context("guest owner assignment is not UTF-8")?;
    ensure!(
        !encoded.is_empty()
            && encoded.len() <= (MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES * 4).div_ceil(3),
        "guest owner assignment exceeds command bound"
    );
    let bytes = URL_SAFE_NO_PAD.decode(encoded)?;
    ensure!(
        !bytes.is_empty()
            && bytes.len() <= MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES
            && URL_SAFE_NO_PAD.encode(&bytes) == encoded,
        "guest owner assignment is not canonical base64url"
    );
    Ok(bytes)
}

fn open_private_root(path: &str) -> Result<lillux::PinnedDirectory> {
    let root = lillux::PinnedDirectory::open(Path::new(path))?
        .with_context(|| format!("guest owner has no installed {path} root"))?;
    root.require_owner_private_directory()?;
    Ok(root)
}

fn admissible_inbound_mode(mode: u32) -> bool {
    // Render's file upload API does not specify the resulting mode. The
    // enclosing activation directory is owner-private; group/other read
    // bits do not grant traversal, while any non-owner write bit would add
    // an unnecessary mutation path before the signed-byte import.
    mode & !0o777 == 0 && mode & 0o700 == 0o600 && mode & 0o077 == mode & 0o044
}

fn require_admissible_inbound_file(file: &lillux::PinnedRegularFile) -> Result<()> {
    lillux::require_effective_user_owned_regular(&file.try_clone_descriptor()?)?;
    ensure!(
        admissible_inbound_mode(file.observation()?.full_permission_mode()?),
        "guest owner inbound file has unexpected mode"
    );
    Ok(())
}

fn read_signed_import(activation: &lillux::PinnedDirectory) -> Result<Vec<u8>> {
    let file = activation
        .open_pinned_regular(OsStr::new(IMPORT_NAME), false)?
        .context("guest owner has no separately delivered signed import")?;
    require_admissible_inbound_file(&file)?;
    let observation = file.observation()?;
    file.read_stable_bounded(
        &observation,
        (MAX_GUEST_IMPORT_AUTHORIZATION_BYTES + 256) as u64,
    )
    .map_err(anyhow::Error::msg)
}

/// Render starts the exact owner executable as the Sandbox administrator.
/// Before any guest work, give only the two mutable private roots and their
/// already-uploaded exact files to the signed non-root account, then discard
/// administrator credentials irreversibly. Immutable runtime members remain
/// root-owned and readable but not writable by that account.
fn prepare_non_root_owner() -> Result<()> {
    let runtime_root = lillux::PinnedDirectory::open(Path::new(RUNTIME_ROOT))?
        .context("guest owner runtime root is absent before account transition")?;
    let observed = ObservedGuestRuntime::observe(&runtime_root)?;
    let account = &observed.profile().account;
    let activation = lillux::PinnedDirectory::open(Path::new(ACTIVATION_ROOT))?
        .context("guest owner activation root is absent")?;
    for name in [IMPORT_NAME, PACKAGE_NAME] {
        let file = activation
            .open_pinned_regular(OsStr::new(name), false)?
            .with_context(|| format!("guest owner has no uploaded {name}"))?;
        account.grant_private_file(&file)?;
    }
    account.grant_private_directory(&activation)?;
    let ryeos_root = lillux::PinnedDirectory::open(Path::new("/ryeos"))?
        .context("guest owner namespace root is absent")?;
    let occurrences = ryeos_root.open_or_create_child(OsStr::new("occurrences"), 0o700)?;
    account.grant_private_directory(&occurrences)?;
    account.drop_current_process()?;
    Ok(())
}

fn run(assignment_bytes: &[u8]) -> Result<()> {
    let runtime_root = lillux::PinnedDirectory::open(Path::new(RUNTIME_ROOT))?
        .context("qualified guest runtime root is absent")?;
    let runtime = ObservedGuestRuntime::observe(&runtime_root)?;
    runtime.profile().account.require_current_process()?;
    let activation = open_private_root(ACTIVATION_ROOT)?;
    let occurrences = open_private_root(OCCURRENCES_ROOT)?;
    runtime.require_disjoint_directory_tree(&activation)?;
    runtime.require_disjoint_directory_tree(&occurrences)?;
    activation.require_disjoint_directory_tree(&occurrences)?;
    let import_bytes = read_signed_import(&activation)?;
    let admitted = runtime.verify_import_documents(&import_bytes, assignment_bytes)?;
    admitted.require_fresh_admission()?;
    let upload = activation
        .open_pinned_regular(OsStr::new(PACKAGE_NAME), false)?
        .context("guest owner has no exact uploaded package")?;
    require_admissible_inbound_file(&upload)?;
    ensure!(
        upload.observation()?.size() == admitted.authorization().ticket.framed_bytes,
        "guest owner upload changed its exact signed size"
    );
    let occurrence =
        occurrences.create_child(OsStr::new(&admitted.authorization().occurrence_id), 0o700)?;
    occurrence.require_owner_private_directory()?;
    let limits = runtime.profile().private_source_limits();
    let lifetime = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(runtime.profile().owner_timeout_seconds),
    ));
    let source = lillux::sandbox::enter_linux_private_source_filesystem(limits)
        .map_err(anyhow::Error::msg)?;
    let mut held = prepare_authorized_held_guest_supervisor_once(
        &occurrence,
        &upload,
        &runtime,
        &import_bytes,
        assignment_bytes,
        source,
        lifetime,
    )?;
    held.mount_preparation_receipt()?;
    let cancelling = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&cancelling))?;
    }
    // Provider stream loss is not a cancellation. Retain this occurrence on
    // SIGHUP and wait for the original supervisor/channel outcome.
    let _stream_hangup = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&_stream_hangup))?;
    let mut released = held.release_once()?;
    loop {
        let natural = match released.try_observe_natural_exit() {
            Ok(exit) => exit,
            Err(error) => {
                let cleanup = released.terminate_namespace_for_export_until(
                    lillux::time::MonotonicDeadline::after(CLEANUP_TIMEOUT),
                );
                return Err(match cleanup {
                    Ok(_) => error.context("guest owner settled the refused native target"),
                    Err(cleanup) => error.context(format!(
                        "guest owner could not prove refused target death: {cleanup:#}"
                    )),
                });
            }
        };
        if let Some(exit) = natural {
            ensure!(
                exit.target_reported_success(),
                "guest supervisor exited without successful applied native launch: {:?}",
                exit.termination().exit()
            );
            // The controller must independently join its authenticated
            // supervisor transcript and frozen export. This exit is only
            // native namespace writer exclusion.
            return Ok(());
        }
        if lifetime.has_elapsed() || cancelling.load(Ordering::Acquire) {
            let _death = released.terminate_namespace_for_export_until(
                lillux::time::MonotonicDeadline::after(CLEANUP_TIMEOUT),
            )?;
            anyhow::bail!("guest owner cancelled or exceeded installed lifetime");
        }
        lillux::time::sleep(POLL_INTERVAL);
    }
}

fn main() -> Result<()> {
    let assignment = decode_assignment_argument(std::env::args_os().skip(1))?;
    prepare_non_root_owner()?;
    run(&assignment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn inbound_file_mode_allows_only_owner_writes_inside_private_activation_root() {
        for mode in [0o600, 0o640, 0o644] {
            assert!(admissible_inbound_mode(mode));
        }
        for mode in [
            0o000, 0o400, 0o500, 0o606, 0o620, 0o622, 0o666, 0o755, 0o777, 0o1600, 0o2600, 0o4600,
        ] {
            assert!(!admissible_inbound_mode(mode));
        }
    }

    #[test]
    fn private_activation_import_accepts_readable_upload_but_refuses_other_writers() {
        let activation = tempfile::tempdir().unwrap();
        std::fs::set_permissions(activation.path(), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let upload = activation.path().join(IMPORT_NAME);
        std::fs::write(&upload, b"signed-import-fixture").unwrap();
        let root = lillux::PinnedDirectory::open(activation.path())
            .unwrap()
            .unwrap();
        root.require_owner_private_directory().unwrap();
        std::fs::set_permissions(&upload, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_signed_import(&root).unwrap(), b"signed-import-fixture");
        std::fs::set_permissions(&upload, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read_signed_import(&root).is_err());
        std::fs::set_permissions(activation.path(), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert!(root.require_owner_private_directory().is_err());
    }

    #[test]
    fn assignment_argument_is_canonical_bounded_and_single_use() {
        let exact = vec![b'a'; MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES];
        let encoded = URL_SAFE_NO_PAD.encode(&exact);
        assert_eq!(
            decode_assignment_argument(["--assignment-b64".into(), encoded.clone().into()])
                .unwrap(),
            exact
        );
        assert!(
            decode_assignment_argument(["--assignment-b64".into(), format!("{encoded}=").into()])
                .is_err()
        );
        assert!(
            decode_assignment_argument(["--assignment-b64".into(), encoded.into(), "extra".into()])
                .is_err()
        );
        assert!(
            decode_assignment_argument(["--assignment-b64".into(), "a".repeat(10_000).into()])
                .is_err()
        );
    }
}
