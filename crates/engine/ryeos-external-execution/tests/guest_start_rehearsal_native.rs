//! Guest-start rehearsal probe: the real owner, supervisor and launcher
//! executables started through the rehearsal enclosure until the supervisor
//! publishes its relay attach request.
//!
//! `RYEOS_GUEST_START_REHEARSAL_APP_ROOT=<app root>` with
//! `RYEOS_GUEST_START_REHEARSAL_BINDING=<binding id>` rehearses that node's
//! installed products (read-only) and prints the outcome;
//! `RYEOS_GUEST_START_REHEARSAL_OWNER` substitutes a candidate owner build
//! for the installed owner executable (`..._SUPERVISOR` and `..._LAUNCHER`
//! likewise for those), and
//! `RYEOS_GUEST_START_REHEARSAL_BASE_SNAPSHOT=<hash>` delivers that project
//! snapshot from the node's CAS as the base;
//! `RYEOS_GUEST_START_REHEARSAL_RETAINED=<dump>` rehearses a placement's
//! retained candidate and inputs instead, and with
//! `RYEOS_GUEST_START_REHEARSAL_ANSWER_ATTACH=1` answers its attach request;
//! `RYEOS_GUEST_START_REHEARSAL_FILE_PRODUCT=<manifest hash>` also mounts
//! that single-file large-content product;
//! `RYEOS_GUEST_START_REHEARSAL_EXPORT=<new directory>` writes the prepared
//! host inputs (with the answer-attach plan, when asked for), this executable
//! and a `run.sh` there instead of running them: `run.sh` replays the
//! activation on the host it runs on, so build this executable statically for
//! a host with another libc. `RYEOS_GUEST_START_REHEARSAL_REPLAY_EXPORT=<export>`
//! replays an export's `run.sh` in a local root enclosure instead. Otherwise the
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
    GuestStartRehearsalBase, GuestStartRehearsalFileProduct, GuestStartRehearsalOutcome,
    GuestStartRehearsalRequest, GuestStartRehearsalStep, export_guest_start_rehearsal,
    run_guest_start_rehearsal, run_rehearsal_host_if_requested,
};
use ryeos_state::LeasedLargeObject;
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

/// Run the prepared rehearsal, or export it for a host when asked.
fn conclude(
    request: &GuestStartRehearsalRequest<'_>,
    export: Option<&Path>,
) -> Result<serde_json::Value> {
    Ok(match export {
        Some(directory) => serde_json::to_value(export_guest_start_rehearsal(request, directory)?)?,
        None => serde_json::to_value(run_guest_start_rehearsal(request)?)?,
    })
}

