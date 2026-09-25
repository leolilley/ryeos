//! Standalone native-mechanism observation, not runtime qualification.
//!
//! A dedicated single-threaded process may change its own namespaces through
//! Lillux. It runs a synthetic live descendant writer, settles the namespace,
//! then captures that same pinned directory into a private scratch CAS. The
//! caller supplies five exact runtime hashes; no ambient executable, loader,
//! library search or worker/admission authority is reconstructed here.
//!
//! This is reusable verifier infrastructure, not a verifier of an arbitrary
//! subject: success does not attest any product qualification claim, candidate
//! completion fence, cloud lifetime, or retained worker result. Scratch state
//! is retained for the enclosing disposable fixture owner, including failures.

use anyhow::{Context as _, Result, ensure};
use lillux::{
    PinnedDirectory,
    time::{Duration, MonotonicDeadline},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    io::{Seek as _, Write as _},
    path::Path,
    sync::Arc,
};

const MEMBERS: [&str; 5] = [
    "bin/probe",
    "lib64/ld-linux-x86-64.so.2",
    "usr/lib/libc.so.6",
    "usr/lib/libgcc_s.so.1",
    "usr/lib/libm.so.6",
];
// The real standalone probe links state/capture and needs libm in addition to
// libc/libgcc. Keep its explicit closure within the small-CAS per-file ceiling.
const MAX_MEMBER: u64 = 32 * 1024 * 1024;
const MAX_TOTAL: u64 = 40 * 1024 * 1024;
const COUNTER_BYTES: u64 = 16;
const CAPTURE_BYTES: u64 = COUNTER_BYTES + 1;
const SCENARIO: &str = "held-live-descendant-settlement-capture";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema: String,
    scenario: String,
    runtime_sha256: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    schema: &'static str,
    scenario: &'static str,
    runtime_sha256: BTreeMap<String, String>,
    held_counter: u64,
    double_release_refused: bool,
    writer_observations: [u64; 2],
    namespace_exit: String,
    capture_bytes: u64,
    capture_limit_bytes: u64,
    refused_limit_bytes: u64,
    captured_files: BTreeMap<String, String>,
    captured_blob_sha256: BTreeMap<String, String>,
    over_budget_returned_tree: bool,
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // Do not leak private input bytes or emit a success envelope on error.
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

fn run() -> Result<()> {
    use lillux::invocation::{InvocationBounds, StartupInvocation};
    let deadline = MonotonicDeadline::after(Duration::from_secs(60));
    // SAFETY: exclusive single-threaded executable startup. All three modes
    // are launched with uniquely inherited pipes, with only opposite ends
    // retained by their parent; no Rust stdio owners or host tasks exist.
    let mut invocation = unsafe {
        StartupInvocation::take_inherited_pipes(
            0,
            1,
            InvocationBounds {
                max_arguments: 3,
                max_argument_bytes: 512,
                max_total_argument_bytes: 1536,
                max_input_bytes: 2048,
                max_output_bytes: 4096,
            },
            deadline,
        )
    }?;
    let args = invocation.arguments();
    ensure!(args.len() >= 2, "probe mode is required");
    match args[1].to_str().context("non-UTF8 mode")? {
        "guest-parent" if args.len() == 2 => guest_parent(),
        "guest-writer" if args.len() == 2 => guest_writer(),
        "observe" if args.len() == 3 => {
            let relative = args[2]
                .to_str()
                .context("non-UTF8 runtime path")?
                .to_owned();
            validate_relative(&relative)?;
            let request = parse_request(&invocation.read_input()?)?;
            let result = observe(&relative, request)?;
            let mut bytes = serde_json::to_vec(&result)?;
            bytes.push(b'\n');
            invocation.write_output(&bytes)?;
            Ok(())
        }
        _ => anyhow::bail!("unknown probe mode or argument shape"),
    }
}

fn validate_relative(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 512
            && !value.contains('\\')
            && !value.chars().any(char::is_control)
            && value
                .split('/')
                .all(|s| !s.is_empty() && s != "." && s != ".."),
        "runtime path must be normalized and relative"
    );
    Ok(())
}

