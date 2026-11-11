//! Install the staged inputs and start the occurrence owner.
//!
//! Every input is untrusted bytes in sealed slots. The only values read out of
//! the signed documents are three digests and a size that can narrow what the
//! launcher accepts; the signatures, bindings and freshness are the owner's to
//! verify, so nothing here grants authority.

use std::ffi::{OsStr, OsString};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ryeos_external_execution::guest_import_authorization::{
    GuestOwnerRuntimeProfile, ObservedGuestRuntime,
};
use ryeos_external_execution_contract::guest_import_authorization::{
    MAX_GUEST_IMPORT_AUTHORIZATION_BYTES, MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES,
};
use serde_json::Value;

/// Slot names the adapter stages; the facade treats them as opaque.
const SLOT_RUNTIME: &str = "runtime";
const SLOT_SIGNED_IMPORT: &str = "signed-import";
const SLOT_PACKAGE: &str = "package";

const STAGE_DIRECTORY: &str = "stage";
const RUNTIME_DIRECTORY: &str = "guest-runtime";
const ACTIVATION_DIRECTORY: &str = "activation";
const IMPORT_FILE: &str = "signed-import.json";
const PACKAGE_FILE: &str = "guest-package";
const OWNER_NAME: &str = "ryeos-external-guest-occurrence-owner";

/// The runtime archive is at most the snapshot producer's bound plus tar
/// framing; the package is at most the facade's slot bound.
const MAX_RUNTIME_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024 + 64 * 1024;
const MAX_OWNER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PROFILE_BYTES: u64 = 4 * 1024;
const CONTROLLER_ROOT_BYTES: u64 = 64;
const MAX_PACKAGE_BYTES: u64 = 4 * 1024 * 1024 * 1024 + 64 * 1024;

const TEMP_PURPOSE: lillux::TempPurpose = lillux::TempPurpose::new("launch");

/// Where the guest namespace lives. Production uses `/ryeos`.
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn owner_program(&self) -> PathBuf {
        self.root
            .join(RUNTIME_DIRECTORY)
            .join("bin")
            .join(OWNER_NAME)
    }
}

/// The account gate. The occurrence owner must start as the sandbox
/// administrator: it hands the activation roots to the profile's account and
/// then drops to it itself (clearing groups, setting no-new-privs, setting
/// every uid and gid), so the launcher never changes ids. Any start that is
/// already the guest account, or whose account is not an exact non-root
/// coordinate, is refused because the owner could then not grant or drop.
pub fn require_administrator_start(profile: &GuestOwnerRuntimeProfile) -> Result<()> {
    profile.account.validate()?;
    ensure!(
        profile.account.require_current_process().is_err(),
        "launcher is already the guest account; the owner must start as administrator"
    );
    Ok(())
}

/// One member the runtime archive must contain, in this order.
struct Member {
    path: &'static [u8],
    directory: bool,
    mode: u32,
    max_bytes: u64,
    exact_bytes: Option<u64>,
}

const RUNTIME_MEMBERS: [Member; 4] = [
    Member {
        path: b"bin/",
        directory: true,
        mode: 0o755,
        max_bytes: 0,
        exact_bytes: None,
    },
    Member {
        path: b"bin/ryeos-external-guest-occurrence-owner",
        directory: false,
        mode: 0o755,
        max_bytes: MAX_OWNER_BYTES,
        exact_bytes: None,
    },
    Member {
        path: b"controller-root.hex",
        directory: false,
        mode: 0o644,
        max_bytes: CONTROLLER_ROOT_BYTES,
        exact_bytes: Some(CONTROLLER_ROOT_BYTES),
    },
    Member {
        path: b"guest-owner-profile.json",
        directory: false,
        mode: 0o644,
        max_bytes: MAX_PROFILE_BYTES,
        exact_bytes: None,
    },
];

/// Digest and size read out of the staged signed import. These only narrow
/// what the launcher will install.
struct Expected {
    payload_sha256: String,
    framed_bytes: u64,
}

fn digest_field<'a>(document: &'a Value, pointer: &str) -> Result<&'a str> {
    let value = document
        .pointer(pointer)
        .and_then(Value::as_str)
        .with_context(|| format!("signed import lacks {pointer}"))?;
    ensure!(
        lillux::valid_hash(value),
        "signed import {pointer} is invalid"
    );
    Ok(value)
}

