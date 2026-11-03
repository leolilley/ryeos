//! Guest-start rehearsal probe: the real owner, supervisor and launcher
//! executables started through the rehearsal enclosure until the supervisor
//! publishes its relay attach request.
//!
//! `RYEOS_GUEST_START_REHEARSAL_APP_ROOT=<app root>` with
//! `RYEOS_GUEST_START_REHEARSAL_BINDING=<binding id>` rehearses that node's
//! installed products (read-only) and prints the outcome;
//! `RYEOS_GUEST_START_REHEARSAL_OWNER` substitutes a candidate owner build
//! for the installed owner executable. Otherwise the
//! synthetic cases run, with executables from
//! `RYEOS_GUEST_START_REHEARSAL_BIN_DIR` (default: the guest-triple release
//! directory of this test's target directory); without them, or on a host without namespaces or delegated
//! subordinate ids, the probe skips. `RYEOS_GUEST_START_REHEARSAL=1` makes a
//! skip a failure. `RYEOS_GUEST_START_REHEARSAL_WORK` overrides the work
//! parent (default `/var/tmp`).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use ryeos_external_execution::guest_import_authorization::GuestOwnerRuntimeProfile;
use ryeos_external_execution::guest_start_rehearsal::{
    GuestStartRehearsalOutcome, GuestStartRehearsalRequest, GuestStartRehearsalStep,
    run_guest_start_rehearsal, run_rehearsal_host_if_requested,
};
use ryeos_state::external_execution::transport::{
    ExternalControllerFileRelayContract, ExternalNetworkInputPolicy,
};

const OWNER: &str = "ryeos-external-guest-occurrence-owner";
const SUPERVISOR: &str = "ryeos-external-candidate-supervisor";
const LAUNCHER: &str = "ryeos-external-candidate-launcher";

fn work_parent() -> PathBuf {
    std::env::var_os("RYEOS_GUEST_START_REHEARSAL_WORK")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/tmp"))
}

fn print(outcome: &GuestStartRehearsalOutcome) {
    println!("{}", serde_json::to_string_pretty(outcome).unwrap());
}

/// Private scratch for inputs this probe assembles; removed on drop.
fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("guest-start-rehearsal-inputs-")
        .tempdir_in(work_parent())
        .expect("create rehearsal input scratch")
}

fn copy_executable(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to).with_context(|| format!("copy {}", from.display()))?;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// The node's installed binding, owner runtime and products, read in place.
struct NodeSelection {
    owner: PathBuf,
    profile: GuestOwnerRuntimeProfile,
    supervisor: PathBuf,
    launcher: PathBuf,
    product_member: (String, PathBuf, u32),
    product_manifest_record: PathBuf,
    relay: ExternalControllerFileRelayContract,
    network_inputs: ExternalNetworkInputPolicy,
    attach_budget_seconds: u64,
}

fn read_json(path: &Path) -> Result<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
}

fn sharded(root: &Path, hash: &str, suffix: &str) -> PathBuf {
    root.join(&hash[0..2]).join(&hash[2..4]).join(format!("{hash}{suffix}"))
}

