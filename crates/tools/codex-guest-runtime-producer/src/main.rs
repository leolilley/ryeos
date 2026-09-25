//! Produce one bounded guest Codex runtime from the exact admitted file input.
//!
//! The output is an ordinary private-workspace product. This executable does
//! not qualify the product, choose a provider, or publish a candidate.

use std::ffi::OsStr;
use std::path::Path;

use anyhow::{Context as _, Result, ensure};

const CODEX_SHA256: &str = "cb0a15567e9a60a5820d54b0f6ae86d504dc3805c1eab21a47f70e3eb7b73a40";
const MAX_CODEX_BYTES: u64 = 268_435_456;
const SOURCE_NAME: &str = "codex-guest-input";

fn main() -> Result<()> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    ensure!(
        arguments.next().as_deref() == Some(OsStr::new("--project-path")),
        "Codex guest runtime producer requires --project-path"
    );
    let project = arguments
        .next()
        .context("Codex guest runtime producer requires an admitted project path")?;
    ensure!(arguments.next().is_none(), "unexpected producer argument");
    let source = lillux::PinnedDirectory::open(Path::new("/ryeos/realizations"))?
        .context("admitted realization root is absent")?;
    let project = lillux::PinnedDirectory::open(Path::new(&project))?
        .context("admitted private project workspace is absent")?;
    let (bytes, digest) = produce(&source, &project, CODEX_SHA256, MAX_CODEX_BYTES)?;
    println!(
        "{}",
        serde_json::json!({"schema":1,"product":"codex-guest-runtime","bytes":bytes,"sha256":digest})
    );
    Ok(())
}

fn produce(
    source_root: &lillux::PinnedDirectory,
    project_root: &lillux::PinnedDirectory,
    expected_sha256: &str,
    maximum_bytes: u64,
) -> Result<(u64, String)> {
    let mut source = source_root
        .open_regular(OsStr::new(SOURCE_NAME), false)?
        .context("exact Codex guest input is absent")?;
    let observed = lillux::observe_open_regular_file(&source)?;
    ensure!(
        (1..=maximum_bytes).contains(&observed.size()),
        "Codex guest input exceeds its file bound"
    );
    ensure!(
        observed.portable_mode()? == 0o755,
        "Codex guest input is not executable content"
    );
    let (source_digest, _) =
        lillux::digest_open_regular_file_stable_exact(&source, observed.size())?;
    ensure!(
        source_digest == expected_sha256,
        "Codex guest input differs from the signed activation member"
    );

    let products = project_root.open_or_create_child(OsStr::new("products"), 0o755)?;
    let runtime = products.create_child(OsStr::new("codex-guest-runtime"), 0o755)?;
    let bin = runtime.create_child(OsStr::new("bin"), 0o755)?;
    let (output, copied) = bin
        .atomic_create_regular_from_reader(OsStr::new("codex"), &mut source, maximum_bytes, 0o755)?
        .context("Codex guest runtime output already exists")?;
    ensure!(
        copied == observed.size(),
        "Codex guest input length changed during copy"
    );
    let (output_digest, _) = lillux::digest_open_regular_file_stable_exact(&output, copied)?;
    ensure!(
        output_digest == expected_sha256,
        "Codex guest runtime output differs from the signed input"
    );
    let (source_after, _) =
        lillux::digest_open_regular_file_stable_exact(&source, observed.size())?;
    ensure!(
        source_after == expected_sha256,
        "Codex guest input changed during production"
    );
    Ok((copied, output_digest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn produces_exact_private_runtime_once() {
        let source = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join(SOURCE_NAME), b"exact-codex-fixture").unwrap();
        std::fs::set_permissions(
            source.path().join(SOURCE_NAME),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let source = lillux::PinnedDirectory::open(source.path())
            .unwrap()
            .unwrap();
        let project = lillux::PinnedDirectory::open(project.path())
            .unwrap()
            .unwrap();
        let digest = lillux::sha256_hex(b"exact-codex-fixture");
        assert_eq!(
            produce(&source, &project, &digest, 64).unwrap(),
            (19, digest.clone())
        );
        assert!(produce(&source, &project, &digest, 64).is_err());
        assert_eq!(
            std::fs::read(
                project
                    .path()
                    .join("products/codex-guest-runtime/bin/codex")
            )
            .unwrap(),
            b"exact-codex-fixture"
        );
        assert_eq!(
            std::fs::metadata(
                project
                    .path()
                    .join("products/codex-guest-runtime/bin/codex")
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o777,
            0o755
        );
    }

    #[test]
    fn refuses_wrong_or_oversized_input_before_output() {
        let source = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join(SOURCE_NAME), b"wrong").unwrap();
        std::fs::set_permissions(
            source.path().join(SOURCE_NAME),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let source = lillux::PinnedDirectory::open(source.path())
            .unwrap()
            .unwrap();
        let project = lillux::PinnedDirectory::open(project.path())
            .unwrap()
            .unwrap();
        assert!(produce(&source, &project, &"a".repeat(64), 64).is_err());
        assert!(produce(&source, &project, &lillux::sha256_hex(b"wrong"), 4).is_err());
        std::fs::set_permissions(
            source.path().join(SOURCE_NAME),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(produce(&source, &project, &lillux::sha256_hex(b"wrong"), 64).is_err());
        assert!(
            project
                .open_child_directory(OsStr::new("products"))
                .unwrap()
                .is_none()
        );
    }
}