fn expected_from_import(bytes: &[u8]) -> Result<Expected> {
    let document: Value = serde_json::from_slice(bytes).context("signed import is not JSON")?;
    let framed_bytes = document
        .pointer("/authorization/ticket/framed_bytes")
        .and_then(Value::as_u64)
        .filter(|bytes| (1..=MAX_PACKAGE_BYTES).contains(bytes))
        .context("signed import has no usable package size")?;
    Ok(Expected {
        payload_sha256: digest_field(&document, "/authorization/ticket/payload_sha256")?.to_owned(),
        framed_bytes,
    })
}

fn expected_runtime_manifest(assignment: &[u8]) -> Result<String> {
    let document: Value = serde_json::from_slice(assignment).context("assignment is not JSON")?;
    Ok(digest_field(&document, "/assignment/guest_runtime_manifest_hash")?.to_owned())
}

/// Require a slot to be sealed and return its pinned file. An unsealed slot
/// may still be growing, so its bytes are never read.
fn sealed_slot(stage: &lillux::PinnedDirectory, slot: &str) -> Result<lillux::PinnedRegularFile> {
    let marker = format!("{slot}.done");
    ensure!(
        stage.open_child_directory(OsStr::new(&marker))?.is_some(),
        "stage slot {slot} is not sealed"
    );
    stage
        .open_pinned_regular(OsStr::new(slot), false)?
        .with_context(|| format!("stage slot {slot} is missing"))
}

fn read_slot(file: &lillux::PinnedRegularFile, max_bytes: u64) -> Result<Vec<u8>> {
    let observation = file.observation()?;
    ensure!(
        (1..=max_bytes).contains(&observation.size()),
        "stage slot size is outside its bound"
    );
    file.read_stable_bounded(&observation, max_bytes)
}

/// Install the runtime archive as the exact tree the snapshot producer
/// emits: four members, in order, with fixed modes and bounded sizes.
fn install_runtime(archive: &[u8], root: &lillux::PinnedDirectory) -> Result<()> {
    let mut archive = tar::Archive::new(Cursor::new(archive));
    let mut entries = archive.entries().context("read runtime archive")?;
    let mut bin: Option<lillux::PinnedDirectory> = None;
    for member in &RUNTIME_MEMBERS {
        let name = String::from_utf8_lossy(member.path).into_owned();
        let mut entry = entries
            .next()
            .with_context(|| format!("runtime archive lacks {name}"))?
            .with_context(|| format!("runtime archive member {name} is unreadable"))?;
        let header = entry.header();
        let kind_ok = if member.directory {
            header.entry_type().is_dir()
        } else {
            header.entry_type().is_file()
        };
        let size = header.size()?;
        ensure!(
            entry.path_bytes().as_ref() == member.path
                && kind_ok
                && header.mode()? == member.mode
                && header.uid()? == 0
                && header.gid()? == 0
                && size <= member.max_bytes
                && member.exact_bytes.is_none_or(|exact| exact == size),
            "runtime archive member {name} differs from the exact runtime tree"
        );
        if member.directory {
            let directory = root.create_child(OsStr::new("bin"), member.mode)?;
            directory.set_mode(member.mode)?;
            bin = Some(directory);
            continue;
        }
        let (parent, file_name) = match member.path.strip_prefix(b"bin/") {
            Some(rest) => (
                bin.as_ref()
                    .context("runtime archive member precedes bin")?,
                rest,
            ),
            None => (root, member.path),
        };
        let file_name = std::str::from_utf8(file_name)?;
        let written = parent
            .atomic_create_regular_from_reader(
                OsStr::new(file_name),
                &mut entry,
                member.max_bytes,
                member.mode,
                TEMP_PURPOSE,
            )?
            .with_context(|| format!("runtime member {name} already exists"))?;
        ensure!(
            written.1 == size,
            "runtime archive member {name} changed length"
        );
    }
    ensure!(
        entries.next().is_none(),
        "runtime archive contains an ambient entry"
    );
    Ok(())
}

