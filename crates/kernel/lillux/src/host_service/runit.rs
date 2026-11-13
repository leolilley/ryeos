//! Runit host-service adapter. `/etc/runit`, `sv`, `down`, and `supervise`
//! are native packaging details and intentionally do not escape this module.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{HostServiceController, HostServiceInstallation, HostServiceLaunch};
use crate::{
    PinnedDirectory, PinnedEntryType, PinnedRegularFile, SubprocessLimits, SubprocessRequest,
};

const RECORDS: &str = "lillux-host-service";
const HOST_SERVICE_TEMP_PURPOSE: crate::TempPurpose = crate::TempPurpose::new("host-service");
const LAUNCH_RECORD: &str = "launch.json";
const MANAGER_RECORD: &str = "manager.json";
const STATE_DIRECTORY: &str = "state";
const RUN_PROGRAM: &str = "run";
const DOWN_MARKER: &str = "down";
const CONTROL: &str = "control";
const NATIVE_SCRIPT_INTERPRETER: &str = "/bin/sh";
// `sv` probes `ok` before submitting its request through `control`. Both are
// native runit IPC endpoints, deliberately contained in this adapter.
const OPERATOR_SUPERVISOR_FIFOS: [&str; 2] = ["ok", CONTROL];
const SUPERVISE_DIRECTORY: &str = "supervise";
// runsv supervises `<service>/log` as the service's logger and feeds it the
// service's stdout. `sv status` also probes the logger's `ok`, so the account
// gets that probe only; logger control stays with the administrator.
const LOG_SERVICE: &str = "log";
const LOGGER_STATUS_FIFOS: [&str; 1] = ["ok"];
// Logger output lives outside the service definition, under a base named for
// this adapter rather than for any client: one directory per service name,
// owned by the service's account, beneath an administrator-owned base.
const LOG_ROOT: &str = "/var/log/lillux-host-service";
const LOG_DIRECTORY_TOOL: &str = "/usr/bin/install";
const LOG_CREDENTIAL_TOOL: &str = "/usr/bin/chpst";
const LOG_WRITER_TOOL: &str = "/usr/bin/svlogd";
// Every association now carries an adapter-rendered logger. Associations
// published under an earlier contract are refused and must be re-provisioned.
const RUNIT_INTEGRATION_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunitUpgradeState {
    schema_version: u32,
    initially_down: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunitConfiguration {
    schema_version: u32,
    control_executable: PathBuf,
}

fn service_directory() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Some(PathBuf::from("/etc/runit/sv"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn activation_directory() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Some(PathBuf::from("/etc/runit/runsvdir/default"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

struct RunitService {
    directory: PinnedDirectory,
    executable: PinnedRegularFile,
}

impl RunitService {
    fn down_file(&self) -> Result<Option<PinnedRegularFile>> {
        self.directory.ensure_path_binding()?;
        let file = self
            .directory
            .open_pinned_regular(OsStr::new(DOWN_MARKER), false)?;
        if let Some(file) = &file {
            file.require_owner(0)?;
        }
        Ok(file)
    }

    fn open(directory: PinnedDirectory, executable: &Path) -> Result<Self> {
        directory.require_owner(0)?;
        directory.ensure_path_binding()?;
        let parent = PinnedDirectory::open_owned_hierarchy(
            executable
                .parent()
                .context("service-control executable has no parent")?,
            0,
        )?
        .context("service-control executable directory is absent")?;
        let file = parent
            .open_pinned_regular(
                executable
                    .file_name()
                    .context("service-control executable has no filename")?,
                false,
            )?
            .context("service-control executable is absent")?;
        file.require_owner(0)?;
        file.require_executable()?;
        Ok(Self {
            directory,
            executable: file,
        })
    }

    fn control(&self, operation: &str) -> Result<()> {
        self.control_with(&[operation], 5.0)
    }

    /// Run `sv` with `options` ahead of the service directory, bounded by
    /// `timeout` seconds.
    fn control_with(&self, options: &[&str], timeout: f64) -> Result<()> {
        let operation = options.join(" ");
        self.directory.ensure_path_binding()?;
        // The selected account receives traversal, not read/list access, to
        // runit's root-owned `supervise` directory. Do not reopen it through
        // `PinnedDirectory`: its read-only descriptor contract correctly
        // requires directory read permission and would force us to widen the
        // native delegation. The fixed root-owned `sv` executable below is
        // the native live-supervisor operation; its failure is propagated and
        // never selects a direct-process fallback.
        let executable = self.executable.inherited_descriptor_authority()?;
        let directory = self.directory.inherited_descriptor_authority()?;
        let result = crate::run(SubprocessRequest {
            cmd: executable.path().to_string_lossy().into_owned(),
            argv0: Some("sv".to_owned()),
            args: options
                .iter()
                .map(|option| (*option).to_owned())
                .chain(std::iter::once(
                    directory.path().to_string_lossy().into_owned(),
                ))
                .collect(),
            cwd: None,
            envs: vec![],
            stdin_data: None,
            timeout,
            limits: Some(SubprocessLimits {
                max_stdout_bytes: Some(16 * 1024),
                max_stderr_bytes: Some(16 * 1024),
                ..Default::default()
            }),
            inherited_fds: vec![executable, directory],
            inherited_fd_mappings: vec![],
            supervised_status: None,
        });
        if !result.success {
            bail!(
                "configured runit supervisor {operation} failed: {} {}",
                result.stdout.trim(),
                result.stderr.trim()
            );
        }
        Ok(())
    }
}

impl HostServiceController for RunitService {
    fn check_available(&self) -> Result<()> {
        self.control("status")
    }
    fn request_up(&self) -> Result<()> {
        self.control("up")
    }
    fn request_down(&self) -> Result<()> {
        self.control("down")
    }
    fn inhibit_restart(&self) -> Result<()> {
        // runsv `o` marks the service wanted down without sending TERM/CONT
        // to the running process; `down` would signal it mid-exit.
        self.control("once")
    }
    fn force_down(&self) -> Result<()> {
        // `down` records the want; `kill` has runsv send its own child the
        // definitive signal, so the exact service process ends and nothing
        // relaunches it.
        self.control("down")?;
        self.control("kill")
    }
    fn wait_down(&self, timeout: std::time::Duration) -> Result<()> {
        // `sv -w N down` records the want and exits zero only once runsv
        // reports the service down; on timeout it exits non-zero.
        let seconds = timeout.as_secs().max(1);
        self.control_with(&["-w", &seconds.to_string(), "down"], seconds as f64 + 5.0)
    }
    fn capture_upgrade_state(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(RunitUpgradeState {
            schema_version: 1,
            initially_down: self.down_file()?.is_some(),
        })?)
    }
    fn restore_upgrade_state(&self, value: &serde_json::Value) -> Result<()> {
        let state: RunitUpgradeState = serde_json::from_value(value.clone())?;
        if state.schema_version != 1 {
            bail!("runit upgrade state contract is not current");
        }
        match (state.initially_down, self.down_file()?) {
            (true, None) => self.directory.atomic_write_pinned_if_same(
                OsStr::new(DOWN_MARKER),
                None,
                b"",
                0o644,
                HOST_SERVICE_TEMP_PURPOSE,
            ),
            (false, Some(file)) => self.directory.remove_pinned_regular_if_same(&file),
            _ => Ok(()),
        }
    }
}

fn canonical_launch(launch: &HostServiceLaunch) -> Result<Vec<u8>> {
    if launch.schema_version != 1 {
        bail!("host service launch contract is not current");
    }
    launch.account.validate().map_err(anyhow::Error::msg)?;
    validate_launch_program(launch)?;
    validate_root_executable(Path::new(NATIVE_SCRIPT_INTERPRETER), "service interpreter")?;
    // Validate the native rendering at admission/discovery too. Otherwise a
    // malformed administrator record could be accepted as data and fail only
    // when runit later invokes its shell.
    let _ = run_program(launch)?;
    Ok(crate::canonical_json(&serde_json::to_value(launch)?)?.into_bytes())
}

fn validate_launch_program(launch: &HostServiceLaunch) -> Result<()> {
    if !launch.executable.is_absolute() {
        bail!("host executable must be an absolute path");
    }
    let parent = PinnedDirectory::open_owned_hierarchy(
        launch
            .executable
            .parent()
            .context("host executable has no parent")?,
        0,
    )?
    .context("host executable directory is absent")?;
    let executable = parent
        .open_pinned_regular(
            launch
                .executable
                .file_name()
                .context("host executable has no filename")?,
            false,
        )?
        .context("host executable is absent")?;
    executable.require_owner(0)?;
    executable.require_executable()?;
    Ok(())
}

fn validate_root_executable(path: &Path, label: &str) -> Result<()> {
    // Native interpreter names conventionally traverse administrator-owned
    // compatibility symlinks (`/bin/sh`, and often `/bin` itself). Resolve
    // that fixed host pathname inside Lillux, then pin and validate the exact
    // resulting regular file. Re-resolving after the descriptor checks
    // detects replacement during admission; an administrator racing its own
    // root namespace remains outside the unprivileged threat boundary.
    let resolved =
        crate::canonicalize_existing_path(path).with_context(|| format!("resolve {label}"))?;
    let parent = PinnedDirectory::open_owned_hierarchy(
        resolved
            .parent()
            .with_context(|| format!("{label} has no parent"))?,
        0,
    )?
    .with_context(|| format!("{label} directory is absent"))?;
    let executable = parent
        .open_pinned_regular(
            resolved
                .file_name()
                .with_context(|| format!("{label} has no filename"))?,
            false,
        )?
        .with_context(|| format!("{label} is absent"))?;
    executable.require_owner(0)?;
    executable.require_executable()?;
    if crate::canonicalize_existing_path(path)? != resolved {
        bail!("{label} changed during admission");
    }
    Ok(())
}

fn manager_bytes() -> Result<Vec<u8>> {
    Ok(
        crate::canonical_json(&serde_json::to_value(RunitConfiguration {
            schema_version: RUNIT_INTEGRATION_VERSION,
            control_executable: PathBuf::from("/usr/bin/sv"),
        })?)?
        .into_bytes(),
    )
}

fn shell_quote(value: &str) -> Result<String> {
    if value.contains(['\0', '\n', '\r']) {
        bail!("host launch value contains a control character");
    }
    // POSIX shell single-quote splice: close the literal, emit one quoted
    // apostrophe, then reopen it. Do not insert backslashes here: inside a
    // single-quoted literal they would become launch data rather than escape
    // syntax.
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}

fn run_program(launch: &HostServiceLaunch) -> Result<Vec<u8>> {
    let executable = launch
        .executable
        .to_str()
        .context("host executable is not UTF-8")?;
    // runsv pipes only stdout to the service's log; join stderr to it so a
    // refusal written before the daemon's own logging starts is captured
    // rather than surviving only in the supervisor's process title.
    let mut body = format!("#!{NATIVE_SCRIPT_INTERPRETER}\nexec 2>&1\n");
    for (name, value) in &launch.environment {
        if name.is_empty()
            || !name.bytes().all(|b| b == b'_' || b.is_ascii_alphanumeric())
            || name.as_bytes()[0].is_ascii_digit()
        {
            bail!("host launch environment has an invalid variable name");
        }
        body.push_str("export ");
        body.push_str(name);
        body.push('=');
        body.push_str(&shell_quote(value)?);
        body.push('\n');
    }
    body.push_str("exec ");
    body.push_str(&shell_quote(executable)?);
    for argument in &launch.arguments {
        body.push(' ');
        body.push_str(&shell_quote(argument)?);
    }
    body.push('\n');
    Ok(body.into_bytes())
}

/// Logger program for the service's `log` sub-service. It runs as root under
/// runsv only long enough to establish the exact log directories, then drops
/// to the service account for the log writer. Account ids and paths are fixed
/// at provisioning so the rendered bytes are part of the association contract.
fn log_program(name: &str, launch: &HostServiceLaunch) -> Result<Vec<u8>> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        bail!("host service name must be one exact path component");
    }
    let (uid, gid) = launch.account.unix_ids().map_err(anyhow::Error::msg)?;
    let root = shell_quote(LOG_ROOT)?;
    let directory = shell_quote(&format!("{LOG_ROOT}/{name}"))?;
    Ok(format!(
        "#!{NATIVE_SCRIPT_INTERPRETER}\nexec 2>&1\n\
         {LOG_DIRECTORY_TOOL} -d -o 0 -g 0 -m 0755 {root}\n\
         {LOG_DIRECTORY_TOOL} -d -o {uid} -g {gid} -m 0750 {directory}\n\
         exec {LOG_CREDENTIAL_TOOL} -u :{uid}:{gid} {LOG_WRITER_TOOL} -tt {directory}\n"
    )
    .into_bytes())
}

fn validate_logger_tools() -> Result<()> {
    validate_root_executable(Path::new(LOG_DIRECTORY_TOOL), "logger directory tool")?;
    validate_root_executable(Path::new(LOG_CREDENTIAL_TOOL), "logger credential tool")?;
    validate_root_executable(Path::new(LOG_WRITER_TOOL), "logger writer")
}

fn ensure_root_regular(
    directory: &PinnedDirectory,
    name: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<()> {
    let Some(existing) = directory.open_pinned_regular(OsStr::new(name), false)? else {
        return directory.atomic_write_pinned_if_same(
            OsStr::new(name),
            None,
            bytes,
            mode,
            HOST_SERVICE_TEMP_PURPOSE,
        );
    };
    existing.require_owner(0)?;
    let current = existing.read_stable_bounded(&existing.observation()?, 64 * 1024)?;
    if current != bytes {
        bail!("existing native service record `{name}` conflicts with this host association");
    }
    Ok(())
}

fn decode_matching_launch(bytes: &[u8], expected: &HostServiceLaunch) -> Result<HostServiceLaunch> {
    let actual: HostServiceLaunch = serde_json::from_slice(bytes)
        .context("existing native service launch record is invalid")?;
    if &actual != expected {
        bail!("existing native service record `launch.json` conflicts with this host association");
    }
    Ok(actual)
}

fn require_matching_launch_record(
    directory: &PinnedDirectory,
    expected: &HostServiceLaunch,
) -> Result<()> {
    let file = directory
        .open_pinned_regular(OsStr::new(LAUNCH_RECORD), false)?
        .context("configured native service launch record is absent")?;
    file.require_owner(0)?;
    let actual = decode_matching_launch(
        &file.read_stable_bounded(&file.observation()?, 64 * 1024)?,
        expected,
    )?;
    // The launch record is a typed, root-owned host configuration document,
    // not a CAS object or a signed byte identity. New records are canonical,
    // while idempotent provisioning compares the complete closed value. The
    // separately derived run program remains byte-exact, so formatting cannot
    // change what the native supervisor executes.
    let _ = canonical_launch(&actual)?;
    Ok(())
}

fn require_root_regular(directory: &PinnedDirectory, name: &str, expected: &[u8]) -> Result<()> {
    let file = directory
        .open_pinned_regular(OsStr::new(name), false)?
        .with_context(|| format!("configured native service record `{name}` is absent"))?;
    file.require_owner(0)?;
    let actual = file.read_stable_bounded(&file.observation()?, 64 * 1024)?;
    if actual != expected {
        bail!("configured native service record `{name}` differs from its launch contract");
    }
    Ok(())
}

fn require_logger(
    directory: &PinnedDirectory,
    name: &str,
    launch: &HostServiceLaunch,
) -> Result<()> {
    let logger = directory
        .open_child_directory(OsStr::new(LOG_SERVICE))?
        .context("configured host service has no adapter logger")?;
    logger.require_owner(0)?;
    require_root_regular(&logger, RUN_PROGRAM, &log_program(name, launch)?)
}

fn records(directory: &PinnedDirectory) -> Result<PinnedDirectory> {
    let records = directory
        .open_child_directory(OsStr::new(RECORDS))?
        .context(
            "configured host service has no Lillux records; refusing direct lifecycle fallback",
        )?;
    records.require_owner(0)?;
    Ok(records)
}

fn require_published_service(
    directory: &PinnedDirectory,
    name: &str,
    launch: &HostServiceLaunch,
) -> Result<()> {
    directory.require_owner(0)?;
    let records = records(directory)?;
    let _ = canonical_launch(launch)?;
    require_matching_launch_record(&records, launch)?;
    ensure_root_regular(&records, MANAGER_RECORD, &manager_bytes()?, 0o644)?;
    ensure_root_regular(directory, RUN_PROGRAM, &run_program(launch)?, 0o755)?;
    // runsv selects its logger only when it starts the service, so a logger
    // added to a live association would not run. It must already be exact.
    require_logger(directory, name, launch)?;
    // `down` is native desired state, not realization content. Recreating it
    // during an idempotent provision would silently stop an already-running
    // service, so only first publication creates this marker.
    if let Some(marker) = directory.open_pinned_regular(OsStr::new(DOWN_MARKER), false)? {
        marker.require_owner(0)?;
    }
    let state = records
        .open_child_directory(OsStr::new(STATE_DIRECTORY))?
        .context("configured host service has no client state directory")?;
    state.require_owner(0)?;
    launch.account.grant_readonly_host_directory(&state)?;
    Ok(())
}

fn build_staged_service(
    directory: &PinnedDirectory,
    name: &str,
    launch: &HostServiceLaunch,
) -> Result<()> {
    directory.require_owner(0)?;
    let records = directory.open_or_create_child(OsStr::new(RECORDS), 0o755)?;
    records.require_owner(0)?;
    ensure_root_regular(&records, LAUNCH_RECORD, &canonical_launch(launch)?, 0o644)?;
    ensure_root_regular(&records, MANAGER_RECORD, &manager_bytes()?, 0o644)?;
    ensure_root_regular(directory, RUN_PROGRAM, &run_program(launch)?, 0o755)?;
    // The logger must exist before activation: runsv decides whether the
    // service has one only when it starts supervising the directory.
    let logger = directory.open_or_create_child(OsStr::new(LOG_SERVICE), 0o755)?;
    logger.require_owner(0)?;
    ensure_root_regular(&logger, RUN_PROGRAM, &log_program(name, launch)?, 0o755)?;
    // This is the native *boot* disposition, not RyeOS's desired lifecycle
    // state. It leaves a newly associated service inert across a host reboot;
    // `sv up` may start the current supervisor instance without removing the
    // marker. The client-owned desired record remains the authority for
    // ordinary start/stop and upgrade restoration.
    ensure_root_regular(directory, DOWN_MARKER, b"", 0o644)?;
    let state = records.open_or_create_child(OsStr::new(STATE_DIRECTORY), 0o700)?;
    state.require_owner(0)?;
    launch.account.grant_readonly_host_directory(&state)?;
    Ok(())
}

fn publish_activation_link(name: &str, service_path: &Path) -> Result<()> {
    let activation = activation_directory().context("runit is unavailable on this platform")?;
    let activation = PinnedDirectory::open_owned_hierarchy(&activation, 0)?
        .context("runit activation directory is absent")?;
    let expected = service_path.as_os_str().as_encoded_bytes();
    match activation.read_symlink_target(OsStr::new(name), 4096)? {
        Some(actual) if actual == expected => Ok(()),
        Some(_) => bail!("runit activation link belongs to another service"),
        None => activation.create_symlink(OsStr::new(name), expected),
    }
}

fn require_activation_link(name: &str, service_path: &Path) -> Result<()> {
    let activation = activation_directory().context("runit is unavailable on this platform")?;
    let activation = PinnedDirectory::open_owned_hierarchy(&activation, 0)?
        .context("runit activation directory is absent")?;
    let actual = activation
        .read_symlink_target(OsStr::new(name), 4096)?
        .context("configured runit activation link is absent")?;
    if actual != service_path.as_os_str().as_encoded_bytes() {
        bail!("configured runit activation link belongs to another service");
    }
    Ok(())
}

/// Wait for runsv's IPC endpoints beneath `service/supervise` and grant the
/// account exactly `fifos`. The grant gives the `supervise` directory group
/// traversal only; endpoints not named here stay administrator-only.
fn await_and_grant_supervisor_fifos(
    service: &PinnedDirectory,
    fifos: &[&str],
    account: &crate::ControllerAccount,
) -> Result<()> {
    let deadline = crate::time::MonotonicDeadline::after(crate::time::Duration::from_secs(5));
    loop {
        if let Some(supervise) = service.open_child_directory(OsStr::new(SUPERVISE_DIRECTORY))? {
            supervise.require_owner(0)?;
            let mut ready = true;
            for &name in fifos {
                match supervise.entry_no_follow(OsStr::new(name))? {
                    Some(entry) if entry.entry_type == PinnedEntryType::Fifo => {}
                    Some(_) => bail!("runit supervisor {name} endpoint is not a FIFO"),
                    None => ready = false,
                }
            }
            if ready {
                for &name in fifos {
                    account.grant_private_control_fifo(&supervise, OsStr::new(name))?;
                }
                return Ok(());
            }
        }
        if deadline.has_elapsed() {
            bail!("runit did not create the configured service IPC endpoints");
        }
        crate::time::sleep(crate::time::Duration::from_millis(25));
    }
}

pub(super) fn provision(name: &str, launch: &HostServiceLaunch) -> Result<()> {
    crate::require_administrator()?;
    // A logger whose tools are absent would only fail later, in a runsv
    // restart loop. Refuse the association before publishing anything.
    validate_logger_tools()?;
    let service_root = service_directory().context("runit is unavailable on this platform")?;
    let parent = PinnedDirectory::open_owned_hierarchy(&service_root, 0)?
        .context("runit service directory is absent")?;
    let service = match parent.open_child_directory(OsStr::new(name))? {
        Some(existing) => {
            require_published_service(&existing, name, launch)?;
            existing
        }
        None => {
            let (staging_name, staging) =
                parent.create_unique_child(".lillux-host-service-stage", 0o755)?;
            build_staged_service(&staging, name, launch)?;
            parent
                .rename_child_directory_noreplace(&staging_name, OsStr::new(name), &staging)
                .map_err(anyhow::Error::from)?;
            parent
                .open_child_directory(OsStr::new(name))?
                .context("published runit service disappeared")?
        }
    };
    publish_activation_link(name, service.path())?;
    await_and_grant_supervisor_fifos(&service, &OPERATOR_SUPERVISOR_FIFOS, &launch.account)?;
    let logger = service
        .open_child_directory(OsStr::new(LOG_SERVICE))?
        .context("published runit service has no adapter logger")?;
    logger.require_owner(0)?;
    await_and_grant_supervisor_fifos(&logger, &LOGGER_STATUS_FIFOS, &launch.account)
}

pub(super) fn discover(name: &str) -> Result<Option<HostServiceInstallation>> {
    let Some(parent) = service_directory() else {
        return Ok(None);
    };
    let Some(directory) = PinnedDirectory::open_owned_hierarchy(&parent.join(name), 0)? else {
        return Ok(None);
    };
    let records = records(&directory)?;
    let configuration = records
        .open_pinned_regular(OsStr::new(MANAGER_RECORD), false)?
        .context("configured runit integration is incomplete")?;
    configuration.require_owner(0)?;
    let configuration: RunitConfiguration = serde_json::from_slice(
        &configuration.read_stable_bounded(&configuration.observation()?, 16 * 1024)?,
    )?;
    if configuration.schema_version != RUNIT_INTEGRATION_VERSION {
        bail!("runit host integration contract is not current");
    }
    let launch_file = records
        .open_pinned_regular(OsStr::new(LAUNCH_RECORD), false)?
        .context("configured runit launch record is absent")?;
    launch_file.require_owner(0)?;
    let launch: HostServiceLaunch = serde_json::from_slice(
        &launch_file.read_stable_bounded(&launch_file.observation()?, 64 * 1024)?,
    )?;
    let _ = canonical_launch(&launch)?;
    require_root_regular(&directory, RUN_PROGRAM, &run_program(&launch)?)?;
    require_logger(&directory, name, &launch)?;
    require_activation_link(name, directory.path())?;
    if let Some(marker) = directory.open_pinned_regular(OsStr::new(DOWN_MARKER), false)? {
        marker.require_owner(0)?;
    }
    let state_directory = records
        .open_child_directory(OsStr::new(STATE_DIRECTORY))?
        .context("configured host service has no client state directory")?;
    state_directory.require_owner(0)?;
    Ok(Some(HostServiceInstallation {
        state_directory,
        launch,
        controller: Box::new(RunitService::open(
            directory,
            &configuration.control_executable,
        )?),
    }))
}

// Bounds for enumerating and removing administrator-owned service trees.
const MAX_LISTED_SERVICES: usize = 4096;
const RETIRE_REMOVAL_BUDGET: crate::DirectoryTraversalBudget = crate::DirectoryTraversalBudget {
    max_entries: 16 * 1024,
    max_depth: 16,
};
// runsvdir rescans its directory every five seconds; runsv then waits for the
// service and its logger to exit before it exits itself.
const SUPERVISOR_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const SERVICE_STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The native directories one runit host is administered through. The host
/// layout is fixed and root-owned; unit tests substitute directories owned by
/// the test account so no test ever reads or changes the real host.
struct RunitLayout {
    services: PathBuf,
    activation: PathBuf,
    logs: PathBuf,
    owner: u32,
    /// The host layout is opened only through an administrator-owned
    /// hierarchy from `/`. A fixture layout checks only its own directories.
    owned_hierarchy: bool,
    supervisor_exit_timeout: std::time::Duration,
}

impl RunitLayout {
    fn host() -> Option<Self> {
        Some(Self {
            services: service_directory()?,
            activation: activation_directory()?,
            logs: PathBuf::from(LOG_ROOT),
            owner: 0,
            owned_hierarchy: true,
            supervisor_exit_timeout: SUPERVISOR_EXIT_TIMEOUT,
        })
    }

    fn open_base(&self, path: &Path) -> Result<Option<PinnedDirectory>> {
        if self.owned_hierarchy {
            return PinnedDirectory::open_owned_hierarchy(path, self.owner);
        }
        let Some(directory) = PinnedDirectory::open(path)? else {
            return Ok(None);
        };
        directory.require_owner(self.owner)?;
        Ok(Some(directory))
    }
}

fn require_service_name(name: &str) -> Result<()> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\0', '\n', '\r']) {
        bail!("host service name must be one exact path component");
    }
    Ok(())
}

fn permission_denied(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

/// Read the recorded launch of one association without validating that its
/// executable still exists: an association whose node is gone is exactly what
/// enumeration and retirement must still be able to name.
fn read_recorded_launch(directory: &PinnedDirectory, owner: u32) -> Result<HostServiceLaunch> {
    directory.require_owner(owner)?;
    let records = directory
        .open_child_directory(OsStr::new(RECORDS))?
        .context("directory is not a Lillux host-service association")?;
    records.require_owner(owner)?;
    let file = records
        .open_pinned_regular(OsStr::new(LAUNCH_RECORD), false)?
        .context("host-service association has no launch record")?;
    file.require_owner(owner)?;
    let launch: HostServiceLaunch =
        serde_json::from_slice(&file.read_stable_bounded(&file.observation()?, 64 * 1024)?)
            .context("host-service launch record is invalid")?;
    if launch.schema_version != 1 {
        bail!("host-service launch record contract is not current");
    }
    Ok(launch)
}

fn activation_target(services: &PinnedDirectory, name: &str) -> PathBuf {
    services.path().join(name)
}

pub(super) fn list(prefix: &str) -> Result<Option<Vec<super::HostServiceAssociation>>> {
    let Some(layout) = RunitLayout::host() else {
        return Ok(None);
    };
    list_in(&layout, prefix)
}

fn list_in(
    layout: &RunitLayout,
    prefix: &str,
) -> Result<Option<Vec<super::HostServiceAssociation>>> {
    let services = match layout.open_base(&layout.services) {
        Ok(Some(services)) => services,
        Ok(None) => return Ok(None),
        Err(error) if permission_denied(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let entries = match services.entries_no_follow_bounded(MAX_LISTED_SERVICES) {
        Ok(entries) => entries,
        Err(error) if permission_denied(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let activation = match layout.open_base(&layout.activation) {
        Ok(activation) => activation,
        Err(error) if permission_denied(&error) => None,
        Err(error) => return Err(error),
    };
    let mut associations = Vec::new();
    for entry in entries {
        if entry.entry_type != PinnedEntryType::Directory {
            continue;
        }
        let Some(name) = entry.name.to_str() else {
            continue;
        };
        if !name.starts_with(prefix) || require_service_name(name).is_err() {
            continue;
        }
        let launch = services
            .open_child_directory(&entry.name)
            .and_then(|directory| {
                read_recorded_launch(
                    &directory.context("host-service directory disappeared")?,
                    layout.owner,
                )
            })
            .map_err(|error| format!("{error:#}"));
        let target = activation_target(&services, name);
        let activated = match &activation {
            Some(activation) => activation
                .read_symlink_target(&entry.name, 4096)
                .ok()
                .flatten()
                .is_some_and(|actual| actual == target.as_os_str().as_encoded_bytes()),
            None => false,
        };
        associations.push(super::HostServiceAssociation {
            name: name.to_owned(),
            launch,
            activated,
        });
    }
    Ok(Some(associations))
}

/// Whether runsv still supervises the service or its logger. runsv holds each
/// `supervise/ok` FIFO open for reading for as long as it runs.
fn supervisor_running(directory: &PinnedDirectory) -> Result<bool> {
    let mut supervised = vec![directory.try_clone()?];
    if let Some(logger) = directory.open_child_directory(OsStr::new(LOG_SERVICE))? {
        supervised.push(logger);
    }
    for service in supervised {
        let Some(supervise) = service.open_child_directory(OsStr::new(SUPERVISE_DIRECTORY))? else {
            continue;
        };
        if supervise.fifo_has_reader(OsStr::new("ok"))? == Some(true) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn await_supervisor_exit(
    directory: &PinnedDirectory,
    name: &str,
    timeout: std::time::Duration,
) -> Result<()> {
    let deadline = crate::time::MonotonicDeadline::after(timeout);
    while supervisor_running(directory)? {
        if deadline.has_elapsed() {
            bail!(
                "runit still supervises host service `{name}` after its activation link was \
                 removed; retry the retirement once runsvdir has stopped it"
            );
        }
        crate::time::sleep(crate::time::Duration::from_millis(100));
    }
    Ok(())
}

/// Bring a supervised service down through its own recorded control
/// executable: a graceful down first, then the supervisor's definitive kill.
fn stop_supervised(directory: PinnedDirectory) -> Result<()> {
    let records = records(&directory)?;
    let configuration = records
        .open_pinned_regular(OsStr::new(MANAGER_RECORD), false)?
        .context("configured runit integration is incomplete")?;
    configuration.require_owner(0)?;
    let configuration: RunitConfiguration = serde_json::from_slice(
        &configuration.read_stable_bounded(&configuration.observation()?, 16 * 1024)?,
    )?;
    let service = RunitService::open(directory, &configuration.control_executable)?;
    if service.wait_down(SERVICE_STOP_TIMEOUT).is_ok() {
        return Ok(());
    }
    service.force_down()?;
    service.wait_down(SERVICE_STOP_TIMEOUT)
}

fn remove_service_logs(layout: &RunitLayout, name: &str) -> Result<bool> {
    let Some(root) = layout.open_base(&layout.logs)? else {
        return Ok(false);
    };
    // The per-service log directory belongs to the service account; only
    // the administrator-owned base is ownership-checked.
    let Some(logs) = root.open_child_directory(OsStr::new(name))? else {
        return Ok(false);
    };
    logs.remove_contents_recursive_bounded(RETIRE_REMOVAL_BUDGET)?;
    if root
        .remove_empty_child_if_same(OsStr::new(name), &logs)?
        .is_none()
    {
        bail!("host service log directory `{name}` was not empty after removal");
    }
    Ok(true)
}

pub(super) fn retire(name: &str, expected: &HostServiceLaunch) -> Result<bool> {
    crate::require_administrator()?;
    let layout = RunitLayout::host().context("runit is unavailable on this platform")?;
    retire_in(&layout, name, expected, &stop_supervised)
}

fn retire_in(
    layout: &RunitLayout,
    name: &str,
    expected: &HostServiceLaunch,
    stop: &dyn Fn(PinnedDirectory) -> Result<()>,
) -> Result<bool> {
    require_service_name(name)?;
    let services = layout
        .open_base(&layout.services)?
        .context("runit service directory is absent")?;
    let activation = layout
        .open_base(&layout.activation)?
        .context("runit activation directory is absent")?;
    let target = activation_target(&services, name);
    let target = target.as_os_str().as_encoded_bytes();
    let Some(directory) = services.open_child_directory(OsStr::new(name))? else {
        // An interrupted retirement can leave its activation link or logs.
        // Both are named exactly by this association and nothing else.
        activation.remove_symlink_if_target(OsStr::new(name), target)?;
        remove_service_logs(layout, name)?;
        return Ok(false);
    };
    let recorded = read_recorded_launch(&directory, layout.owner)
        .with_context(|| format!("refusing to retire `{name}`"))?;
    if recorded != *expected {
        bail!(
            "host service `{name}` records a different launch than the one inspected; \
             refusing to retire it"
        );
    }
    if let Some(actual) = activation.read_symlink_target(OsStr::new(name), 4096)?
        && actual != target
    {
        bail!("runit activation link `{name}` belongs to another service; refusing to retire it");
    }
    if supervisor_running(&directory)? {
        stop(directory.try_clone()?)?;
    }
    activation.remove_symlink_if_target(OsStr::new(name), target)?;
    await_supervisor_exit(&directory, name, layout.supervisor_exit_timeout)?;
    directory.remove_contents_recursive_bounded(RETIRE_REMOVAL_BUDGET)?;
    if services
        .remove_empty_child_if_same(OsStr::new(name), &directory)?
        .is_none()
    {
        bail!("host service directory `{name}` was not empty after removal");
    }
    remove_service_logs(layout, name)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(environment: serde_json::Value) -> HostServiceLaunch {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "executable": "/usr/bin/ryeosd",
            "arguments": ["host-service", "--app-root", "/home/example/.local/share/ryeos"],
            "environment": environment,
            "account": {
                "implementation": "unix",
                "uid": 1000,
                "gid": 1000
            }
        }))
        .unwrap()
    }

    #[test]
    fn run_program_quotes_exact_launch_values_without_shell_interpretation() {
        let rendered = String::from_utf8(
            run_program(&launch(serde_json::json!({
                "HOME": "/home/example/it's-safe",
                "PATH": "/usr/bin:/bin"
            })))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            rendered,
            "#!/bin/sh\nexec 2>&1\nexport HOME='/home/example/it'\"'\"'s-safe'\nexport PATH='/usr/bin:/bin'\nexec '/usr/bin/ryeosd' 'host-service' '--app-root' '/home/example/.local/share/ryeos'\n"
        );
    }

    #[test]
    fn run_program_rejects_invalid_environment_syntax_and_control_values() {
        assert!(run_program(&launch(serde_json::json!({"BAD-NAME": "value"}))).is_err());
        assert!(run_program(&launch(serde_json::json!({"HOME": "line\nbreak"}))).is_err());
    }

    #[test]
    fn idempotent_provision_compares_the_complete_typed_launch_not_json_whitespace() {
        let expected = launch(serde_json::json!({}));
        let pretty = serde_json::to_vec_pretty(&expected).unwrap();
        assert_eq!(
            decode_matching_launch(&pretty, &expected).unwrap(),
            expected
        );

        let mut different = serde_json::to_value(&expected).unwrap();
        different["arguments"][2] = "/another/node".into();
        assert!(
            decode_matching_launch(&serde_json::to_vec(&different).unwrap(), &expected).is_err()
        );

        let mut unknown = serde_json::to_value(&expected).unwrap();
        unknown["ambient_path"] = "/usr/bin".into();
        assert!(decode_matching_launch(&serde_json::to_vec(&unknown).unwrap(), &expected).is_err());

        let mut wrong_schema = serde_json::to_value(&expected).unwrap();
        wrong_schema["schema_version"] = 0.into();
        assert!(
            decode_matching_launch(&serde_json::to_vec(&wrong_schema).unwrap(), &expected).is_err()
        );

        let mut wrong_account = serde_json::to_value(&expected).unwrap();
        wrong_account["account"]["uid"] = 1001.into();
        assert!(
            decode_matching_launch(&serde_json::to_vec(&wrong_account).unwrap(), &expected)
                .is_err()
        );

        let mut wrong_environment = serde_json::to_value(&expected).unwrap();
        wrong_environment["environment"]["PATH"] = "/ambient".into();
        assert!(
            decode_matching_launch(&serde_json::to_vec(&wrong_environment).unwrap(), &expected)
                .is_err()
        );
        assert!(decode_matching_launch(b"{", &expected).is_err());
    }

    #[test]
    fn operator_delegation_covers_runit_probe_before_control() {
        assert_eq!(OPERATOR_SUPERVISOR_FIFOS, ["ok", "control"]);
    }

    #[test]
    fn logger_delegation_is_status_probe_only() {
        assert_eq!(LOGGER_STATUS_FIFOS, ["ok"]);
        assert!(!LOGGER_STATUS_FIFOS.contains(&CONTROL));
    }

    #[test]
    fn log_program_embeds_exact_account_and_directory_without_parsing_records() {
        let rendered =
            String::from_utf8(log_program("ryeos-4b17", &launch(serde_json::json!({}))).unwrap())
                .unwrap();
        assert_eq!(
            rendered,
            "#!/bin/sh\nexec 2>&1\n\
             /usr/bin/install -d -o 0 -g 0 -m 0755 '/var/log/lillux-host-service'\n\
             /usr/bin/install -d -o 1000 -g 1000 -m 0750 '/var/log/lillux-host-service/ryeos-4b17'\n\
             exec /usr/bin/chpst -u :1000:1000 /usr/bin/svlogd -tt '/var/log/lillux-host-service/ryeos-4b17'\n"
        );

        let mut other = serde_json::to_value(launch(serde_json::json!({}))).unwrap();
        other["account"]["uid"] = 1001.into();
        other["account"]["gid"] = 1002.into();
        let other: HostServiceLaunch = serde_json::from_value(other).unwrap();
        let other = String::from_utf8(log_program("ryeos-4b17", &other).unwrap()).unwrap();
        assert!(other.contains("-o 1001 -g 1002 -m 0750"));
        assert!(other.contains("chpst -u :1001:1002 "));
        assert_ne!(other, rendered);
    }

    #[test]
    fn log_program_quotes_the_service_name_and_rejects_non_components() {
        let rendered =
            String::from_utf8(log_program("it's", &launch(serde_json::json!({}))).unwrap())
                .unwrap();
        assert!(rendered.contains("'/var/log/lillux-host-service/it'\"'\"'s'"));
        for name in ["", ".", "..", "a/b", "nul\0name", "line\nbreak"] {
            assert!(
                log_program(name, &launch(serde_json::json!({}))).is_err(),
                "{name:?}"
            );
        }
        let mut root = serde_json::to_value(launch(serde_json::json!({}))).unwrap();
        root["account"]["uid"] = 0.into();
        let root: HostServiceLaunch = serde_json::from_value(root).unwrap();
        assert!(log_program("ryeos-4b17", &root).is_err());
    }

    /// A runit layout in temp directories owned by the test account. Nothing
    /// here reads or changes the host's real service directories.
    struct Fixture {
        _tmp: tempfile::TempDir,
        layout: RunitLayout,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let layout = RunitLayout {
                services: root.join("sv"),
                activation: root.join("runsvdir"),
                logs: root.join("log"),
                owner: crate::effective_uid().unwrap(),
                owned_hierarchy: false,
                supervisor_exit_timeout: std::time::Duration::from_millis(300),
            };
            for directory in [&layout.services, &layout.activation, &layout.logs] {
                std::fs::create_dir(directory).unwrap();
                set_mode(directory, 0o755);
            }
            Self { _tmp: tmp, layout }
        }

        fn service(&self, name: &str) -> PathBuf {
            self.layout.services.join(name)
        }

        fn link(&self, name: &str) -> PathBuf {
            self.layout.activation.join(name)
        }

        fn logs(&self, name: &str) -> PathBuf {
            self.layout.logs.join(name)
        }

        /// Publish the shape `provision` leaves: records, run program, logger,
        /// supervise endpoints, activation link and a log directory.
        fn associate(&self, name: &str, launch: &HostServiceLaunch) {
            let service = self.service(name);
            let records = service.join(RECORDS);
            for directory in [
                service.clone(),
                records.clone(),
                records.join(STATE_DIRECTORY),
                service.join(LOG_SERVICE),
                service.join(SUPERVISE_DIRECTORY),
                service.join(LOG_SERVICE).join(SUPERVISE_DIRECTORY),
            ] {
                std::fs::create_dir(&directory).unwrap();
                set_mode(&directory, 0o755);
            }
            std::fs::write(
                records.join(LAUNCH_RECORD),
                serde_json::to_vec(launch).unwrap(),
            )
            .unwrap();
            set_mode(&records.join(LAUNCH_RECORD), 0o644);
            std::fs::write(records.join(MANAGER_RECORD), manager_bytes().unwrap()).unwrap();
            std::fs::write(service.join(RUN_PROGRAM), b"#!/bin/sh\n").unwrap();
            std::fs::write(service.join(DOWN_MARKER), b"").unwrap();
            for supervise in [
                service.join(SUPERVISE_DIRECTORY),
                service.join(LOG_SERVICE).join(SUPERVISE_DIRECTORY),
            ] {
                for fifo in OPERATOR_SUPERVISOR_FIFOS {
                    make_fifo(&supervise.join(fifo));
                }
            }
            std::os::unix::fs::symlink(&service, self.link(name)).unwrap();
            std::fs::create_dir(self.logs(name)).unwrap();
            std::fs::write(self.logs(name).join("current"), b"log line\n").unwrap();
        }
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn make_fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt as _;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    /// Stand in for runsv: hold the service's `supervise/ok` open for reading.
    fn hold_supervisor(fixture: &Fixture, name: &str) -> std::fs::File {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fixture.service(name).join(SUPERVISE_DIRECTORY).join("ok"))
            .unwrap()
    }

    fn never_stop(_: PinnedDirectory) -> Result<()> {
        panic!("an unsupervised association must not be stopped")
    }

    fn other_launch() -> HostServiceLaunch {
        let mut other = serde_json::to_value(launch(serde_json::json!({}))).unwrap();
        other["arguments"][2] = "/another/node".into();
        serde_json::from_value(other).unwrap()
    }

    #[test]
    fn retire_removes_link_service_and_logs() {
        let fixture = Fixture::new();
        let expected = launch(serde_json::json!({}));
        fixture.associate("ryeos-dead", &expected);

        assert!(retire_in(&fixture.layout, "ryeos-dead", &expected, &never_stop).unwrap());
        assert!(!fixture.service("ryeos-dead").exists());
        assert!(fixture.link("ryeos-dead").symlink_metadata().is_err());
        assert!(!fixture.logs("ryeos-dead").exists());

        // A repeated retirement finds nothing and changes nothing.
        assert!(!retire_in(&fixture.layout, "ryeos-dead", &expected, &never_stop).unwrap());
    }

    #[test]
    fn retire_finishes_an_interrupted_retirement() {
        let fixture = Fixture::new();
        let expected = launch(serde_json::json!({}));
        fixture.associate("ryeos-half", &expected);
        std::fs::remove_dir_all(fixture.service("ryeos-half")).unwrap();

        assert!(!retire_in(&fixture.layout, "ryeos-half", &expected, &never_stop).unwrap());
        assert!(fixture.link("ryeos-half").symlink_metadata().is_err());
        assert!(!fixture.logs("ryeos-half").exists());
    }

    #[test]
    fn retire_refuses_foreign_service() {
        let fixture = Fixture::new();
        let expected = launch(serde_json::json!({}));

        // Recorded launch differs from the inspected one.
        fixture.associate("ryeos-other", &other_launch());
        let error = retire_in(&fixture.layout, "ryeos-other", &expected, &never_stop).unwrap_err();
        assert!(
            format!("{error:#}").contains("different launch"),
            "{error:#}"
        );

        // A service directory that is not a Lillux association.
        std::fs::create_dir(fixture.service("ryeos-foreign")).unwrap();
        set_mode(&fixture.service("ryeos-foreign"), 0o755);
        std::fs::write(fixture.service("ryeos-foreign").join(RUN_PROGRAM), b"").unwrap();
        let error =
            retire_in(&fixture.layout, "ryeos-foreign", &expected, &never_stop).unwrap_err();
        assert!(
            format!("{error:#}").contains("not a Lillux host-service association"),
            "{error:#}"
        );

        // An activation link that names another service directory.
        fixture.associate("ryeos-swapped", &expected);
        std::fs::remove_file(fixture.link("ryeos-swapped")).unwrap();
        std::os::unix::fs::symlink(
            fixture.service("ryeos-other"),
            fixture.link("ryeos-swapped"),
        )
        .unwrap();
        let error =
            retire_in(&fixture.layout, "ryeos-swapped", &expected, &never_stop).unwrap_err();
        assert!(
            format!("{error:#}").contains("another service"),
            "{error:#}"
        );

        for name in ["ryeos-other", "ryeos-foreign", "ryeos-swapped"] {
            assert!(fixture.service(name).is_dir(), "{name}");
        }
        assert!(fixture.link("ryeos-other").symlink_metadata().is_ok());
        assert!(fixture.link("ryeos-swapped").symlink_metadata().is_ok());
        assert!(fixture.logs("ryeos-other").is_dir());

        for name in [
            "",
            ".",
            "..",
            ".lillux-host-service-stage1",
            "a/b",
            "line\nbreak",
        ] {
            assert!(
                retire_in(&fixture.layout, name, &expected, &never_stop).is_err(),
                "{name:?}"
            );
        }
    }

    #[test]
    fn retire_stops_a_live_supervisor_and_waits_for_it_to_exit() {
        let fixture = Fixture::new();
        let expected = launch(serde_json::json!({}));
        fixture.associate("ryeos-live", &expected);
        let supervisor = hold_supervisor(&fixture, "ryeos-live");
        let stopped = std::cell::Cell::new(0);
        let stop = |_: PinnedDirectory| -> Result<()> {
            stopped.set(stopped.get() + 1);
            Ok(())
        };

        let error = retire_in(&fixture.layout, "ryeos-live", &expected, &stop).unwrap_err();
        assert!(
            format!("{error:#}").contains("still supervises"),
            "{error:#}"
        );
        assert_eq!(stopped.get(), 1);
        // Deactivated, but nothing removed while the supervisor holds it.
        assert!(fixture.link("ryeos-live").symlink_metadata().is_err());
        assert!(fixture.service("ryeos-live").is_dir());
        assert!(fixture.logs("ryeos-live").is_dir());

        drop(supervisor);
        assert!(retire_in(&fixture.layout, "ryeos-live", &expected, &never_stop).unwrap());
        assert!(!fixture.service("ryeos-live").exists());
        assert!(!fixture.logs("ryeos-live").exists());
    }

    #[test]
    fn list_reads_recorded_app_root() {
        let fixture = Fixture::new();
        let expected = launch(serde_json::json!({}));
        fixture.associate("ryeos-a", &expected);
        fixture.associate("other-b", &other_launch());
        fixture.associate("ryeos-c", &other_launch());
        std::fs::remove_file(fixture.link("ryeos-c")).unwrap();
        std::fs::create_dir(fixture.service("ryeos-d")).unwrap();
        set_mode(&fixture.service("ryeos-d"), 0o755);
        std::fs::create_dir(fixture.service(".lillux-host-service-stage1")).unwrap();

        let listed = list_in(&fixture.layout, "ryeos-").unwrap().unwrap();
        let names: Vec<_> = listed.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["ryeos-a", "ryeos-c", "ryeos-d"]);
        assert_eq!(listed[0].launch.as_ref().unwrap(), &expected);
        assert_eq!(
            listed[0].launch.as_ref().unwrap().arguments,
            [
                "host-service",
                "--app-root",
                "/home/example/.local/share/ryeos"
            ]
        );
        assert!(listed[0].activated);
        assert_eq!(listed[1].launch.as_ref().unwrap(), &other_launch());
        assert!(!listed[1].activated);
        assert!(listed[2].launch.is_err());
        assert!(!listed[2].activated);
    }

    #[test]
    fn list_without_a_service_directory_is_unavailable() {
        let fixture = Fixture::new();
        std::fs::remove_dir(&fixture.layout.services).unwrap();
        assert!(list_in(&fixture.layout, "ryeos-").unwrap().is_none());
    }

    #[test]
    fn fifo_probe_distinguishes_a_held_reader() {
        let fixture = Fixture::new();
        fixture.associate("ryeos-probe", &launch(serde_json::json!({})));
        let directory = PinnedDirectory::open(&fixture.service("ryeos-probe"))
            .unwrap()
            .unwrap();
        assert!(!supervisor_running(&directory).unwrap());
        let supervisor = hold_supervisor(&fixture, "ryeos-probe");
        assert!(supervisor_running(&directory).unwrap());
        drop(supervisor);
        assert!(!supervisor_running(&directory).unwrap());
    }

    #[test]
    fn integration_contract_records_the_adapter_logger_generation() {
        let manager: serde_json::Value = serde_json::from_slice(&manager_bytes().unwrap()).unwrap();
        assert_eq!(manager["schema_version"], RUNIT_INTEGRATION_VERSION);
        assert_eq!(RUNIT_INTEGRATION_VERSION, 2);
    }
}