fn parse_request(bytes: &[u8]) -> Result<Request> {
    ensure!(bytes.len() <= 2048, "request exceeds bound");
    ensure!(
        serde_json::from_slice::<serde_json::Value>(bytes)?.is_object(),
        "request must be a JSON object"
    );
    let request: Request = serde_json::from_slice(bytes)?;
    ensure!(
        request.schema == "test.native_mechanism_request.v1" && request.scenario == SCENARIO,
        "unsupported mechanism request"
    );
    ensure!(
        request.runtime_sha256.len() == MEMBERS.len()
            && MEMBERS.iter().all(
                |member| request.runtime_sha256.get(*member).is_some_and(
                    |h| lillux::valid_hash(h) && !h.bytes().any(|c| c.is_ascii_uppercase())
                )
            ),
        "expected exact five-member canonical runtime identity"
    );
    Ok(request)
}

fn open_directory(root: &PinnedDirectory, relative: &str) -> Result<PinnedDirectory> {
    validate_relative(relative)?;
    let mut directory = root.try_clone()?;
    for part in relative.split('/') {
        directory = directory
            .open_child_directory(OsStr::new(part))?
            .context("runtime directory is absent")?;
    }
    Ok(directory)
}

/// Copy only verified bytes into a new exact runtime directory. Keeping the
/// input pathname mounted directly would leave a check/use substitution gap.
fn stage_runtime(
    input: &PinnedDirectory,
    output: &PinnedDirectory,
    hashes: &BTreeMap<String, String>,
) -> Result<()> {
    let mut seen = Vec::new();
    input.visit_regular_files_bounded(
        lillux::DirectoryTraversalBudget::new(9, 3),
        |_, _| Ok(false),
        |relative, _| {
            seen.push(
                relative
                    .to_str()
                    .context("non-UTF8 runtime entry")?
                    .to_owned(),
            );
            Ok(())
        },
    )?;
    seen.sort();
    ensure!(seen == MEMBERS, "runtime has missing or undeclared members");
    let mut total = 0u64;
    for member in MEMBERS {
        let file = input
            .open_pinned_regular_descendant(Path::new(member), false)?
            .context("missing runtime member")?;
        let bytes = file.read_stable_bounded(&file.observation()?, MAX_MEMBER)?;
        total = total
            .checked_add(bytes.len() as u64)
            .context("runtime byte overflow")?;
        ensure!(total <= MAX_TOTAL, "runtime total exceeds bound");
        ensure!(
            bytes.len() >= 64
                && &bytes[..4] == b"\x7fELF"
                && bytes[4] == 2
                && bytes[5] == 1
                && bytes[18..20] == [0x3e, 0],
            "expected Linux x86_64 ELF"
        );
        ensure!(
            hashes.get(member) == Some(&lillux::sha256_hex(&bytes)),
            "runtime hash mismatch"
        );
        let path = Path::new(member);
        let mut parent = output.try_clone()?;
        for component in path.parent().context("runtime parent absent")?.components() {
            parent = parent.open_or_create_child(component.as_os_str(), 0o755)?;
        }
        parent
            .atomic_create_pinned_regular(
                path.file_name().context("runtime filename absent")?,
                &bytes,
                0o755,
            )?
            .context("runtime member already exists")?;
    }
    Ok(())
}

