//! Produce a retained, credential-free guest-owner runtime from one exact
//! RyeOS realization. The signed Tool pins the owner binary and supplies only
//! the node's public root and an authored profile; this command never reads
//! an installed Bundle pathname or qualifies a Render snapshot.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use lillux::crypto::VerifyingKey;
use ryeos_external_execution::guest_import_authorization::GuestOwnerRuntimeProfile;
use ryeos_external_execution::guest_runtime_product::produce_guest_owner_runtime;

const INPUT_ROOT: &str = "/ryeos/realizations";
const INPUT_NAME: &str = "guest-owner-input";
const PRODUCTS_NAME: &str = "products";
const OUTPUT_NAME: &str = "external-guest-owner-runtime";
const MAX_OWNER_BYTES: u64 = 256 * 1024 * 1024;

struct Arguments {
    project_path: OsString,
    controller_root: VerifyingKey,
    profile: GuestOwnerRuntimeProfile,
}

fn parse_arguments(args: impl IntoIterator<Item = OsString>) -> Result<Arguments> {
    let mut args = args.into_iter();
    ensure!(
        args.next().as_deref() == Some(OsStr::new("--project-path")),
        "producer requires --project-path"
    );
    let project_path = args.next().context("producer project path is absent")?;
    ensure!(
        args.next().as_deref() == Some(OsStr::new("--controller-root-hex")),
        "producer requires --controller-root-hex"
    );
    let root_hex = args.next().context("producer controller root is absent")?;
    let root_hex = root_hex
        .to_str()
        .context("producer controller root is not UTF-8")?;
    ensure!(
        root_hex.len() == 64
            && root_hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "producer controller root is not canonical lowercase hex"
    );
    let root_bytes: [u8; 32] = hex::decode(root_hex)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("producer controller root changed length"))?;
    let controller_root = VerifyingKey::from_bytes(&root_bytes)?;
    ensure!(
        !controller_root.is_weak(),
        "producer controller root is weak"
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
        controller_root,
        profile,
    })
}

fn produce(
    source_root: &lillux::PinnedDirectory,
    project_root: &lillux::PinnedDirectory,
    controller_root: &VerifyingKey,
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
    project_root.require_owner_private_directory()?;
    let products = project_root.open_or_create_child(OsStr::new(PRODUCTS_NAME), 0o700)?;
    products.require_owner_private_directory()?;
    let product = produce_guest_owner_runtime(
        &products,
        OsStr::new(OUTPUT_NAME),
        &authority,
        observation.size(),
        &digest,
        controller_root,
        profile,
    )?;
    Ok((product.manifest_hash().to_owned(), digest))
}

fn main() -> Result<()> {
    let arguments = parse_arguments(std::env::args_os().skip(1))?;
    let source_root = lillux::PinnedDirectory::open(Path::new(INPUT_ROOT))?
        .context("admitted realization root is absent")?;
    let project_root = lillux::PinnedDirectory::open(Path::new(&arguments.project_path))?
        .context("admitted private project workspace is absent")?;
    let (manifest_hash, owner_sha256) = produce(
        &source_root,
        &project_root,
        &arguments.controller_root,
        &arguments.profile,
    )?;
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
    use lillux::crypto::SigningKey;
    use std::os::unix::fs::PermissionsExt as _;

    fn profile() -> GuestOwnerRuntimeProfile {
        GuestOwnerRuntimeProfile {
            schema: 1,
            private_source_max_bytes: 32 * 1024 * 1024,
            private_source_max_inodes: 1024,
            owner_timeout_seconds: 600,
        }
    }

    #[test]
    fn exact_pinned_input_produces_one_private_runtime() {
        let source = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::set_permissions(project.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let input = source.path().join(INPUT_NAME);
        std::fs::write(&input, b"exact-owner-fixture").unwrap();
        std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o755)).unwrap();
        let source = lillux::PinnedDirectory::open(source.path())
            .unwrap()
            .unwrap();
        let project = lillux::PinnedDirectory::open(project.path())
            .unwrap()
            .unwrap();
        let root = SigningKey::from_bytes(&[29; 32]).verifying_key();
        let (manifest, digest) = produce(&source, &project, &root, &profile()).unwrap();
        assert_eq!(digest, lillux::sha256_hex(b"exact-owner-fixture"));
        assert!(lillux::valid_hash(&manifest));
        assert!(produce(&source, &project, &root, &profile()).is_err());
        let output = project.path().join(
            "products/external-guest-owner-runtime/bin/ryeos-external-guest-occurrence-owner",
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"exact-owner-fixture");
    }

    #[test]
    fn malformed_profile_and_nonexecutable_source_refuse() {
        let root = SigningKey::from_bytes(&[29; 32]).verifying_key();
        let arguments = vec![
            "--project-path".into(),
            "/private".into(),
            "--controller-root-hex".into(),
            hex::encode(root.to_bytes()).into(),
            "--profile-json".into(),
            "{\"schema\":1}".into(),
        ];
        assert!(parse_arguments(arguments).is_err());
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
        assert!(produce(&source, &project, &root, &profile()).is_err());
        assert!(project.path().join(PRODUCTS_NAME).exists() == false);
    }
}
