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
            args: vec![
                operation.to_owned(),
                directory.path().to_string_lossy().into_owned(),
            ],
            cwd: None,
            envs: vec![],
            stdin_data: None,
            timeout: 5.0,
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
            schema_version: 1,
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
    let mut body = format!("#!{NATIVE_SCRIPT_INTERPRETER}\n");
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

fn ensure_root_regular(
    directory: &PinnedDirectory,
    name: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<()> {
    let Some(existing) = directory.open_pinned_regular(OsStr::new(name), false)? else {
        return directory.atomic_write_pinned_if_same(OsStr::new(name), None, bytes, mode);
    };
    existing.require_owner(0)?;
    let current = existing.read_stable_bounded(&existing.observation()?, 64 * 1024)?;
    if current != bytes {
        bail!("existing native service record `{name}` conflicts with this host association");
    }
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
    launch: &HostServiceLaunch,
) -> Result<()> {
    directory.require_owner(0)?;
    let records = records(directory)?;
    ensure_root_regular(&records, LAUNCH_RECORD, &canonical_launch(launch)?, 0o644)?;
    ensure_root_regular(&records, MANAGER_RECORD, &manager_bytes()?, 0o644)?;
    ensure_root_regular(directory, RUN_PROGRAM, &run_program(launch)?, 0o755)?;
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

fn build_staged_service(directory: &PinnedDirectory, launch: &HostServiceLaunch) -> Result<()> {
    directory.require_owner(0)?;
    let records = directory.open_or_create_child(OsStr::new(RECORDS), 0o755)?;
    records.require_owner(0)?;
    ensure_root_regular(&records, LAUNCH_RECORD, &canonical_launch(launch)?, 0o644)?;
    ensure_root_regular(&records, MANAGER_RECORD, &manager_bytes()?, 0o644)?;
    ensure_root_regular(directory, RUN_PROGRAM, &run_program(launch)?, 0o755)?;
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

fn await_and_grant_operator_fifos(
    service: &PinnedDirectory,
    account: &crate::ControllerAccount,
) -> Result<()> {
    let deadline = crate::time::MonotonicDeadline::after(crate::time::Duration::from_secs(5));
    loop {
        if let Some(supervise) = service.open_child_directory(OsStr::new("supervise"))? {
            supervise.require_owner(0)?;
            let mut ready = true;
            for name in OPERATOR_SUPERVISOR_FIFOS {
                match supervise.entry_no_follow(OsStr::new(name))? {
                    Some(entry) if entry.entry_type == PinnedEntryType::Fifo => {}
                    Some(_) => bail!("runit supervisor {name} endpoint is not a FIFO"),
                    None => ready = false,
                }
            }
            if ready {
                for name in OPERATOR_SUPERVISOR_FIFOS {
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
    let service_root = service_directory().context("runit is unavailable on this platform")?;
    let parent = PinnedDirectory::open_owned_hierarchy(&service_root, 0)?
        .context("runit service directory is absent")?;
    let service = match parent.open_child_directory(OsStr::new(name))? {
        Some(existing) => {
            require_published_service(&existing, launch)?;
            existing
        }
        None => {
            let (staging_name, staging) =
                parent.create_unique_child(".lillux-host-service-stage", 0o755)?;
            build_staged_service(&staging, launch)?;
            parent
                .rename_child_directory_noreplace(&staging_name, OsStr::new(name), &staging)
                .map_err(anyhow::Error::from)?;
            parent
                .open_child_directory(OsStr::new(name))?
                .context("published runit service disappeared")?
        }
    };
    publish_activation_link(name, service.path())?;
    await_and_grant_operator_fifos(&service, &launch.account)
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
    if configuration.schema_version != 1 {
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
            "#!/bin/sh\nexport HOME='/home/example/it'\"'\"'s-safe'\nexport PATH='/usr/bin:/bin'\nexec '/usr/bin/ryeosd' 'host-service' '--app-root' '/home/example/.local/share/ryeos'\n"
        );
    }

    #[test]
    fn run_program_rejects_invalid_environment_syntax_and_control_values() {
        assert!(run_program(&launch(serde_json::json!({"BAD-NAME": "value"}))).is_err());
        assert!(run_program(&launch(serde_json::json!({"HOME": "line\nbreak"}))).is_err());
    }

    #[test]
    fn operator_delegation_covers_runit_probe_before_control() {
        assert_eq!(OPERATOR_SUPERVISOR_FIFOS, ["ok", "control"]);
    }
}