fn observe(relative: &str, request: Request) -> Result<Observation> {
    let cwd = PinnedDirectory::open(Path::new("."))?.context("private input cwd absent")?;
    let input = open_directory(&cwd, relative)?;
    let (_, scratch) = cwd.create_unique_child("native-mechanism", 0o700)?;
    let runtime = scratch.create_child(OsStr::new("runtime"), 0o700)?;
    stage_runtime(&input, &runtime, &request.runtime_sha256)?;
    let candidate = scratch.create_child(OsStr::new("candidate"), 0o700)?;
    candidate
        .atomic_create_pinned_regular(OsStr::new("counter"), b"0000000000000000", 0o644)?
        .context("counter already exists")?;
    // Ordinary private CAS initialization, not a fabricated candidate-launch
    // binding. It contains no signed heads, project admission or worker state.
    let state_root = scratch.create_child(OsStr::new("cas-state"), 0o700)?;
    let state =
        ryeos_state::StateDb::open(state_root.path(), Arc::new(ryeos_state::TrustStore::new()))?;
    let authority = state.pinned_authority()?;
    let guard = authority.acquire_shared_guard()?;
    drop(state);
    let policy = ryeos_state::objects::ProjectSnapshotPolicy::new(
        ryeos_state::project_sync::ProjectSyncScope::FullProject,
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
    )?;
    let runtime_fd = runtime.inherited_descriptor_authority()?;
    let candidate_fd = candidate.inherited_descriptor_authority()?;
    let request_native = lillux::LinuxSandboxRequest {
        executable: "/runtime/lib64/ld-linux-x86-64.so.2".into(),
        argv0: "ld-linux-x86-64.so.2".into(),
        arguments: loader_arguments("guest-parent")
            .into_iter()
            .map(Into::into)
            .collect(),
        cwd: "/work".into(),
        environment: BTreeMap::new(),
        mounts: vec![
            lillux::LinuxSandboxMount {
                source_fd: runtime_fd
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?,
                destination: "/runtime".into(),
                access: lillux::LinuxSandboxMountAccess::ReadOnly,
                layer: 0,
            },
            lillux::LinuxSandboxMount {
                source_fd: candidate_fd
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?,
                destination: "/work".into(),
                access: lillux::LinuxSandboxMountAccess::Writable,
                layer: 0,
            },
        ],
        fixed_parent_views: vec![],
        overlay: None,
        network: lillux::LinuxSandboxNetwork::Isolated,
        private_tmp: true,
        proc_filesystem: lillux::LinuxSandboxProcFilesystem::PidNamespace,
        minimal_devices: true,
        character_devices: vec![],
        target_channels: vec![],
        lifecycle: lillux::LinuxSandboxLifecycle::Run,
        contain_process_group: true,
        nested_sandbox: false,
        aggregate_limits: None,
    };
    let (mut held, pipes) =
        lillux::prepare_linux_sandbox_piped(request_native).map_err(anyhow::Error::msg)?;
    // No fallible early return between preparation and explicit settlement.
    // Preserve all pipe and mount owners until settlement. Guests write no
    // protocol output; retained pipes are not interpreted as death evidence.
    let observations = (|| -> Result<[u64; 2]> {
        ensure!(read_counter(&candidate)? == 0, "held target wrote counter");
        ensure!(
            candidate
                .open_pinned_regular(OsStr::new("parent.ready"), false)?
                .is_none(),
            "held target wrote handshake"
        );
        held.release_once().map_err(anyhow::Error::msg)?;
        ensure!(
            held.release_once().is_err(),
            "second release unexpectedly succeeded"
        );
        let deadline = MonotonicDeadline::after(Duration::from_secs(10));
        let mut first = None;
        loop {
            ensure!(!deadline.has_elapsed(), "writer progress deadline elapsed");
            ensure!(
                held.try_observe_target_exit()
                    .map_err(anyhow::Error::msg)?
                    .is_none(),
                "guest parent exited before settlement"
            );
            if let Some(marker) =
                candidate.open_pinned_regular(OsStr::new("parent.ready"), false)?
            {
                ensure!(marker.read_bounded(1)? == b"1", "invalid writer handshake");
                let value = read_counter(&candidate)?;
                if value > 0 {
                    match first {
                        Some(prior) if value > prior => return Ok([prior, value]),
                        None => first = Some(value),
                        _ => (),
                    }
                }
            }
            lillux::time::sleep(Duration::from_millis(10));
        }
    })();
    let settlement = held
        .terminate_namespace_for_export_until(MonotonicDeadline::after(Duration::from_secs(10)))
        .map_err(anyhow::Error::msg);
    // A refused/timed-out settlement never permits capture or a success result.
    let termination = settlement.context("native namespace settlement uncertain")?;
    ensure!(
        termination.launch_failure().is_none(),
        "native guest launch failed"
    );
    let writer_observations = observations?;
    drop(pipes);
    let counter = stable_bytes(&candidate, "counter", COUNTER_BYTES)?;
    ensure!(
        counter.len() as u64 == COUNTER_BYTES,
        "counter length changed"
    );
    parse_counter(&counter)?;
    let marker = stable_bytes(&candidate, "parent.ready", 1)?;
    ensure!(marker == b"1", "settled handshake changed");
    let mut blobs = BTreeMap::new();
    let mut expected = BTreeMap::new();
    for (name, bytes) in [("counter", counter), ("parent.ready", marker)] {
        let blob_hash = lillux::sha256_hex(&bytes);
        let descriptor = ryeos_state::objects::ProjectFile {
            blob_hash: blob_hash.clone(),
            size: bytes.len() as u64,
            normalized_mode: 0o644,
        };
        expected.insert(
            name.to_owned(),
            lillux::sha256_hex(lillux::canonical_json(&descriptor.to_value())?.as_bytes()),
        );
        blobs.insert(name.to_owned(), blob_hash);
    }
    let budget = ryeos_project_capture::ProjectCaptureBudget {
        max_bytes: CAPTURE_BYTES,
        deadline: MonotonicDeadline::after(Duration::from_secs(5)),
    };
    let tree = ryeos_project_capture::ingest_project_tree_bounded(
        &authority, &guard, &candidate, &policy, budget,
    )?;
    ensure!(
        tree.files == expected,
        "capture differs from settled pinned bytes"
    );
    let refused = ryeos_project_capture::ingest_project_tree_bounded(
        &authority,
        &guard,
        &candidate,
        &policy,
        ryeos_project_capture::ProjectCaptureBudget {
            max_bytes: CAPTURE_BYTES - 1,
            deadline: MonotonicDeadline::after(Duration::from_secs(5)),
        },
    );
    let error = refused
        .err()
        .context("over-budget capture returned a tree")?;
    // The API presently exposes anyhow errors, not a typed budget category.
    // Recognize only its bounded-content refusal; unrelated I/O/deadline
    // failures cannot be credited as the intended byte-budget observation.
    ensure!(
        error
            .chain()
            .any(|e| e.to_string().starts_with("streaming CAS source exceeds ")),
        "capture failed for a reason other than byte budget"
    );
    Ok(Observation {
        schema: "test.native_mechanism_observation.v1",
        scenario: SCENARIO,
        runtime_sha256: request.runtime_sha256,
        held_counter: 0,
        double_release_refused: true,
        writer_observations,
        namespace_exit: format!("{:?}", termination.exit()),
        capture_bytes: CAPTURE_BYTES,
        capture_limit_bytes: CAPTURE_BYTES,
        refused_limit_bytes: CAPTURE_BYTES - 1,
        captured_files: tree.files,
        captured_blob_sha256: blobs,
        over_budget_returned_tree: false,
    })
}