/// Verify the staged inputs, install them and start the owner. Returns the
/// owner's exit code. `account` is the account-transition gate and `exec`
/// starts the owner; both are injected so tests never switch accounts or
/// start a process.
pub fn launch(
    layout: &Layout,
    assignment: &[u8],
    account: &dyn Fn(&GuestOwnerRuntimeProfile) -> Result<()>,
    exec: &mut dyn FnMut(&Path, &[OsString]) -> Result<i32>,
) -> Result<i32> {
    ensure!(
        (1..=MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES).contains(&assignment.len()),
        "assignment is outside its bound"
    );
    let root =
        lillux::PinnedDirectory::open(&layout.root)?.context("guest namespace root is absent")?;
    let stage = root
        .open_child_directory(OsStr::new(STAGE_DIRECTORY))?
        .context("stage directory is absent")?;
    let runtime = sealed_slot(&stage, SLOT_RUNTIME)?;
    let signed_import = sealed_slot(&stage, SLOT_SIGNED_IMPORT)?;
    let package = sealed_slot(&stage, SLOT_PACKAGE)?;

    let import_bytes = read_slot(
        &signed_import,
        (MAX_GUEST_IMPORT_AUTHORIZATION_BYTES + 256) as u64,
    )?;
    let expected = expected_from_import(&import_bytes)?;
    let archive = read_slot(&runtime, MAX_RUNTIME_ARCHIVE_BYTES)?;
    let manifest = expected_runtime_manifest(assignment)?;

    let runtime_root = root.create_child(OsStr::new(RUNTIME_DIRECTORY), 0o755)?;
    runtime_root.set_mode(0o755)?;
    install_runtime(&archive, &runtime_root)?;
    let observed = ObservedGuestRuntime::observe(&runtime_root)?;
    ensure!(
        observed.manifest_hash() == manifest,
        "installed runtime differs from the assigned runtime manifest"
    );
    account(observed.profile())?;

    let activation = root.create_child(OsStr::new(ACTIVATION_DIRECTORY), 0o700)?;
    activation.set_mode(0o700)?;
    activation
        .atomic_create_regular_from_reader(
            OsStr::new(IMPORT_FILE),
            &mut Cursor::new(&import_bytes),
            import_bytes.len() as u64,
            0o600,
            TEMP_PURPOSE,
        )?
        .context("signed import already placed")?;
    let mut reader = package.stable_reader_exact(
        expected.framed_bytes,
        &expected.payload_sha256,
        MAX_PACKAGE_BYTES,
    )?;
    activation
        .atomic_create_regular_from_reader(
            OsStr::new(PACKAGE_FILE),
            &mut reader,
            expected.framed_bytes,
            0o600,
            TEMP_PURPOSE,
        )?
        .context("guest package already placed")?;
    reader.finish()?;
    stage.remove_pinned_regular_if_same(&package)?;

    let args = [
        OsString::from("--assignment-b64"),
        OsString::from(URL_SAFE_NO_PAD.encode(assignment)),
    ];
    exec(&layout.owner_program(), &args)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Read as _;

    use lillux::crypto::SigningKey;
    use ryeos_external_execution::guest_runtime_product::{
        produce_guest_owner_runtime, seal_guest_owner_snapshot_upload,
    };

    use super::*;

    struct Fixture {
        root: tempfile::TempDir,
        archive: Vec<u8>,
        package: Vec<u8>,
        assignment: Vec<u8>,
        import: Vec<u8>,
    }

    fn runtime_archive() -> (Vec<u8>, String) {
        let source = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("owner"), b"owner-fixture").unwrap();
        let source = lillux::PinnedDirectory::open(source.path())
            .unwrap()
            .unwrap();
        let project = lillux::PinnedDirectory::open(project.path())
            .unwrap()
            .unwrap();
        project.set_mode(0o700).unwrap();
        let file = source
            .open_pinned_regular(OsStr::new("owner"), false)
            .unwrap()
            .unwrap();
        file.set_mode(0o755).unwrap();
        let authority = file.inherited_descriptor_authority().unwrap();
        let profile = GuestOwnerRuntimeProfile {
            schema: 2,
            account: lillux::GuestRuntimeAccount::Unix {
                uid: 65534,
                gid: 65534,
            },
            private_source_max_bytes: 32 * 1024 * 1024,
            private_source_max_inodes: 1024,
            owner_timeout_seconds: 600,
        };
        let controller = SigningKey::from_bytes(&[29; 32]).verifying_key();
        let product = produce_guest_owner_runtime(
            &project,
            OsStr::new("runtime"),
            &authority,
            13,
            &lillux::sha256_hex(b"owner-fixture"),
            &controller,
            &profile,
        )
        .unwrap();
        let upload = seal_guest_owner_snapshot_upload(
            product.root(),
            product.manifest_hash(),
            64 * 1024 * 1024,
        )
        .unwrap();
        let mut archive = Vec::new();
        upload
            .descriptor()
            .stable_regular_reader_exact(upload.bytes(), upload.sha256(), upload.bytes())
            .unwrap()
            .read_to_end(&mut archive)
            .unwrap();
        (archive, product.manifest_hash().to_owned())
    }

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        let parent = lillux::PinnedDirectory::open(path.parent().unwrap())
            .unwrap()
            .unwrap();
        parent
            .open_pinned_regular(path.file_name().unwrap(), false)
            .unwrap()
            .unwrap()
            .set_mode(0o600)
            .unwrap();
    }

    fn fixture() -> Fixture {
        let (archive, manifest) = runtime_archive();
        let package = b"framed-package-bytes".to_vec();
        let import = serde_json::json!({
            "authorization": {"ticket": {
                "supervisor_sha256": lillux::sha256_hex(&archive),
                "payload_sha256": lillux::sha256_hex(&package),
                "framed_bytes": package.len(),
            }},
            "signature_hex": "00",
        })
        .to_string()
        .into_bytes();
        let assignment = serde_json::json!({
            "assignment": {"guest_runtime_manifest_hash": manifest},
            "signature_hex": "00",
        })
        .to_string()
        .into_bytes();
        let root = tempfile::tempdir().unwrap();
        let stage = root.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        for (slot, bytes) in [
            (SLOT_RUNTIME, &archive),
            (SLOT_SIGNED_IMPORT, &import),
            (SLOT_PACKAGE, &package),
        ] {
            write(&stage.join(slot), bytes);
            std::fs::create_dir(stage.join(format!("{slot}.done"))).unwrap();
        }
        Fixture {
            root,
            archive,
            package,
            assignment,
            import,
        }
    }

    fn run(fixture: &Fixture) -> (Result<i32>, Vec<(PathBuf, Vec<OsString>)>) {
        let calls = RefCell::new(Vec::new());
        let result = launch(
            &Layout::new(fixture.root.path()),
            &fixture.assignment,
            &|_| Ok(()),
            &mut |program, args| {
                calls
                    .borrow_mut()
                    .push((program.to_path_buf(), args.to_vec()));
                Ok(7)
            },
        );
        (result, calls.into_inner())
    }

    #[test]
    fn exact_inputs_are_installed_and_owner_gets_the_fixed_argv() {
        let fixture = fixture();
        let (result, calls) = run(&fixture);
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls.len(), 1);
        let (program, args) = &calls[0];
        assert_eq!(
            program,
            &fixture
                .root
                .path()
                .join("guest-runtime/bin/ryeos-external-guest-occurrence-owner")
        );
        assert_eq!(
            args,
            &[
                OsString::from("--assignment-b64"),
                OsString::from(URL_SAFE_NO_PAD.encode(&fixture.assignment)),
            ]
        );
        let activation = fixture.root.path().join("activation");
        assert_eq!(
            std::fs::read(activation.join(IMPORT_FILE)).unwrap(),
            fixture.import
        );
        assert_eq!(
            std::fs::read(activation.join(PACKAGE_FILE)).unwrap(),
            fixture.package
        );
        assert_eq!(
            std::fs::read(
                fixture
                    .root
                    .path()
                    .join("guest-runtime/bin")
                    .join(OWNER_NAME)
            )
            .unwrap(),
            b"owner-fixture"
        );
    }

    #[test]
    fn tampered_runtime_is_refused_before_activation_or_exec() {
        let fixture = fixture();
        let slot = fixture.root.path().join("stage").join(SLOT_RUNTIME);
        let mut bytes = std::fs::read(&slot).unwrap();
        let at = bytes
            .windows(13)
            .position(|window| window == b"owner-fixture")
            .unwrap();
        bytes[at] ^= 1;
        write(&slot, &bytes);
        let (result, calls) = run(&fixture);
        assert!(result.is_err() && calls.is_empty());
        assert!(!fixture.root.path().join("activation").exists());
    }

    #[test]
    fn wrong_assigned_runtime_manifest_is_refused() {
        let mut fixture = fixture();
        fixture.assignment = serde_json::json!({
            "assignment": {"guest_runtime_manifest_hash": "0".repeat(64)},
        })
        .to_string()
        .into_bytes();
        let (result, calls) = run(&fixture);
        assert!(result.is_err() && calls.is_empty());
    }

    #[test]
    fn missing_and_unsealed_slots_are_refused() {
        for slot in [SLOT_RUNTIME, SLOT_SIGNED_IMPORT, SLOT_PACKAGE] {
            let fixture = fixture();
            std::fs::remove_file(fixture.root.path().join("stage").join(slot)).unwrap();
            let (result, calls) = run(&fixture);
            assert!(result.is_err() && calls.is_empty(), "{slot} missing");

            let fixture = self::fixture();
            std::fs::remove_dir(
                fixture
                    .root
                    .path()
                    .join("stage")
                    .join(format!("{slot}.done")),
            )
            .unwrap();
            let (result, calls) = run(&fixture);
            assert!(result.is_err() && calls.is_empty(), "{slot} unsealed");
            assert!(!fixture.root.path().join("guest-runtime").exists());
        }
    }

    #[test]
    fn tampered_package_is_refused_before_exec() {
        let fixture = fixture();
        write(
            &fixture.root.path().join("stage").join(SLOT_PACKAGE),
            b"framed-package-byteZ",
        );
        let (result, calls) = run(&fixture);
        assert!(result.is_err() && calls.is_empty());
    }

    #[test]
    fn oversize_or_empty_assignment_is_refused_before_contact() {
        let mut fixture = fixture();
        for bad in [
            Vec::new(),
            vec![b'a'; MAX_SIGNED_GUEST_OCCURRENCE_ASSIGNMENT_BYTES + 1],
        ] {
            fixture.assignment = bad;
            let (result, calls) = run(&fixture);
            assert!(result.is_err() && calls.is_empty());
            assert!(!fixture.root.path().join("guest-runtime").exists());
        }
    }

    #[test]
    fn account_gate_refusal_stops_before_activation_and_exec() {
        let fixture = fixture();
        let calls = RefCell::new(0);
        let result = launch(
            &Layout::new(fixture.root.path()),
            &fixture.assignment,
            &|_| anyhow::bail!("account transition refused"),
            &mut |_, _| {
                *calls.borrow_mut() += 1;
                Ok(0)
            },
        );
        assert!(result.is_err());
        assert_eq!(*calls.borrow(), 0);
        assert!(!fixture.root.path().join("activation").exists());
    }

    #[test]
    fn administrator_gate_accepts_the_fixture_profile_and_passes_the_exact_argv() {
        let fixture = fixture();
        let calls = RefCell::new(Vec::new());
        let result = launch(
            &Layout::new(fixture.root.path()),
            &fixture.assignment,
            &require_administrator_start,
            &mut |program, args| {
                calls
                    .borrow_mut()
                    .push((program.to_path_buf(), args.to_vec()));
                Ok(0)
            },
        );
        assert_eq!(result.unwrap(), 0);
        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(calls.borrow()[0].1.len(), 2);
    }

    #[test]
    fn extra_or_reordered_archive_members_are_refused() {
        let fixture = fixture();
        let mut builder = tar::Builder::new(Vec::new());
        let mut archive = tar::Archive::new(Cursor::new(&fixture.archive));
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let mut header = entry.header().clone();
            let path = entry.path().unwrap().into_owned();
            let mut body = Vec::new();
            entry.read_to_end(&mut body).unwrap();
            builder
                .append_data(&mut header, path, body.as_slice())
                .unwrap();
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "extra", &b"x"[..])
            .unwrap();
        let bytes = builder.into_inner().unwrap();
        let target = tempfile::tempdir().unwrap();
        let root = lillux::PinnedDirectory::open(target.path())
            .unwrap()
            .unwrap();
        assert!(install_runtime(&bytes, &root).is_err());
    }
}
