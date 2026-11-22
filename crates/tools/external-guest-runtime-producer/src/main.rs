//! Produce a retained, credential-free guest-owner runtime from one exact
//! RyeOS realization. The signed Tool pins the owner binary and supplies only
//! an authored profile. The product names no controller: the placing
//! controller's key travels in its signed occurrence assignment. This command
//! never reads an installed Bundle pathname or qualifies a provider snapshot.

use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use ryeos_external_execution::guest_import_authorization::GuestOwnerRuntimeProfile;
use ryeos_external_execution::guest_runtime_product::produce_guest_owner_runtime;

const INPUT_ROOT: &str = "/ryeos/realizations";
const INPUT_NAME: &str = "guest-owner-input";
const PRODUCTS_NAME: &str = "products";
const OUTPUT_NAME: &str = "external-guest-owner-runtime";
const MAX_OWNER_BYTES: u64 = 256 * 1024 * 1024;

struct Arguments {
    project_path: OsString,
    profile: GuestOwnerRuntimeProfile,
}

/// The producer takes no parameters; its input is exactly the empty object.
const PRODUCER_INPUT: &[u8] = b"{}";

fn parse_arguments(args: impl IntoIterator<Item = OsString>, input: &[u8]) -> Result<Arguments> {
    let mut args = args.into_iter();
    ensure!(
        args.next().as_deref() == Some(OsStr::new("--project-path")),
        "producer requires --project-path"
    );
    let project_path = args.next().context("producer project path is absent")?;
    ensure!(
        input == PRODUCER_INPUT,
        "producer input must be the empty object"
    );
    ensure!(
        args.next().as_deref() == Some(OsStr::new("--profile-json")),
        "producer requires --profile-json"
    );
    let profile_json = args.next().context("producer profile is absent")?;
    let profile_json = profile_json
        .to_str()
        .context("producer profile is not UTF-8")?;
    ensure!(
        profile_json.len() <= 4 * 1024,
        "producer profile exceeds bound"
    );
    let profile: GuestOwnerRuntimeProfile = serde_json::from_str(profile_json)?;
    ensure!(
        lillux::canonical_json(&serde_json::to_value(&profile)?)? == profile_json,
        "producer profile is noncanonical"
    );
    ensure!(args.next().is_none(), "producer received extra arguments");
    Ok(Arguments {
        project_path,
        profile,
    })
}

fn produce(
    source_root: &lillux::PinnedDirectory,
    project_root: &lillux::PinnedDirectory,
    profile: &GuestOwnerRuntimeProfile,
) -> Result<(String, String)> {
    let source = source_root
        .open_pinned_regular(OsStr::new(INPUT_NAME), false)?
        .context("exact pinned guest-owner input is absent")?;
    let authority = source.inherited_descriptor_authority()?;
    authority.require_owned_executable()?;
    let observation = authority.regular_file_observation()?;
    ensure!(
        (1..=MAX_OWNER_BYTES).contains(&observation.size()),
        "guest-owner input exceeds file bound"
    );
    let digest = authority.digest_regular_file_stable_exact(&observation)?;
    let products = project_root.open_or_create_child(OsStr::new(PRODUCTS_NAME), 0o700)?;
    products.require_owner_private_directory()?;
    let product = produce_guest_owner_runtime(
        &products,
        OsStr::new(OUTPUT_NAME),
        &authority,
        observation.size(),
        &digest,
        profile,
    )?;
    Ok((product.manifest_hash().to_owned(), digest))
}

fn main() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().take(513).read_to_end(&mut input)?;
    let arguments = parse_arguments(std::env::args_os().skip(1), &input)?;
    let source_root = lillux::PinnedDirectory::open(Path::new(INPUT_ROOT))?
        .context("admitted realization root is absent")?;
    let project_root = lillux::PinnedDirectory::open(Path::new(&arguments.project_path))?
        .context("admitted private project workspace is absent")?;
    let (manifest_hash, owner_sha256) = produce(&source_root, &project_root, &arguments.profile)?;
    println!(
        "{}",
        serde_json::json!({
            "schema": 1,
            "product": OUTPUT_NAME,
            "manifest_hash": manifest_hash,
            "owner_sha256": owner_sha256,
        })
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn profile() -> GuestOwnerRuntimeProfile {
        GuestOwnerRuntimeProfile {
            schema: 2,
            account: lillux::GuestRuntimeAccount::Unix {
                uid: 65534,
                gid: 65534,
            },
            private_source_max_bytes: 32 * 1024 * 1024,
            private_source_max_inodes: 1024,
            owner_timeout_seconds: 600,
        }
    }

    #[test]
    fn exact_admitted_input_produces_one_private_runtime() {
        let source = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::set_permissions(project.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let input = source.path().join(INPUT_NAME);
        std::fs::write(&input, b"exact-owner-fixture").unwrap();
        std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o755)).unwrap();
        let source = lillux::PinnedDirectory::open(source.path())
            .unwrap()
            .unwrap();
        let project = lillux::PinnedDirectory::open(project.path())
            .unwrap()
            .unwrap();
        let (manifest, digest) = produce(&source, &project, &profile()).unwrap();
        assert_eq!(digest, lillux::sha256_hex(b"exact-owner-fixture"));
        assert!(lillux::valid_hash(&manifest));
        assert!(produce(&source, &project, &profile()).is_err());
        let output = project.path().join(
            "products/external-guest-owner-runtime/bin/ryeos-external-guest-occurrence-owner",
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"exact-owner-fixture");
        for directory in [
            project.path().join(PRODUCTS_NAME),
            project.path().join("products/external-guest-owner-runtime"),
        ] {
            assert_eq!(
                std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn malformed_profile_and_nonexecutable_source_refuse() {
        let input = "{}";
        let arguments = vec![
            "--project-path".into(),
            "/private".into(),
            "--profile-json".into(),
            "{\"schema\":1}".into(),
        ];
        assert!(parse_arguments(arguments, input.as_bytes()).is_err());
        let canonical_profile =
            lillux::canonical_json(&serde_json::to_value(profile()).unwrap()).unwrap();
        let valid = vec![
            "--project-path".into(),
            "/private".into(),
            "--profile-json".into(),
            canonical_profile.into(),
        ];
        for refused in [
            "{ }",
            "{\"extra\":1}",
            "{\"controller_public_key\":\"ed25519:AAAA\"}",
            "",
        ] {
            assert!(parse_arguments(valid.clone(), refused.as_bytes()).is_err());
        }
        assert_eq!(
            parse_arguments(valid, input.as_bytes()).unwrap().profile,
            profile()
        );
        let source = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::set_permissions(project.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let input = source.path().join(INPUT_NAME);
        std::fs::write(&input, b"not-executable").unwrap();
        std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o644)).unwrap();
        let source = lillux::PinnedDirectory::open(source.path())
            .unwrap()
            .unwrap();
        let project = lillux::PinnedDirectory::open(project.path())
            .unwrap()
            .unwrap();
        assert!(produce(&source, &project, &profile()).is_err());
        assert!(!project.path().join(PRODUCTS_NAME).exists());
    }
}