/// How long exported documents must stay admissible: a host is reached by a
/// slower route than a local run.
fn document_lifetime(export: Option<&Path>) -> Option<lillux::time::Duration> {
    export.map(|_| lillux::time::Duration::from_secs(3 * 3600))
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

/// The node's installed binding, owner runtime and products. Objects and
/// blobs are read through the node's CAS, large content through its
/// large-object store; every file the rehearsal needs a path for is a copy
/// the harness made in its own scratch (or a lease it holds).
struct NodeSelection {
    owner: PathBuf,
    profile: GuestOwnerRuntimeProfile,
    supervisor: PathBuf,
    launcher: PathBuf,
    product_member: (String, PathBuf, u32),
    /// The lease on the large object the product member names, with the store
    /// and runtime directory its descriptor-relative path is resolved
    /// through; all must stay open while the path is used.
    _product_lease: Option<(
        LeasedLargeObject,
        ryeos_state::LargeObjectStore,
        lillux::PinnedDirectory,
    )>,
    product_manifest_record: PathBuf,
    relay: ExternalControllerFileRelayContract,
    network_inputs: ExternalNetworkInputPolicy,
    attach_budget_seconds: u64,
}

fn find_bundle_member(app: &Path, name: &str, sha256: &str) -> Result<PathBuf> {
    for bundle in std::fs::read_dir(app.join(".ai/bundles"))? {
        let bin = bundle?.path().join(".ai/bin");
        let Ok(targets) = std::fs::read_dir(&bin) else {
            continue;
        };
        for target in targets {
            let candidate = target?.path().join(name);
            if candidate.is_file() && lillux::sha256_hex(&std::fs::read(&candidate)?) == sha256 {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!("no installed bundle carries {name} with sha256 {sha256}")
}

/// Copy a CAS blob through the store's API to `to`.
fn copy_blob(cas: &lillux::CasStore, hash: &str, to: &Path) -> Result<()> {
    let (mut blob, _size) = cas
        .open_blob(hash)?
        .with_context(|| format!("blob {hash} is not in the node's CAS"))?;
    let mut out = std::fs::File::create(to).with_context(|| format!("create {}", to.display()))?;
    std::io::copy(&mut blob, &mut out)?;
    Ok(())
}

/// One regular-file entry of an installed content manifest, either tier.
struct ManifestFile {
    path: String,
    mode: Option<u32>,
    blob_hash: Option<String>,
    file_sha256: Option<String>,
    size: Option<u64>,
}

fn manifest_files(value: &serde_json::Value) -> Result<Vec<ManifestFile>> {
    use ryeos_state::objects::{
        EXTERNAL_LARGE_CONTENT_MANIFEST_KIND, ExternalContentManifestEntryKind,
        ExternalContentManifestObject, ExternalLargeContentManifestObject,
    };
    if value["kind"] == EXTERNAL_LARGE_CONTENT_MANIFEST_KIND {
        Ok(ExternalLargeContentManifestObject::from_value(value)?
            .entries
            .into_iter()
            .filter(|entry| entry.kind == ExternalContentManifestEntryKind::File)
            .map(|entry| ManifestFile {
                path: entry.path,
                mode: entry.mode,
                blob_hash: entry.blob_hash,
                file_sha256: entry.file_sha256,
                size: entry.size,
            })
            .collect())
    } else {
        Ok(ExternalContentManifestObject::from_value(value)?
            .entries
            .into_iter()
            .filter(|entry| entry.kind == ExternalContentManifestEntryKind::File)
            .map(|entry| ManifestFile {
                path: entry.path,
                mode: entry.mode,
                blob_hash: entry.blob_hash,
                file_sha256: None,
                size: entry.size,
            })
            .collect())
    }
}

fn node_selection(app: &Path, binding: &str, inputs: &Path) -> Result<NodeSelection> {
    // The binding is a signed node-config document; its typed loader lives in
    // ryeos-app, which this crate cannot depend on, so the fields are read raw.
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
    // The node daemon's runtime state directory and its CAS root.
    let state = app.join(".ai/state");
    let cas = lillux::CasStore::new(state.join("objects"));
    let runtime_hash = field("guest_runtime_manifest_hash")?;
    let runtime =
        manifest_files(&cas.get_object(&runtime_hash)?.with_context(|| {
            format!("runtime manifest {runtime_hash} is not in the node's CAS")
        })?)?;
    let member = |path: &str, to: &str| -> Result<PathBuf> {
        let entry = runtime
            .iter()
            .find(|entry| entry.path == path)
            .with_context(|| format!("runtime manifest has no {path}"))?;
        let target = inputs.join(to);
        copy_blob(
            &cas,
            entry
                .blob_hash
                .as_deref()
                .context("runtime member has no blob")?,
            &target,
        )?;
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
        Ok(target)
    };
    let profile = GuestOwnerRuntimeProfile::from_cas(&cas, &runtime_hash)?
        .with_context(|| format!("runtime manifest {runtime_hash} is not in the node's CAS"))?;
    let product_manifest_hash = binding["workload"]["runtime_manifest_hash"]
        .as_str()
        .context("binding workload has no runtime manifest")?;
    let product_value = cas.get_object(product_manifest_hash)?.with_context(|| {
        format!("product manifest {product_manifest_hash} is not in the node's CAS")
    })?;
    let product_manifest_record = inputs.join("product-manifest-record.json");
    std::fs::write(
        &product_manifest_record,
        cas.get_object_bytes_bounded(product_manifest_hash, 4 * 1024 * 1024)?
            .context("product manifest vanished from the node's CAS")?,
    )?;
    let file = manifest_files(&product_value)?
        .into_iter()
        .next()
        .context("product manifest has no file")?;
    let (source, lease) = match (file.blob_hash.as_deref(), file.file_sha256.as_deref()) {
        (Some(blob), _) => {
            let target = inputs.join("product-source");
            copy_blob(&cas, blob, &target)?;
            (target, None)
        }
        (None, Some(large)) => {
            let runtime_directory = lillux::PinnedDirectory::open(&state)?
                .context("node has no runtime state directory")?;
            let store = ryeos_state::LargeObjectStore::open_under(&runtime_directory)?
                .context("node has no large-object store")?;
            let lease =
                store.lease_object(large, file.size.context("large product file has no size")?)?;
            (
                lease.path().to_path_buf(),
                Some((lease, store, runtime_directory)),
            )
        }
        _ => anyhow::bail!("product file has no content"),
    };
    Ok(NodeSelection {
        // A candidate owner build may stand in for the installed one.
        owner: match std::env::var_os("RYEOS_GUEST_START_REHEARSAL_OWNER") {
            Some(path) => PathBuf::from(path),
            None => member(&format!("bin/{OWNER}"), "owner")?,
        },
        profile,
        // A candidate supervisor build may stand in for the installed one.
        supervisor: match std::env::var_os("RYEOS_GUEST_START_REHEARSAL_SUPERVISOR") {
            Some(path) => PathBuf::from(path),
            None => find_bundle_member(app, SUPERVISOR, &field("supervisor_artifact_hash")?)?,
        },
        launcher: match std::env::var_os("RYEOS_GUEST_START_REHEARSAL_LAUNCHER") {
            Some(path) => PathBuf::from(path),
            None => find_bundle_member(app, LAUNCHER, &field("launcher_artifact_hash")?)?,
        },
        product_member: (file.path, source, file.mode.unwrap_or(0o755)),
        _product_lease: lease,
        product_manifest_record,
        relay: serde_json::from_value(binding["controller_transport"].clone())?,
        network_inputs: serde_json::from_value(binding["provider_network_inputs"].clone())?,
        attach_budget_seconds: binding["observation_timeout_seconds"]
            .as_u64()
            .context("binding has no observation timeout")?,
    })
}

/// The retained record of a single-file product and a scratch copy of its
/// content, read through a lease on the node's large-object store.
fn leased_file_product(
    app: &Path,
    cas: &lillux::CasStore,
    manifest_hash: &str,
    inputs: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let record = cas
        .get_object_bytes_bounded(manifest_hash, 4 * 1024 * 1024)?
        .with_context(|| {
            format!("file product manifest {manifest_hash} is not in the node's CAS")
        })?;
    let file = manifest_files(&serde_json::from_slice(&record)?)?
        .into_iter()
        .next()
        .context("file product manifest has no file")?;
    let large = file
        .file_sha256
        .as_deref()
        .context("file product is not large content")?;
    let state = app.join(".ai/state");
    let runtime_directory =
        lillux::PinnedDirectory::open(&state)?.context("node has no runtime state directory")?;
    let store = ryeos_state::LargeObjectStore::open_under(&runtime_directory)?
        .context("node has no large-object store")?;
    let lease = store.lease_object(large, file.size.context("file product has no size")?)?;
    let content = inputs.join("file-product");
    std::fs::copy(lease.path(), &content)?;
    drop((lease, store, runtime_directory));
    let path = inputs.join("file-product-record.json");
    std::fs::write(&path, record)?;
    Ok((path, content))
}

/// Copy every file of one installed manifest under `to` (a directory mount)
/// or to `to` itself (a single-file mount).
fn materialize(
    app: &Path,
    cas: &lillux::CasStore,
    manifest_hash: &str,
    to: &Path,
    single: bool,
) -> Result<PathBuf> {
    let record = cas
        .get_object_bytes_bounded(manifest_hash, 4 * 1024 * 1024)?
        .with_context(|| format!("manifest {manifest_hash} is not in the node's CAS"))?;
    let files = manifest_files(&serde_json::from_slice(&record)?)?;
    let state = app.join(".ai/state");
    let runtime_directory =
        lillux::PinnedDirectory::open(&state)?.context("node has no runtime state directory")?;
    let store = ryeos_state::LargeObjectStore::open_under(&runtime_directory)?
        .context("node has no large-object store")?;
    if !single {
        std::fs::create_dir_all(to)?;
    }
    for file in &files {
        let target = if single {
            to.to_path_buf()
        } else {
            to.join(&file.path)
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match (file.blob_hash.as_deref(), file.file_sha256.as_deref()) {
            (Some(blob), _) => copy_blob(cas, blob, &target)?,
            (None, Some(large)) => {
                let lease =
                    store.lease_object(large, file.size.context("large file has no size")?)?;
                std::fs::copy(lease.path(), &target)?;
            }
            _ => anyhow::bail!("manifest file {} has no content", file.path),
        }
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            &target,
            std::fs::Permissions::from_mode(file.mode.unwrap_or(0o644)),
        )?;
    }
    let record_path = PathBuf::from(format!("{}.manifest.json", to.display()));
    std::fs::write(&record_path, record)?;
    Ok(record_path)
}

/// Rehearse the candidate program and guest inputs a controller retained for
/// one placement (its sealed bootstrap, as written by the controller-side
/// dump), rematerialized from the node's stores, under the node's products.
fn rehearse_retained(
    app: &Path,
    binding: &str,
    retained: &Path,
    export: Option<&Path>,
) -> Result<serde_json::Value> {
    use ryeos_external_execution::guest_start_rehearsal::{
        RehearsalCandidate, RehearsalMount, RehearsalMountSource,
    };
    let document: serde_json::Value = serde_json::from_slice(&std::fs::read(retained)?)?;
    let bootstrap = &document["bootstrap"];
    let guest_inputs: ryeos_external_execution_contract::ExternalGuestInputProjection =
        serde_json::from_value(bootstrap["guest_inputs"].clone())?;
    let inputs = scratch();
    let cas = lillux::CasStore::new(app.join(".ai/state/objects"));
    let mut sources = Vec::new();
    for (index, input) in guest_inputs.inputs.iter().enumerate() {
        use ryeos_external_execution_contract::{GuestMountContentAuthority, GuestMountKind};
        sources.push(match &input.content_authority {
            GuestMountContentAuthority::ProductManifest { manifest_hash, .. } => {
                let to = inputs.path().join(format!("input-{index:02}"));
                let record = materialize(
                    app,
                    &cas,
                    manifest_hash,
                    &to,
                    input.kind == GuestMountKind::RegularFile,
                )?;
                Some((to, record))
            }
            GuestMountContentAuthority::PrivateScratch { .. } => None,
            other => anyhow::bail!(
                "retained input {} has unsupported content authority {other:?}",
                input.authority_id
            ),
        });
    }
    let mounts: Vec<RehearsalMount<'_>> = guest_inputs
        .inputs
        .iter()
        .zip(&sources)
        .map(|(input, source)| RehearsalMount {
            role: input.role,
            authority_id: &input.authority_id,
            destination: &input.destination,
            kind: input.kind,
            access: input.access,
            normalized_mode: input.normalized_mode,
            source: match (source, &input.content_authority) {
                (Some((content, record)), _) => RehearsalMountSource::Product {
                    content,
                    manifest_record: record,
                },
                (
                    None,
                    ryeos_external_execution_contract::GuestMountContentAuthority::PrivateScratch {
                        binding_hash,
                    },
                ) => RehearsalMountSource::PrivateScratch { binding_hash },
                _ => unreachable!(),
            },
        })
        .collect();
    let selection = node_selection(app, binding, inputs.path())?;
    eprintln!(
        "rehearsing the retained candidate of binding {binding} with {} mounts",
        mounts.len()
    );
    let host_executable = std::env::current_exe()?;
    let work_parent = work_parent();
    let request = GuestStartRehearsalRequest {
        owner_executable: &selection.owner,
        profile: selection.profile,
        supervisor_executable: &selection.supervisor,
        launcher_executable: &selection.launcher,
        product: inputs.path(),
        product_manifest_record: None,
        file_products: &[],
        base_snapshot: Some(GuestStartRehearsalBase {
            cas: &cas,
            snapshot_hash: &guest_inputs.base_snapshot.snapshot_hash,
        }),
        relay: selection.relay,
        network_inputs: selection.network_inputs,
        attach_budget: lillux::time::Duration::from_secs(selection.attach_budget_seconds),
        document_lifetime: document_lifetime(export),
        work_parent: &work_parent,
        host_executable: &host_executable,
        candidate: Some(RehearsalCandidate {
            program: serde_json::from_value(bootstrap["candidate_program"].clone())?,
            bootstrap_schema: bootstrap["schema"]
                .as_u64()
                .and_then(|schema| u32::try_from(schema).ok())
                .context("retained bootstrap has no schema")?,
            mounts: &mounts,
            environment: guest_inputs.environment.clone(),
            executable_search: guest_inputs.executable_search.clone(),
            limits: serde_json::from_value(bootstrap.clone())?,
        }),
        answer_attach: std::env::var_os("RYEOS_GUEST_START_REHEARSAL_ANSWER_ATTACH").is_some(),
    };
    conclude(&request, export)
}

fn rehearse_node(app: &Path, binding: &str, export: Option<&Path>) -> Result<serde_json::Value> {
    let inputs = scratch();
    // The base an allocation of this binding delivered (its reservation's
    // `base_snapshot_hash`); absent, the rehearsal delivers an empty project.
    let base_hash = std::env::var("RYEOS_GUEST_START_REHEARSAL_BASE_SNAPSHOT").ok();
    let cas = lillux::CasStore::new(app.join(".ai/state/objects"));
    // A single-file product the activation also mounts (the realization a
    // reservation's qualification use names as its provider executable).
    let file_product = match std::env::var("RYEOS_GUEST_START_REHEARSAL_FILE_PRODUCT") {
        Ok(hash) => Some(leased_file_product(app, &cas, &hash, inputs.path())?),
        Err(_) => None,
    };
    let file_products: Vec<GuestStartRehearsalFileProduct<'_>> = file_product
        .iter()
        .map(|(record, content)| GuestStartRehearsalFileProduct {
            authority_id: "provider-executable",
            destination: "/ryeos/realizations/provider-executable",
            file: content,
            manifest_record: record,
        })
        .collect();
    let selection = node_selection(app, binding, inputs.path())?;
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
    let host_executable = std::env::current_exe()?;
    let work_parent = work_parent();
    let request = GuestStartRehearsalRequest {
        owner_executable: &selection.owner,
        profile: selection.profile,
        supervisor_executable: &selection.supervisor,
        launcher_executable: &selection.launcher,
        product: &inputs.path().join("product"),
        product_manifest_record: Some(&selection.product_manifest_record),
        file_products: &file_products,
        base_snapshot: base_hash
            .as_deref()
            .map(|snapshot_hash| GuestStartRehearsalBase {
                cas: &cas,
                snapshot_hash,
            }),
        relay: selection.relay,
        network_inputs: selection.network_inputs,
        attach_budget: lillux::time::Duration::from_secs(selection.attach_budget_seconds),
        document_lifetime: document_lifetime(export),
        work_parent: &work_parent,
        host_executable: &host_executable,
        candidate: None,
        answer_attach: false,
    };
    conclude(&request, export)
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

fn rehearse_synthetic(
    bin: &Path,
    private_source_max_bytes: u64,
) -> Result<GuestStartRehearsalOutcome> {
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
        base_snapshot: None,
        file_products: &[],
        relay: synthetic_relay(),
        network_inputs: synthetic_network(),
        attach_budget: lillux::time::Duration::from_secs(300),
        document_lifetime: None,
        work_parent: &work_parent(),
        host_executable: &std::env::current_exe()?,
        candidate: None,
        answer_attach: false,
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
        let export = std::env::var_os("RYEOS_GUEST_START_REHEARSAL_EXPORT").map(PathBuf::from);
        let outcome = match std::env::var_os("RYEOS_GUEST_START_REHEARSAL_RETAINED") {
            Some(retained) => rehearse_retained(
                Path::new(&app),
                &binding,
                Path::new(&retained),
                export.as_deref(),
            )
            .expect("rehearse the retained candidate"),
            None => rehearse_node(Path::new(&app), &binding, export.as_deref())
                .expect("rehearse node selection"),
        };
        println!("{}", serde_json::to_string_pretty(&outcome).unwrap());
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