fn find_bundle_member(app: &Path, name: &str, sha256: &str) -> Result<PathBuf> {
    for bundle in std::fs::read_dir(app.join(".ai/bundles"))? {
        let bin = bundle?.path().join(".ai/bin");
        let Ok(targets) = std::fs::read_dir(&bin) else {
            continue;
        };
        for target in targets {
            let candidate = target?.path().join(name);
            if candidate.is_file()
                && lillux::sha256_hex(&std::fs::read(&candidate)?) == sha256
            {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!("no installed bundle carries {name} with sha256 {sha256}")
}

fn node_selection(app: &Path, binding: &str) -> Result<NodeSelection> {
    let text = std::fs::read_to_string(
        app.join(".ai/node/external_execution")
            .join(format!("{binding}.yaml")),
    )?;
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with("# ryeos:signed:"))
        .collect::<Vec<_>>()
        .join("\n");
    let binding: serde_json::Value = serde_yaml::from_str(&body)?;
    let field = |name: &str| -> Result<String> {
        binding[name]
            .as_str()
            .map(str::to_owned)
            .with_context(|| format!("binding has no {name}"))
    };
    let state = app.join(".ai/state");
    let objects = state.join("objects/objects");
    let blobs = state.join("objects/blobs");
    let runtime = read_json(&sharded(&objects, &field("guest_runtime_manifest_hash")?, ".json"))?;
    let member = |path: &str| -> Result<PathBuf> {
        let entry = runtime["entries"]
            .as_array()
            .context("runtime manifest has no entries")?
            .iter()
            .find(|entry| entry["path"] == path)
            .with_context(|| format!("runtime manifest has no {path}"))?;
        Ok(sharded(
            &blobs,
            entry["blob_hash"].as_str().context("runtime member has no blob")?,
            "",
        ))
    };
    let profile: GuestOwnerRuntimeProfile =
        serde_json::from_slice(&std::fs::read(member("guest-owner-profile.json")?)?)?;
    let product_manifest_record = sharded(
        &objects,
        binding["workload"]["runtime_manifest_hash"]
            .as_str()
            .context("binding workload has no runtime manifest")?,
        ".json",
    );
    let product = read_json(&product_manifest_record)?;
    let file = product["entries"]
        .as_array()
        .context("product manifest has no entries")?
        .iter()
        .find(|entry| entry["kind"] == "file")
        .context("product manifest has no file")?;
    let source = match (file["blob_hash"].as_str(), file["file_sha256"].as_str()) {
        (Some(blob), _) => sharded(&blobs, blob, ""),
        (None, Some(large)) => state.join("large-objects/objects").join(large),
        _ => anyhow::bail!("product file has no content"),
    };
    Ok(NodeSelection {
        // A candidate owner build may stand in for the installed one.
        owner: match std::env::var_os("RYEOS_GUEST_START_REHEARSAL_OWNER") {
            Some(path) => PathBuf::from(path),
            None => member(&format!("bin/{OWNER}"))?,
        },
        profile,
        supervisor: find_bundle_member(app, SUPERVISOR, &field("supervisor_artifact_hash")?)?,
        launcher: find_bundle_member(app, LAUNCHER, &field("launcher_artifact_hash")?)?,
        product_member: (
            file["path"].as_str().context("product file has no path")?.to_owned(),
            source,
            file["mode"].as_u64().unwrap_or(0o755) as u32,
        ),
        product_manifest_record,
        relay: serde_json::from_value(binding["controller_transport"].clone())?,
        network_inputs: serde_json::from_value(binding["provider_network_inputs"].clone())?,
        attach_budget_seconds: binding["observation_timeout_seconds"]
            .as_u64()
            .context("binding has no observation timeout")?,
    })
}

fn rehearse_node(app: &Path, binding: &str) -> Result<GuestStartRehearsalOutcome> {
    let selection = node_selection(app, binding)?;
    let inputs = scratch();
    let (relative, source, mode) = &selection.product_member;
    let target = inputs.path().join("product").join(relative);
    std::fs::create_dir_all(target.parent().context("product member has no parent")?)?;
    std::fs::copy(source, &target)?;
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(*mode))?;
    eprintln!(
        "rehearsing binding {binding}: owner {}, supervisor {}, launcher {}, product {}",
        selection.owner.display(),
        selection.supervisor.display(),
        selection.launcher.display(),
        source.display()
    );
    run_guest_start_rehearsal(&GuestStartRehearsalRequest {
        owner_executable: &selection.owner,
        profile: selection.profile,
        supervisor_executable: &selection.supervisor,
        launcher_executable: &selection.launcher,
        product: &inputs.path().join("product"),
        product_manifest_record: Some(&selection.product_manifest_record),
        relay: selection.relay,
        network_inputs: selection.network_inputs,
        attach_budget: lillux::time::Duration::from_secs(selection.attach_budget_seconds),
        work_parent: &work_parent(),
        host_executable: &std::env::current_exe()?,
    })
}

fn synthetic_relay() -> ExternalControllerFileRelayContract {
    serde_json::from_value(serde_json::json!({
        "schema": 3,
        "route_contract": "ryeos.external-channel.controller-file-relay.v1",
        "owner_workspace_id": "rehearsal-workspace",
        "private_mailbox_parent": "/ryeos/occurrences",
        "maximum_file_bytes": 1048576,
        "maximum_frames": 1024,
        "poll_interval_ms": 1000,
        "poll_deadline_ms": 30000
    }))
    .unwrap()
}

fn synthetic_network() -> ExternalNetworkInputPolicy {
    serde_json::from_value(serde_json::json!({
        "resolver": {"source": "/etc/resolv.conf", "max_bytes": 65536},
        "hosts": {"source": "/etc/hosts", "max_bytes": 65536}
    }))
    .unwrap()
}

fn rehearse_synthetic(bin: &Path, private_source_max_bytes: u64) -> Result<GuestStartRehearsalOutcome> {
    let inputs = scratch();
    let product = inputs.path().join("product/bin");
    std::fs::create_dir_all(&product)?;
    copy_executable(&bin.join(LAUNCHER), &product.join("rehearsal"))?;
    let profile = GuestOwnerRuntimeProfile {
        schema: 2,
        account: lillux::GuestRuntimeAccount::Unix {
            uid: 65534,
            gid: 65534,
        },
        private_source_max_bytes,
        private_source_max_inodes: 4096,
        owner_timeout_seconds: 120,
    };
    run_guest_start_rehearsal(&GuestStartRehearsalRequest {
        owner_executable: &bin.join(OWNER),
        profile,
        supervisor_executable: &bin.join(SUPERVISOR),
        launcher_executable: &bin.join(LAUNCHER),
        product: &inputs.path().join("product"),
        product_manifest_record: None,
        relay: synthetic_relay(),
        network_inputs: synthetic_network(),
        attach_budget: lillux::time::Duration::from_secs(300),
        work_parent: &work_parent(),
        host_executable: &std::env::current_exe()?,
    })
}

fn skip(reason: &str) {
    assert!(
        std::env::var("RYEOS_GUEST_START_REHEARSAL").as_deref() != Ok("1"),
        "guest-start rehearsal required, but {reason}"
    );
    eprintln!("guest-start rehearsal skipped: {reason}");
}

fn main() {
    run_rehearsal_host_if_requested();
    if std::env::args().any(|arg| arg == "--list") {
        println!("guest_start_rehearsal_native: test");
        return;
    }
    if std::env::var("RYEOS_GUEST_START_REHEARSAL").as_deref() == Ok("0") {
        eprintln!("guest-start rehearsal skipped (RYEOS_GUEST_START_REHEARSAL=0)");
        return;
    }
    if let Some(app) = std::env::var_os("RYEOS_GUEST_START_REHEARSAL_APP_ROOT") {
        let binding = std::env::var("RYEOS_GUEST_START_REHEARSAL_BINDING")
            .expect("RYEOS_GUEST_START_REHEARSAL_BINDING names the binding to rehearse");
        let outcome = rehearse_node(Path::new(&app), &binding).expect("rehearse node selection");
        print(&outcome);
        return;
    }
    let bin = std::env::var_os("RYEOS_GUEST_START_REHEARSAL_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Guest executables ship self-contained (static) for the guest
            // triple; a host-profile build needs an interpreter the guest
            // sandbox does not carry.
            std::env::current_exe()
                .unwrap()
                .ancestors()
                .nth(3)
                .unwrap()
                .join("x86_64-unknown-linux-gnu/release")
        });
    if let Some(missing) = [OWNER, SUPERVISOR, LAUNCHER]
        .into_iter()
        .find(|name| !bin.join(name).is_file())
    {
        return skip(&format!("{} is not built", bin.join(missing).display()));
    }
    let accepted = rehearse_synthetic(&bin, 1024 * 1024 * 1024).expect("synthetic rehearsal");
    print(&accepted);
    if let GuestStartRehearsalOutcome::Refused {
        step: GuestStartRehearsalStep::Host,
        message,
        ..
    } = &accepted
    {
        return skip(message);
    }
    assert!(
        matches!(accepted, GuestStartRehearsalOutcome::AttachPublished { .. }),
        "synthetic rehearsal did not publish its attach request"
    );
    // A ceiling below the package: the owner's private source cannot hold it.
    let refused = rehearse_synthetic(&bin, 4 * 1024 * 1024).expect("ceiling rehearsal");
    print(&refused);
    let GuestStartRehearsalOutcome::Refused { step, .. } = refused else {
        panic!("a private source below the package size was accepted");
    };
    assert_eq!(step, GuestStartRehearsalStep::Stage);
}
