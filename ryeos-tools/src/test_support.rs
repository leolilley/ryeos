//! Test fixtures that build isolated, manifest-stable copies of the
//! `ryeos-bundles/core/` tree.
//!
//! ## Why this exists
//!
//! In the dev tree `ryeos-bundles/core/.ai/bin/<triple>/rye-inspect` is
//! a symlink to `target/debug/rye-inspect`. The bundle manifest hashes
//! the dereferenced binary, so any `cargo build` cycle that recompiles
//! `rye-inspect` invalidates the manifest. Under
//! `cargo nextest run --workspace --no-fail-fast` parallel test crates
//! trigger different cargo build invariants and the binary gets
//! rebuilt at unpredictable points mid-run; tests that resolve
//! `bin:rye-inspect` (the `service_data_e2e` `tool_fetch_*` /
//! `tool_verify_*` / `tool_identity_*` set) then fail with
//! `BinHashMismatch`. Each test passes individually after
//! `./scripts/gate.sh --no-tests` re-syncs the manifest.
//!
//! `isolated_core_bundle(test_name)` resolves the issue structurally:
//! it copies the core bundle into `target/test-bundles/<test_name>/`
//! (dereferencing every binary symlink to a real file along the way)
//! and re-signs the manifest in-process via
//! [`crate::actions::build_bundle::rebuild_bundle_manifest`]. The copy
//! is decoupled from `target/debug` so subsequent rebuilds do not
//! invalidate it.
//!
//! ## Implementation choice
//!
//! The re-sign step uses the `ryeos-tools` library API
//! ([`rebuild_bundle_manifest`](crate::actions::build_bundle::rebuild_bundle_manifest))
//! rather than shelling out to `cargo run --bin rye-bundle-tool`,
//! because the subprocess overhead would be paid once per consuming
//! test crate (the per-test `OnceLock` initializer in the consumer
//! amortizes it across that crate's tests).
//!
//! ## Production trust is unchanged
//!
//! `rye init` continues to copy bundles into `system_space_dir` as
//! static, signed artifacts with no symlinks. The race that this
//! fixture closes is dev-tree-only and has never reached an installed
//! daemon.
//!
//! ## Signing key
//!
//! Re-signing requires the platform-author signing key — the same key
//! `./scripts/gate.sh` uses for its automatic re-sync. The key path is
//! taken from the `RYE_SIGNING_KEY` environment variable, falling back
//! to `~/.ai/config/keys/signing/private_key.pem`. Tests that run on
//! a host without that key set up will fail loudly at the first call;
//! that is the same precondition as `gate.sh`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::actions::build_bundle::{load_signing_key, rebuild_bundle_manifest};

/// The cargo workspace root (parent of `ryeos-tools/`).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("ryeos-tools has parent dir")
        .to_path_buf()
}

/// Resolve the platform-author signing key path the test fixtures use
/// when re-signing the isolated bundle's manifest.
fn signing_key_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("RYE_SIGNING_KEY") {
        return PathBuf::from(explicit);
    }
    let home =
        std::env::var_os("HOME").expect("$HOME must be set to locate platform-author signing key");
    PathBuf::from(home).join(".ai/config/keys/signing/private_key.pem")
}

/// Recursive copy that dereferences symlinks (so the destination tree
/// has only regular files and directories) and skips
/// `.ai/objects/` — the CAS store gets repopulated from scratch by
/// [`rebuild_bundle_manifest`] after the copy completes, so carrying
/// over stale blobs (typically gigabytes of accumulated `rye-inspect`
/// rebuilds) only wastes disk.
///
/// Required because [`fs::copy`] on a symlink already follows the link,
/// but the directory walker has to know not to recurse into linked
/// directories or copy them as symlinks themselves.
fn copy_dir_dereference(src: &Path, dst: &Path, src_root: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("create dir {}", dst.display()))?;

    for entry in fs::read_dir(src)
        .with_context(|| format!("read_dir {}", src.display()))?
    {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        // Skip `.ai/objects/` — rebuild_bundle_manifest repopulates
        // CAS from the freshly-dereferenced binaries.
        if src_path == src_root.join(".ai").join("objects") {
            continue;
        }

        // file_type() does not follow symlinks; metadata() does. We
        // want to dereference symlinks pointing at files (which is the
        // rye-inspect case) into real files in the copy. Symlinked
        // directories are not expected in the bundle layout.
        let meta = fs::metadata(&src_path)
            .with_context(|| format!("metadata {}", src_path.display()))?;
        if meta.is_dir() {
            copy_dir_dereference(&src_path, &dst_path, src_root)?;
        } else if meta.is_file() {
            fs::copy(&src_path, &dst_path).with_context(|| {
                format!("copy {} -> {}", src_path.display(), dst_path.display())
            })?;
        } else {
            return Err(anyhow!(
                "unsupported file type at {} (neither file nor dir after symlink resolution)",
                src_path.display()
            ));
        }
    }
    Ok(())
}

/// Copy `ryeos-bundles/core` into `target/test-bundles/<test_name>/`,
/// dereferencing any binary symlinks to real files, and re-sign the
/// resulting bundle manifest in-process with the platform-author key.
///
/// Returns the absolute path to the isolated bundle root, suitable for
/// passing as `RYE_SYSTEM_SPACE` to a `DaemonHarness` test.
///
/// The destination is wiped and recreated on every call. Consumers
/// that share an isolated bundle across many tests in the same crate
/// should wrap the call in a [`std::sync::OnceLock`] so the copy +
/// re-sign cost is paid exactly once per crate per test run.
pub fn isolated_core_bundle(test_name: &str) -> PathBuf {
    let workspace = workspace_root();
    let source = workspace.join("ryeos-bundles/core");
    let dest = workspace.join("target/test-bundles").join(test_name);

    if dest.exists() {
        fs::remove_dir_all(&dest).unwrap_or_else(|e| {
            panic!(
                "remove existing isolated bundle dir {}: {e}",
                dest.display()
            )
        });
    }

    copy_dir_dereference(&source, &dest, &source).unwrap_or_else(|e| {
        panic!(
            "copy core bundle {} -> {}: {e:#}",
            source.display(),
            dest.display()
        )
    });

    let key_path = signing_key_path();
    let signing_key = load_signing_key(&key_path).unwrap_or_else(|e| {
        panic!(
            "load platform-author signing key from {}: {e:#}",
            key_path.display()
        )
    });

    rebuild_bundle_manifest(&dest, &signing_key).unwrap_or_else(|e| {
        panic!(
            "rebuild manifest in isolated bundle {}: {e:#}",
            dest.display()
        )
    });

    dest
}
