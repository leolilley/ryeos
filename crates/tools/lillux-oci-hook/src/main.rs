//! Installed OCI lifecycle hook for the hard-contained workflow product.
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::path::{Path, PathBuf};

const APP_ROOT: &str = "/data/app";
const BINDING_DIRECTORY: &str = "/run/ryeos";
const BINDING_NAME: &str = "host-runtime.json";
const HOST_STATE_ROOT: &str = "/var/lib/ryeos/contained-oci";
const MAX_RECORD_BYTES: u64 = 131_072;
const CONTROLLER_UID: u32 = 10001;
const CONTROLLER_GID: u32 = 10001;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleRecord {
    schema: u32,
    container_id: String,
    lease_name: String,
    phase: LifecyclePhase,
    lifecycle: Option<lillux::OciLifecycleGeneration>,
    binding_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LifecyclePhase {
    Intent,
    Prepared,
    Active,
    Released,
}

fn main() -> Result<()> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let operation = arguments.next().context("missing OCI hook operation")?;
    lillux::require_administrator()?;
    require_installed_executable()?;
    if operation.to_str() == Some("install-host-state") {
        if arguments.next().is_some() {
            bail!("usage: ryeos-lillux-oci-hook install-host-state");
        }
        return install_host_state();
    }
    if operation.to_str() == Some("recover") {
        let container_id = arguments
            .next()
            .context("recover requires a container ID")?;
        if arguments.next().is_some() {
            bail!("usage: ryeos-lillux-oci-hook recover <container-id>");
        }
        return recover(container_id.to_str().context("container ID is not UTF-8")?);
    }
    if arguments.next().is_some() {
        bail!("usage: ryeos-lillux-oci-hook <prestart|poststop>");
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .take((lillux::OciHookState::MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let state = lillux::OciHookState::parse_bounded(&bytes).map_err(anyhow::Error::msg)?;
    match operation.to_str() {
        Some("prestart") => prestart(state),
        Some("poststop") => poststop(state),
        _ => bail!("usage: ryeos-lillux-oci-hook <prestart|poststop>"),
    }
}

fn require_installed_executable() -> Result<()> {
    let executable = std::env::current_exe()?;
    let parent = lillux::PinnedDirectory::open_owned_hierarchy(
        executable
            .parent()
            .context("hook executable has no parent")?,
        0,
    )?
    .context("hook executable parent is absent")?;
    let file = parent
        .open_pinned_regular(
            executable
                .file_name()
                .context("hook executable has no filename")?,
            false,
        )?
        .context("hook executable disappeared")?;
    file.require_owner(0)?;
    file.require_executable()?;
    Ok(())
}

fn install_host_state() -> Result<()> {
    let var_lib = lillux::PinnedDirectory::open_owned_hierarchy(Path::new("/var/lib"), 0)?
        .context("administrator state parent /var/lib is absent")?;
    let ryeos = var_lib.open_or_create_child("ryeos".as_ref(), 0o700)?;
    ryeos.require_owner(0)?;
    let state = ryeos.open_or_create_child("contained-oci".as_ref(), 0o700)?;
    state.require_owner(0)?;
    Ok(())
}

fn host_state() -> Result<lillux::PinnedDirectory> {
    let directory = lillux::PinnedDirectory::open_owned_hierarchy(Path::new(HOST_STATE_ROOT), 0)?
        .context("contained OCI host-state directory is absent")?;
    directory.require_owner(0)?;
    Ok(directory)
}

fn container_record_name(container_id: &str) -> String {
    format!("container-{container_id}.json")
}

fn volume_record_name(app_root: &lillux::PinnedDirectory) -> Result<String> {
    let identity = serde_json::to_value(app_root.identity()?)?;
    let canonical = lillux::canonical_json(&identity)?;
    Ok(format!(
        "volume-{}.json",
        lillux::sha256_hex(canonical.as_bytes())
    ))
}

fn read_record(directory: &lillux::PinnedDirectory, name: &str) -> Result<Option<LifecycleRecord>> {
    let Some(file) = directory.open_pinned_regular(name.as_ref(), false)? else {
        return Ok(None);
    };
    file.require_owner(0)?;
    let observation = file.observation()?;
    Ok(Some(serde_json::from_slice(
        &file.read_stable_bounded(&observation, MAX_RECORD_BYTES)?,
    )?))
}

fn write_record(
    directory: &lillux::PinnedDirectory,
    name: &str,
    record: &LifecycleRecord,
) -> Result<()> {
    let existing = directory.open_pinned_regular(name.as_ref(), false)?;
    if let Some(existing) = &existing {
        existing.require_owner(0)?;
    }
    let bytes = lillux::canonical_json(&serde_json::to_value(record)?)?.into_bytes();
    directory.atomic_write_pinned_if_same(name.as_ref(), existing.as_ref(), &bytes, 0o600)?;
    Ok(())
}

fn prestart(state: lillux::OciHookState) -> Result<()> {
    state.validate_prestart().map_err(anyhow::Error::msg)?;
    let host_state = host_state()?;
    let observed_app_path = PathBuf::from(format!("/proc/{}/root{APP_ROOT}", state.pid));
    let app_root = lillux::PinnedDirectory::open(&observed_app_path)?
        .context("contained OCI app root is absent through the init root")?;
    lillux::ControllerAccount::unix(CONTROLLER_UID, CONTROLLER_GID)
        .require_directory_owner(&app_root)?;
    let lease_name = volume_record_name(&app_root)?;
    let container_name = container_record_name(&state.id);
    if read_record(&host_state, &lease_name)?
        .is_some_and(|record| record.phase != LifecyclePhase::Released)
    {
        bail!("contained OCI app root has an unreleased preceding lifetime");
    }
    if read_record(&host_state, &container_name)?
        .is_some_and(|record| record.phase != LifecyclePhase::Released)
    {
        bail!("contained OCI identity has an unreleased preceding lifetime");
    }
    let mut record = LifecycleRecord {
        schema: 1,
        container_id: state.id.clone(),
        lease_name: lease_name.clone(),
        phase: LifecyclePhase::Intent,
        lifecycle: None,
        binding_digest: None,
    };
    // Publish the volume lease before any kernel mutation. An interrupted
    // intent is quarantined rather than guessed safe.
    write_record(&host_state, &lease_name, &record)?;
    write_record(&host_state, &container_name, &record)?;
    let (process_scopes, lifecycle) =
        lillux::ProcessScopeConfiguration::prepare_oci_hook(&state).map_err(anyhow::Error::msg)?;
    record.phase = LifecyclePhase::Prepared;
    record.lifecycle = Some(lifecycle.clone());
    write_record(&host_state, &lease_name, &record)?;
    write_record(&host_state, &container_name, &record)?;
    lillux::ProcessScopeConfiguration::enter_oci_mount_namespace(&state)
        .map_err(anyhow::Error::msg)?;
    let binding = ryeos_node::host_runtime::HostRuntimeBinding::capture_oci_observed(
        &app_root,
        PathBuf::from(APP_ROOT),
        lillux::ControllerAccount::unix(CONTROLLER_UID, CONTROLLER_GID),
        process_scopes,
        lifecycle,
    )?;
    let digest = binding.identity_digest()?;
    let directory = lillux::PinnedDirectory::open_owned_hierarchy(Path::new(BINDING_DIRECTORY), 0)?
        .context("contained OCI binding directory is absent")?;
    let bytes = lillux::canonical_json(&serde_json::to_value(binding)?)?.into_bytes();
    let existing = directory.open_pinned_regular(BINDING_NAME.as_ref(), false)?;
    if let Some(existing) = &existing {
        existing.require_owner(0)?;
    }
    directory.atomic_write_pinned_if_same(
        BINDING_NAME.as_ref(),
        existing.as_ref(),
        &bytes,
        0o600,
    )?;
    record.phase = LifecyclePhase::Active;
    record.binding_digest = Some(digest);
    write_record(&host_state, &lease_name, &record)?;
    write_record(&host_state, &container_name, &record)?;
    Ok(())
}

fn poststop(state: lillux::OciHookState) -> Result<()> {
    state.validate_poststop().map_err(anyhow::Error::msg)?;
    release(&state.id)
}

fn recover(container_id: &str) -> Result<()> {
    if container_id.is_empty()
        || container_id.len() > 128
        || !container_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("recover requires a canonical container ID");
    }
    release(container_id)
}

fn release(container_id: &str) -> Result<()> {
    let host_state = host_state()?;
    let container_name = container_record_name(container_id);
    let mut record = read_record(&host_state, &container_name)?
        .context("poststop has no retained lifecycle record")?;
    if record.schema != 1
        || record.container_id != container_id
        || record.phase == LifecyclePhase::Released
    {
        bail!("contained OCI lifecycle record is not active");
    }
    let lease = read_record(&host_state, &record.lease_name)?
        .context("poststop has no retained app-root lease")?;
    if lease != record {
        bail!("container index and app-root lease disagree");
    }
    record
        .lifecycle
        .as_ref()
        .context("lifecycle intent never acquired an exact OCI generation")?
        .prove_ended_and_retire()
        .map_err(anyhow::Error::msg)?;
    record.phase = LifecyclePhase::Released;
    write_record(&host_state, &record.lease_name, &record)?;
    write_record(&host_state, &container_name, &record)?;
    Ok(())
}
