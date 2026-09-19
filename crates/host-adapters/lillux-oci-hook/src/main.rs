//! Installed OCI lifecycle hook for the hard-contained workflow product.
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};

mod docker_runtime;

const APP_ROOT: &str = "/data/app";
const BINDING_DIRECTORY: &str = "/run/ryeos";
const BINDING_NAME: &str = "host-runtime.json";
const HOST_STATE_ROOT: &str = "/var/lib/ryeos/contained-oci";
const SETUP_TRANSACTION_NAME: &str = "setup-transaction.json";
const MAX_RECORD_BYTES: u64 = 131_072;
const CONTROLLER_UID: u32 = 10001;
const CONTROLLER_GID: u32 = 10001;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleRecord {
    schema: u32,
    container_id: String,
    lease_name: String,
    phase: LifecyclePhase,
    intent: lillux::OciLifecycleIntent,
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

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupTransaction {
    schema: u32,
    open: bool,
    record: LifecycleRecord,
}

fn phase_rank(phase: LifecyclePhase) -> u8 {
    match phase {
        LifecyclePhase::Intent => 0,
        LifecyclePhase::Prepared => 1,
        LifecyclePhase::Active => 2,
        LifecyclePhase::Released => 3,
    }
}

fn reconcile_records(first: LifecycleRecord, second: LifecycleRecord) -> Result<LifecycleRecord> {
    if first.container_id != second.container_id
        || first.lease_name != second.lease_name
        || first.intent != second.intent
        || first
            .lifecycle
            .as_ref()
            .zip(second.lifecycle.as_ref())
            .is_some_and(|(a, b)| a != b)
        || first
            .binding_digest
            .as_ref()
            .zip(second.binding_digest.as_ref())
            .is_some_and(|(a, b)| a != b)
    {
        bail!("container index and app-root lease disagree on immutable authority");
    }
    Ok(if phase_rank(first.phase) >= phase_rank(second.phase) {
        first
    } else {
        second
    })
}

impl LifecycleRecord {
    fn validate(&self, expected_name: Option<&str>) -> Result<()> {
        if self.schema != 1
            || self.container_id.is_empty()
            || self.container_id.len() > 128
            || !self
                .container_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || !self.lease_name.starts_with("volume-")
            || self.lease_name.len() != "volume-".len() + 64 + ".json".len()
            || !self.lease_name.ends_with(".json")
            || !self.lease_name["volume-".len()..self.lease_name.len() - ".json".len()]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("contained OCI lifecycle record is not canonical");
        }
        if let Some(name) = expected_name
            && name != self.lease_name
            && name != container_record_name(&self.container_id)
        {
            bail!("contained OCI lifecycle record is stored under the wrong key");
        }
        match self.phase {
            LifecyclePhase::Intent if self.lifecycle.is_none() && self.binding_digest.is_none() => {
            }
            LifecyclePhase::Prepared
                if self.lifecycle.is_some() && self.binding_digest.is_none() => {}
            LifecyclePhase::Active if self.lifecycle.is_some() && self.binding_digest.is_some() => {
            }
            LifecyclePhase::Released
                if self.lifecycle.is_some() || self.binding_digest.is_none() => {}
            _ => bail!("contained OCI lifecycle phase fields disagree"),
        }
        self.intent.validate().map_err(anyhow::Error::msg)?;
        self.intent
            .require_container_id(&self.container_id)
            .map_err(anyhow::Error::msg)?;
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.validate().map_err(anyhow::Error::msg)?;
            lifecycle
                .require_container_id(&self.container_id)
                .map_err(anyhow::Error::msg)?;
            lifecycle
                .require_intent(&self.intent)
                .map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let operation = arguments.next().context("missing OCI hook operation")?;
    lillux::require_administrator()?;
    require_installed_executable()?;
    if operation.to_str() == Some("docker-runtime") {
        return docker_runtime::run(arguments.collect());
    }
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

fn lock_host_state(directory: &lillux::PinnedDirectory) -> Result<std::fs::File> {
    let descriptor = directory.try_clone_descriptor()?;
    if unsafe { libc::flock(descriptor.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(descriptor)
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
    let record: LifecycleRecord =
        serde_json::from_slice(&file.read_stable_bounded(&observation, MAX_RECORD_BYTES)?)?;
    record.validate(Some(name))?;
    Ok(Some(record))
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

fn read_setup_transaction(directory: &lillux::PinnedDirectory) -> Result<Option<SetupTransaction>> {
    let Some(file) = directory.open_pinned_regular(SETUP_TRANSACTION_NAME.as_ref(), false)? else {
        return Ok(None);
    };
    file.require_owner(0)?;
    let observation = file.observation()?;
    let transaction: SetupTransaction =
        serde_json::from_slice(&file.read_stable_bounded(&observation, MAX_RECORD_BYTES)?)?;
    if transaction.schema != 1
        || !matches!(
            transaction.record.phase,
            LifecyclePhase::Intent | LifecyclePhase::Released
        )
    {
        bail!("contained OCI setup transaction is not canonical");
    }
    transaction.record.validate(None)?;
    Ok(Some(transaction))
}

fn write_setup_transaction(
    directory: &lillux::PinnedDirectory,
    transaction: &SetupTransaction,
) -> Result<()> {
    let existing = directory.open_pinned_regular(SETUP_TRANSACTION_NAME.as_ref(), false)?;
    if let Some(existing) = &existing {
        existing.require_owner(0)?;
    }
    let bytes = lillux::canonical_json(&serde_json::to_value(transaction)?)?.into_bytes();
    directory.atomic_write_pinned_if_same(
        SETUP_TRANSACTION_NAME.as_ref(),
        existing.as_ref(),
        &bytes,
        0o600,
    )?;
    Ok(())
}

fn prestart(state: lillux::OciHookState) -> Result<()> {
    state.validate_prestart().map_err(anyhow::Error::msg)?;
    let host_state = host_state()?;
    let _transaction = lock_host_state(&host_state)?;
    if read_setup_transaction(&host_state)?.is_some_and(|transaction| transaction.open) {
        bail!("an interrupted contained OCI setup requires recovery");
    }
    let intent = lillux::OciLifecycleIntent::observe(&state).map_err(anyhow::Error::msg)?;
    intent.require_live().map_err(anyhow::Error::msg)?;
    let process_root = intent.open_process_root().map_err(anyhow::Error::msg)?;
    let app_root = process_root
        .open_directory(Path::new(APP_ROOT.trim_start_matches('/')))
        .map_err(anyhow::Error::msg)
        .context("contained OCI app root is absent through the exact init root")?;
    let binding_directory = process_root
        .open_directory(Path::new(BINDING_DIRECTORY.trim_start_matches('/')))
        .map_err(anyhow::Error::msg)
        .context("contained OCI binding directory is absent through the exact init root")?;
    binding_directory.require_owner(0)?;
    intent.require_live().map_err(anyhow::Error::msg)?;
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
        intent,
        lifecycle: None,
        binding_digest: None,
    };
    // Publish one fixed transaction journal before either secondary index.
    // Thus a crash between index writes blocks every new setup until exact
    // recovery converges the intended volume and container records.
    let mut setup = SetupTransaction {
        schema: 1,
        open: true,
        record: LifecycleRecord {
            schema: record.schema,
            container_id: record.container_id.clone(),
            lease_name: record.lease_name.clone(),
            phase: record.phase,
            intent: record.intent.clone(),
            lifecycle: None,
            binding_digest: None,
        },
    };
    write_setup_transaction(&host_state, &setup)?;
    write_record(&host_state, &container_name, &record)?;
    write_record(&host_state, &lease_name, &record)?;
    setup.open = false;
    write_setup_transaction(&host_state, &setup)?;
    let account = lillux::ControllerAccount::unix(CONTROLLER_UID, CONTROLLER_GID);
    let (process_scopes, lifecycle) = lillux::ProcessScopeConfiguration::prepare_oci_hook(
        &state,
        &account,
        &record.intent,
        &process_root,
    )
    .map_err(anyhow::Error::msg)?;
    record.phase = LifecyclePhase::Prepared;
    record.lifecycle = Some(lifecycle.clone());
    write_record(&host_state, &lease_name, &record)?;
    write_record(&host_state, &container_name, &record)?;
    lillux::ProcessScopeConfiguration::enter_oci_mount_namespace(
        &state,
        &record.intent,
        &process_root,
    )
    .map_err(anyhow::Error::msg)?;
    process_root
        .require_retained_lifetime()
        .map_err(anyhow::Error::msg)?;
    let binding = ryeos_node::host_runtime::HostRuntimeBinding::capture_oci_observed(
        &app_root,
        PathBuf::from(APP_ROOT),
        account,
        process_scopes,
        lifecycle,
    )?;
    let digest = binding.identity_digest()?;
    let bytes = lillux::canonical_json(&serde_json::to_value(binding)?)?.into_bytes();
    let existing = binding_directory.open_pinned_regular(BINDING_NAME.as_ref(), false)?;
    if let Some(existing) = &existing {
        existing.require_owner(0)?;
    }
    binding_directory.atomic_write_pinned_if_same(
        BINDING_NAME.as_ref(),
        existing.as_ref(),
        &bytes,
        0o600,
    )?;
    process_root
        .require_retained_lifetime()
        .map_err(anyhow::Error::msg)?;
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
    let _transaction = lock_host_state(&host_state)?;
    let container_name = container_record_name(container_id);
    let mut setup = read_setup_transaction(&host_state)?;
    if let Some(transaction) = setup.as_mut()
        && transaction.open
    {
        if transaction.record.container_id != container_id {
            bail!("a different interrupted contained OCI setup requires recovery");
        }
        let container_index = read_record(&host_state, &container_name)?;
        let volume_index = read_record(&host_state, &transaction.record.lease_name)?;
        for index in [container_index.as_ref(), volume_index.as_ref()] {
            if let Some(index) = index {
                let allowed = match transaction.record.phase {
                    LifecyclePhase::Intent => {
                        index.phase == LifecyclePhase::Released || index == &transaction.record
                    }
                    LifecyclePhase::Released => index.intent == transaction.record.intent,
                    _ => false,
                };
                if !allowed {
                    bail!("interrupted lifecycle index differs from its authoritative journal");
                }
            }
        }
        if transaction.record.phase == LifecyclePhase::Intent {
            transaction
                .record
                .intent
                .prove_ended_and_retire()
                .map_err(anyhow::Error::msg)?;
        }
        let mut released = transaction.record.clone();
        released.phase = LifecyclePhase::Released;
        write_record(&host_state, &released.lease_name, &released)?;
        write_record(&host_state, &container_name, &released)?;
        transaction.open = false;
        write_setup_transaction(&host_state, transaction)?;
        return Ok(());
    }
    let container_record = match read_record(&host_state, &container_name)? {
        Some(record) => record,
        None => bail!("poststop has no retained lifecycle record"),
    };
    if container_record.schema != 1 || container_record.container_id != container_id {
        bail!("contained OCI lifecycle record is not active");
    }
    let lease = read_record(&host_state, &container_record.lease_name)?;
    let mut record = match lease {
        Some(lease) => reconcile_records(container_record, lease)?,
        None if container_record.phase == LifecyclePhase::Intent => container_record,
        None => bail!("poststop has no retained app-root lease"),
    };
    if record.phase == LifecyclePhase::Released {
        write_record(&host_state, &record.lease_name, &record)?;
        write_record(&host_state, &container_name, &record)?;
        return Ok(());
    }
    if let Some(lifecycle) = &record.lifecycle {
        lifecycle
            .prove_ended_and_retire()
            .map_err(anyhow::Error::msg)?;
    } else {
        record
            .intent
            .prove_ended_and_retire()
            .map_err(anyhow::Error::msg)?;
    }
    record.phase = LifecyclePhase::Released;
    let mut release_transaction = SetupTransaction {
        schema: 1,
        open: true,
        record: record.clone(),
    };
    write_setup_transaction(&host_state, &release_transaction)?;
    write_record(&host_state, &record.lease_name, &record)?;
    write_record(&host_state, &container_name, &record)?;
    release_transaction.open = false;
    write_setup_transaction(&host_state, &release_transaction)?;
    Ok(())
}