fn loader_arguments(mode: &str) -> Vec<String> {
    [
        "--inhibit-cache",
        "--library-path",
        "/runtime/usr/lib",
        "/runtime/bin/probe",
        mode,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn stable_bytes(root: &PinnedDirectory, name: &str, limit: u64) -> Result<Vec<u8>> {
    let file = root
        .open_pinned_regular(OsStr::new(name), false)?
        .context("candidate file absent")?;
    file.read_stable_bounded(&file.observation()?, limit)
}

fn parse_counter(bytes: &[u8]) -> Result<u64> {
    ensure!(
        bytes.len() == COUNTER_BYTES as usize && bytes.iter().all(u8::is_ascii_digit),
        "malformed writer counter"
    );
    Ok(std::str::from_utf8(bytes)?.parse()?)
}

fn read_counter(root: &PinnedDirectory) -> Result<u64> {
    let file = root
        .open_pinned_regular(OsStr::new("counter"), false)?
        .context("writer counter absent")?;
    // Intentionally an active-writer observation, not a stable/frozen read.
    parse_counter(&file.read_bounded(COUNTER_BYTES)?)
}

fn guest_parent() -> Result<()> {
    let work = PinnedDirectory::open(Path::new("/work"))?.context("guest work absent")?;
    let mut child = lillux::SubordinateProcess::spawn(lillux::SubordinateProcessRequest {
        cmd: "/runtime/lib64/ld-linux-x86-64.so.2".into(),
        argv0: None,
        args: loader_arguments("guest-writer"),
        cwd: "/work".into(),
        envs: vec![],
        limits: None,
        inherited_fds: vec![],
    })
    .map_err(anyhow::Error::msg)?;
    drop(child.take_input().map_err(anyhow::Error::msg)?);
    work.atomic_create_pinned_regular(OsStr::new("parent.ready"), b"1", 0o644)?
        .context("guest handshake already present")?;
    let deadline = MonotonicDeadline::after(Duration::from_secs(45));
    while !deadline.has_elapsed() {
        ensure!(
            child.try_exit().map_err(anyhow::Error::msg)?.is_none(),
            "writer exited early"
        );
        lillux::time::sleep(Duration::from_millis(10));
    }
    // Exact child owner drop is only emergency guest cleanup; the controller
    // still requires namespace termination before any filesystem capture.
    anyhow::bail!("guest parent reached its independent ceiling")
}

fn guest_writer() -> Result<()> {
    let work = PinnedDirectory::open(Path::new("/work"))?.context("guest work absent")?;
    let mut counter = work
        .open_regular(OsStr::new("counter"), true)?
        .context("guest counter absent")?;
    let deadline = MonotonicDeadline::after(Duration::from_secs(45));
    let mut value = 1u64;
    while !deadline.has_elapsed() {
        counter.seek(std::io::SeekFrom::Start(0))?;
        counter.write_all(format!("{value:016}").as_bytes())?;
        counter.flush()?;
        value += 1;
        lillux::time::sleep(Duration::from_millis(5));
    }
    anyhow::bail!("guest writer reached its independent ceiling")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> serde_json::Value {
        json!({"schema":"test.native_mechanism_request.v1", "scenario":SCENARIO,
            "runtime_sha256": MEMBERS.into_iter().map(|p| (p, "a".repeat(64))).collect::<BTreeMap<_,_>>()})
    }

    #[test]
    fn request_is_exact_and_bounded() {
        assert!(parse_request(&serde_json::to_vec(&request()).unwrap()).is_ok());
        for changed in [json!({}), json!([]), json!({"schema":"other"})] {
            assert!(parse_request(&serde_json::to_vec(&changed).unwrap()).is_err());
        }
        let mut value = request();
        value["extra"] = json!(true);
        assert!(parse_request(&serde_json::to_vec(&value).unwrap()).is_err());
        let value = request();
        let sequence = json!([value["schema"], value["scenario"], value["runtime_sha256"]]);
        assert!(parse_request(&serde_json::to_vec(&sequence).unwrap()).is_err());
        let mut value = request();
        value["runtime_sha256"]
            .as_object_mut()
            .unwrap()
            .remove(MEMBERS[0]);
        assert!(parse_request(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut value = request();
        value["runtime_sha256"][MEMBERS[0]] = json!("A".repeat(64));
        assert!(parse_request(&serde_json::to_vec(&value).unwrap()).is_err());
        assert!(parse_request(&vec![b' '; 2049]).is_err());
    }

    #[test]
    fn relative_paths_and_writer_messages_are_closed() {
        assert!(validate_relative("inputs/runtime").is_ok());
        for path in [
            "",
            "/runtime",
            "./runtime",
            "a/../b",
            "a//b",
            "a/",
            "a\\b",
            "a\nb",
        ] {
            assert!(validate_relative(path).is_err(), "{path:?}");
        }
        assert_eq!(parse_counter(b"0000000000000002").unwrap(), 2);
        for bytes in [
            b"".as_slice(),
            b"2",
            b"000000000000000x",
            b"00000000000000000",
        ] {
            assert!(parse_counter(bytes).is_err());
        }
    }

    #[test]
    fn runtime_missing_substituted_or_symlinked_members_refuse() {
        // Harness-only malformed filesystem setup; production access above
        // remains entirely descriptor-pinned Lillux operations.
        let fixture = tempfile::tempdir().unwrap();
        let root = PinnedDirectory::open(fixture.path()).unwrap().unwrap();
        let input = root.create_child(OsStr::new("input"), 0o700).unwrap();
        let output = root.create_child(OsStr::new("output"), 0o700).unwrap();
        let hashes = parse_request(&serde_json::to_vec(&request()).unwrap())
            .unwrap()
            .runtime_sha256;
        assert!(stage_runtime(&input, &output, &hashes).is_err());
        for member in MEMBERS {
            let path = fixture.path().join("input").join(member);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut elf = vec![0u8; 64];
            elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
            elf[18] = 0x3e;
            std::fs::write(path, elf).unwrap();
        }
        assert!(
            stage_runtime(&input, &output, &hashes)
                .unwrap_err()
                .to_string()
                .contains("hash mismatch")
        );
        #[cfg(unix)]
        {
            std::fs::remove_file(fixture.path().join("input/bin/probe")).unwrap();
            std::os::unix::fs::symlink(
                "../usr/lib/libc.so.6",
                fixture.path().join("input/bin/probe"),
            )
            .unwrap();
            assert!(stage_runtime(&input, &output, &hashes).is_err());
        }
    }
}
