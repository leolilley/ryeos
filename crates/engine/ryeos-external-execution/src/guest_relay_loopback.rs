//! Local stand-in for a sandbox host's file transport.
//!
//! A controller that has composed a real activation (owner runtime, signed
//! import, guest package and node-signed assignment) hands it to a rehearsal
//! host, which starts the owner in a Lillux root enclosure exactly as the
//! guest-start rehearsal does and then stays up, moving relay files between
//! the controller and the guest's mailbox on request: a download reads the
//! exact file, an upload delivers it owned by the requested account with mode
//! `0600`, published under its name in one atomic rename. Owner stdout is
//! forwarded as it arrives so the controller can judge the owner's report the
//! way a lifecycle adapter judges a provider's run stream.

use std::ffi::OsStr;
use std::io::{BufRead as _, Read as _, Write as _};
use std::path::Path;

use anyhow::{Context as _, Result, bail, ensure};
use base64::{
    Engine as _, engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD,
};
use ryeos_external_execution_contract::guest_owner_report::{
    GuestOwnerActivationReport, GuestOwnerStdout, GuestOwnerTerminal,
};
use serde::{Deserialize, Serialize};

use crate::guest_start_rehearsal::{
    ACTIVATION_NAME, GuestStartRehearsalPackage, HostMode, HostPlan, IMPORT_NAME, MAX_STDERR_BYTES,
    PACKAGE_NAME, PLAN_NAME, POLL_INTERVAL, REHEARSAL_HOST_ENV, RUNTIME_NAME, copy_file, copy_tree,
    settle_owner, spawn_owner, write_file,
};

/// Largest relay file the loopback moves; the signed relay bound is smaller.
const MAX_RELAY_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Largest protocol line: a base64 relay file plus framing.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_OWNER_STDOUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Download {
        path: String,
    },
    Upload {
        path: String,
        bytes_base64: String,
        uid: u32,
        gid: u32,
    },
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    OwnerStdout {
        text: String,
    },
    OwnerExited {
        code: Option<i32>,
    },
    Downloaded {
        bytes_base64: String,
    },
    Absent,
    Uploaded,
    Refused {
        message: String,
    },
    Stopped {
        owner_cleanup: String,
        owner_stderr: String,
    },
    Failed {
        message: String,
    },
}

fn emit(message: &Message) {
    if let Ok(line) = serde_json::to_string(message) {
        println!("{line}");
    }
    let _ = std::io::stdout().flush();
}

/// Activation documents a controller composed, as a sandbox host receives them.
pub struct GuestRelayLoopbackRequest<'a> {
    /// The guest owner runtime the assignment's manifest names.
    pub runtime: &'a lillux::PinnedDirectory,
    pub runtime_manifest_hash: &'a str,
    /// Canonical bytes of the signed guest import.
    pub signed_import: &'a [u8],
    pub package: &'a lillux::PinnedRegularFile,
    /// Canonical bytes of the node-signed occurrence assignment.
    pub signed_assignment: &'a [u8],
    pub occurrence_id: &'a str,
    pub mailbox_root: &'a str,
    pub owner_deadline_ms: u64,
    /// Private parent for the host's work directory.
    pub work_parent: &'a Path,
    /// Executable that runs the rehearsal host entry first thing.
    pub host_executable: &'a Path,
}

/// How the host ended.
#[derive(Debug, Clone)]
pub struct GuestRelayLoopbackEnd {
    pub owner_cleanup: String,
    pub owner_stderr: String,
    pub owner_stdout: String,
    pub host_stderr: String,
}

/// A running loopback host. Dropping it without [`Self::stop`] kills it.
pub struct GuestRelayLoopbackHost {
    process: Option<lillux::subordinate_process::SubordinateProcess>,
    input: lillux::subordinate_process::SubordinateProcessInput,
    output: lillux::subordinate_process::SubordinateProcessOutput,
    error: lillux::subordinate_process::SubordinateProcessError,
    work: Option<(
        lillux::PinnedDirectory,
        std::ffi::OsString,
        lillux::PinnedDirectory,
    )>,
    owner_report: GuestOwnerStdout,
    owner_terminal: Option<std::result::Result<GuestOwnerTerminal, ()>>,
    owner_stdout: String,
    owner_exit: Option<Option<i32>>,
}

impl GuestRelayLoopbackHost {
    /// Copy the activation into a private work directory and start the host.
    pub fn start(request: &GuestRelayLoopbackRequest<'_>) -> Result<Self> {
        let parent = lillux::PinnedDirectory::open(request.work_parent)?.with_context(|| {
            format!(
                "loopback work parent {} is absent",
                request.work_parent.display()
            )
        })?;
        let (name, work) = parent.create_unique_child("guest-relay-loopback", 0o700)?;
        let prepared = (|| -> Result<()> {
            let runtime = work.create_child(OsStr::new(RUNTIME_NAME), 0o700)?;
            copy_tree(request.runtime, &runtime, &mut 64)?;
            runtime.set_mode(0o755)?;
            let activation = work.create_child(OsStr::new(ACTIVATION_NAME), 0o700)?;
            write_file(&activation, IMPORT_NAME, request.signed_import, 0o600)?;
            copy_file(&activation, PACKAGE_NAME, request.package, 0o600)?;
            let plan = HostPlan {
                mode: HostMode::RelayLoopback,
                occurrence_id: request.occurrence_id.to_owned(),
                mailbox_root: request.mailbox_root.to_owned(),
                runtime_manifest_hash: request.runtime_manifest_hash.to_owned(),
                assignment_b64: URL_SAFE_NO_PAD.encode(request.signed_assignment),
                attach_budget_ms: request.owner_deadline_ms,
                owner_deadline_ms: request.owner_deadline_ms,
                package: GuestStartRehearsalPackage {
                    framed_bytes: request.package.observation()?.size(),
                    regular_bytes: 0,
                    entries: 0,
                },
                answer_attach: None,
            };
            write_file(&work, PLAN_NAME, &serde_json::to_vec(&plan)?, 0o600)?;
            Ok(())
        })();
        let started = prepared.and_then(|()| {
            let mut process = lillux::subordinate_process::SubordinateProcess::spawn(
                lillux::subordinate_process::SubordinateProcessRequest {
                    cmd: request.host_executable.to_string_lossy().into_owned(),
                    argv0: None,
                    args: Vec::new(),
                    cwd: "/".into(),
                    envs: vec![(
                        REHEARSAL_HOST_ENV.into(),
                        work.path().to_string_lossy().into_owned(),
                    )],
                    limits: None,
                    inherited_fds: Vec::new(),
                },
            )
            .map_err(anyhow::Error::msg)?;
            let input = process.take_input().map_err(anyhow::Error::msg)?;
            let output = process.take_output().map_err(anyhow::Error::msg)?;
            let error = process.take_error().map_err(anyhow::Error::msg)?;
            Ok((process, input, output, error))
        });
        match started {
            Ok((process, input, output, error)) => Ok(Self {
                process: Some(process),
                input,
                output,
                error,
                work: Some((parent, name, work)),
                owner_report: GuestOwnerStdout::new(),
                owner_terminal: None,
                owner_stdout: String::new(),
                owner_exit: None,
            }),
            Err(error) => {
                let _ = remove_work(&parent, &name, &work);
                Err(error)
            }
        }
    }

    fn observe(&mut self, message: &Message) {
        match message {
            Message::OwnerStdout { text } => {
                if self.owner_stdout.len() + text.len() <= MAX_OWNER_STDOUT_BYTES {
                    self.owner_stdout.push_str(text);
                }
                if self.owner_terminal.is_none() {
                    match self.owner_report.push(text) {
                        Ok(Some(terminal)) => self.owner_terminal = Some(Ok(terminal)),
                        Ok(None) => {}
                        Err(_) => self.owner_terminal = Some(Err(())),
                    }
                }
            }
            Message::OwnerExited { code } => self.owner_exit = Some(*code),
            _ => {}
        }
    }

    /// The next host line, or `None` at the deadline.
    fn next(&mut self, deadline: lillux::time::MonotonicDeadline) -> Result<Option<Message>> {
        let line = match self
            .output
            .read_frame_until(b'\n', MAX_LINE_BYTES, deadline)
        {
            Ok(line) => line,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => return Ok(None),
            Err(error) => return Err(error).context("read the loopback host"),
        };
        ensure!(
            !line.is_empty(),
            "the loopback host closed its output: {}",
            self.stderr_now()
        );
        let text = std::str::from_utf8(&line)?.trim_end();
        let message = serde_json::from_str::<Message>(text)
            .with_context(|| format!("the loopback host reported {text}"))?;
        self.observe(&message);
        Ok(Some(message))
    }

    fn stderr_now(&mut self) -> String {
        self.error
            .drain_until(
                MAX_STDERR_BYTES,
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(200)),
            )
            .map(|capture| String::from_utf8_lossy(&capture.bytes).into_owned())
            .unwrap_or_default()
    }

    /// Wait for the owner's terminal report line, as an adapter reads the
    /// run stream of a provider activation.
    pub fn await_owner_report(
        &mut self,
        deadline: lillux::time::MonotonicDeadline,
    ) -> GuestOwnerActivationReport {
        loop {
            let last_phase = self.owner_report.last_phase();
            match self.owner_terminal {
                Some(Ok(GuestOwnerTerminal::Held)) => {
                    return GuestOwnerActivationReport::Held {
                        last_phase,
                        supervisor_exit: None,
                    };
                }
                Some(Ok(GuestOwnerTerminal::Refused(step))) => {
                    return GuestOwnerActivationReport::Refused {
                        step,
                        last_phase,
                        detail: self.owner_report.refusal_detail().map(str::to_owned),
                    };
                }
                Some(Err(())) => return GuestOwnerActivationReport::MalformedReport { last_phase },
                None => {}
            }
            if self.owner_exit.is_some() {
                return GuestOwnerActivationReport::ExitedBeforeReport { last_phase };
            }
            match self.next(deadline) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return GuestOwnerActivationReport::NoReportBeforeDeadline { last_phase };
                }
                Err(_) => return GuestOwnerActivationReport::StreamLost { last_phase },
            }
        }
    }

    fn call(
        &mut self,
        request: &Request,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Message> {
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        self.input
            .write_all_until(&line, deadline)
            .context("write the loopback host")?;
        loop {
            match self.next(deadline)? {
                None => bail!("the loopback host did not answer before the deadline"),
                Some(Message::OwnerStdout { .. } | Message::OwnerExited { .. }) => {}
                Some(Message::Refused { message } | Message::Failed { message }) => bail!(message),
                Some(reply) => return Ok(reply),
            }
        }
    }

    /// Read one exact mailbox file; `None` when it is absent.
    pub fn download(
        &mut self,
        path: &str,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<Option<Vec<u8>>> {
        match self.call(&Request::Download { path: path.into() }, deadline)? {
            Message::Downloaded { bytes_base64 } => Ok(Some(STANDARD.decode(bytes_base64)?)),
            Message::Absent => Ok(None),
            other => bail!("the loopback host answered a download with {other:?}"),
        }
    }

    /// Deliver one new mailbox file owned by `uid`/`gid` with mode `0600`.
    pub fn upload(
        &mut self,
        path: &str,
        bytes: &[u8],
        uid: u32,
        gid: u32,
        deadline: lillux::time::MonotonicDeadline,
    ) -> Result<()> {
        let request = Request::Upload {
            path: path.into(),
            bytes_base64: STANDARD.encode(bytes),
            uid,
            gid,
        };
        match self.call(&request, deadline)? {
            Message::Uploaded => Ok(()),
            other => bail!("the loopback host answered an upload with {other:?}"),
        }
    }

    /// Forward any owner output that has arrived.
    pub fn poll(&mut self) {
        while let Ok(Some(_)) = self.next(lillux::time::MonotonicDeadline::after(
            lillux::time::Duration::from_millis(1),
        )) {}
    }

    pub fn owner_stdout(&self) -> &str {
        &self.owner_stdout
    }

    pub fn owner_exited(&self) -> Option<Option<i32>> {
        self.owner_exit
    }

    /// Whether the owner reported that it released the supervisor. An owner
    /// that exits after that has said nothing about the supervisor's absence.
    pub fn owner_released_supervisor(&self) -> bool {
        matches!(self.owner_terminal, Some(Ok(GuestOwnerTerminal::Held)))
    }

    /// Ask the owner to cancel as a lost controller would, then end the host.
    pub fn stop(mut self) -> Result<GuestRelayLoopbackEnd> {
        let deadline =
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(90));
        let result = self.call(&Request::Stop, deadline);
        let mut end = GuestRelayLoopbackEnd {
            owner_cleanup: String::new(),
            owner_stderr: String::new(),
            owner_stdout: self.owner_stdout.clone(),
            host_stderr: String::new(),
        };
        match result {
            Ok(Message::Stopped {
                owner_cleanup,
                owner_stderr,
            }) => {
                end.owner_cleanup = owner_cleanup;
                end.owner_stderr = owner_stderr;
            }
            Ok(other) => end.owner_cleanup = format!("unexpected stop answer {other:?}"),
            Err(error) => end.owner_cleanup = format!("stop failed: {error:#}"),
        }
        end.host_stderr = self.stderr_now();
        self.reap();
        Ok(end)
    }

    fn reap(&mut self) {
        if let Some(mut process) = self.process.take() {
            let deadline =
                lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10));
            if !matches!(process.wait_exact_child_until(deadline), Ok(Some(_))) {
                let _ = process.kill_exact_child_until(lillux::time::MonotonicDeadline::after(
                    lillux::time::Duration::from_secs(5),
                ));
            }
        }
        if let Some((parent, name, work)) = self.work.take() {
            let _ = remove_work(&parent, &name, &work);
        }
    }
}

impl Drop for GuestRelayLoopbackHost {
    fn drop(&mut self) {
        if let Some(process) = self.process.as_mut() {
            let _ = process.kill_exact_child_until(lillux::time::MonotonicDeadline::after(
                lillux::time::Duration::from_secs(5),
            ));
        }
        self.reap();
    }
}

fn remove_work(
    parent: &lillux::PinnedDirectory,
    name: &OsStr,
    work: &lillux::PinnedDirectory,
) -> Result<()> {
    work.remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget::new(1_000_000, 64))?;
    parent.remove_empty_child_if_same(name, work)?;
    Ok(())
}

enum Input {
    Owner(String),
    /// The owner's stdout reached end of file: every report line is forwarded.
    OwnerClosed,
    Request(String),
    Closed,
}

/// How long after the owner exits its stdout may stay open (a descendant
/// holding the pipe) before the exit is reported without waiting for it.
const OWNER_STDOUT_CLOSE_GRACE: lillux::time::Duration = lillux::time::Duration::from_secs(1);

/// Enclosed host body: start the owner over the laid-out `/ryeos` and serve
/// requests from stdin until a stop or the controller closing it.
pub(crate) fn serve_enclosed(plan: &HostPlan, laid_out: Result<()>) {
    if let Err(error) = laid_out.and_then(|()| serve(plan)) {
        emit(&Message::Failed {
            message: format!("{error:#}"),
        });
    }
}

fn serve(plan: &HostPlan) -> Result<()> {
    let mut owner = spawn_owner(plan)?;
    let mut stdout = owner.take_output().map_err(anyhow::Error::msg)?;
    let mut stderr = owner.take_error().map_err(anyhow::Error::msg)?;
    let (sender, receiver) = std::sync::mpsc::channel::<Input>();
    let owner_sender = sender.clone();
    std::thread::Builder::new()
        .name("loopback-owner-stdout".into())
        .spawn(move || {
            let mut buffer = [0_u8; 4096];
            while let Ok(read) = stdout.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
                if owner_sender.send(Input::Owner(text)).is_err() {
                    return;
                }
            }
            let _ = owner_sender.send(Input::OwnerClosed);
        })?;
    std::thread::Builder::new()
        .name("loopback-requests".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if sender.send(Input::Request(line)).is_err() {
                    return;
                }
            }
            let _ = sender.send(Input::Closed);
        })?;
    let mut captured = Vec::new();
    let mut drain = |captured: &mut Vec<u8>, slice: lillux::time::Duration| {
        if let Ok(diagnostics) = stderr.drain_until(
            MAX_STDERR_BYTES.saturating_sub(captured.len()),
            lillux::time::MonotonicDeadline::after(slice),
        ) {
            captured.extend_from_slice(&diagnostics.bytes);
        }
    };
    let mut exited = false;
    let mut stdout_closed = false;
    // The owner's exit, held back until its stdout is fully forwarded so a
    // terminal report line is never overtaken by the exit that follows it.
    let mut exit_observed = None;
    loop {
        match receiver.recv_timeout(std::time::Duration::from_millis(5)) {
            Ok(Input::Owner(text)) => emit(&Message::OwnerStdout { text }),
            Ok(Input::OwnerClosed) => stdout_closed = true,
            Ok(Input::Request(line)) => match serde_json::from_str::<Request>(&line) {
                Ok(Request::Stop) => break,
                Ok(request) => emit(&match handle(request) {
                    Ok(reply) => reply,
                    Err(error) => Message::Refused {
                        message: format!("{error:#}"),
                    },
                }),
                Err(error) => emit(&Message::Refused {
                    message: format!("malformed loopback request: {error}"),
                }),
            },
            Ok(Input::Closed) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        drain(&mut captured, POLL_INTERVAL);
        if !exited
            && exit_observed.is_none()
            && let Some(exit) = owner.try_exit().map_err(anyhow::Error::msg)?
        {
            drain(&mut captured, lillux::time::Duration::from_millis(200));
            exit_observed = Some((
                exit.code,
                lillux::time::MonotonicDeadline::after(OWNER_STDOUT_CLOSE_GRACE),
            ));
        }
        if !exited
            && let Some((code, grace)) = &exit_observed
            && (stdout_closed || grace.has_elapsed())
        {
            exited = true;
            emit(&Message::OwnerExited { code: *code });
        }
    }
    while let Ok(input) = receiver.try_recv() {
        if let Input::Owner(text) = input {
            emit(&Message::OwnerStdout { text });
        }
    }
    let owner_cleanup = if exited {
        "the owner had already exited".to_owned()
    } else {
        settle_owner(&mut owner, &mut |slice| drain(&mut captured, slice))
    };
    emit(&Message::Stopped {
        owner_cleanup,
        owner_stderr: String::from_utf8_lossy(&captured).into_owned(),
    });
    Ok(())
}

fn split(path: &str) -> Result<(lillux::PinnedDirectory, &OsStr)> {
    let path = Path::new(path);
    ensure!(path.is_absolute(), "loopback relay path is not absolute");
    let parent = path.parent().context("loopback relay path has no parent")?;
    let name = path
        .file_name()
        .context("loopback relay path has no name")?;
    let directory = lillux::PinnedDirectory::open(parent)?
        .with_context(|| format!("loopback mailbox {} is absent", parent.display()))?;
    Ok((directory, name))
}

/// Deliver one new mailbox file owned by `uid`/`gid` with mode `0600`. Owner
/// and mode are set before the name appears, so the guest never observes a
/// file it cannot read.
pub(crate) fn deliver_owned(path: &str, bytes: &[u8], uid: u32, gid: u32) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_RELAY_FILE_BYTES,
        "loopback relay upload is empty or too large"
    );
    let (directory, name) = split(path)?;
    ensure!(
        directory.open_pinned_regular(name, false)?.is_none(),
        "loopback relay slot {path} already holds a file"
    );
    let staging = format!(
        ".loopback-delivery-{}",
        hex::encode(lillux::crypto::generate_random_bytes::<8>())
    );
    let mut file = directory.open_regular_create(OsStr::new(&staging), true, true, 0o600)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let staged = directory
        .open_pinned_regular(OsStr::new(&staging), false)?
        .context("loopback staged upload vanished")?;
    lillux::ControllerAccount::unix(uid, gid).grant_private_file(&staged)?;
    directory
        .rename_regular_child_noreplace_atomic(
            OsStr::new(&staging),
            name,
            &staged.try_clone_descriptor()?,
        )
        .map_err(anyhow::Error::from)?;
    Ok(())
}

fn handle(request: Request) -> Result<Message> {
    match request {
        Request::Download { path } => {
            let (directory, name) = split(&path)?;
            let Some(file) = directory.open_pinned_regular(name, false)? else {
                return Ok(Message::Absent);
            };
            let bytes = file
                .read_stable_bounded(&file.observation()?, MAX_RELAY_FILE_BYTES)
                .map_err(anyhow::Error::msg)?;
            Ok(Message::Downloaded {
                bytes_base64: STANDARD.encode(bytes),
            })
        }
        Request::Upload {
            path,
            bytes_base64,
            uid,
            gid,
        } => {
            deliver_owned(&path, &STANDARD.decode(bytes_base64)?, uid, gid)?;
            Ok(Message::Uploaded)
        }
        Request::Stop => bail!("stop is handled by the serve loop"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_lines_are_tagged() {
        let line = serde_json::to_string(&Request::Download { path: "/x".into() }).unwrap();
        assert_eq!(line, r#"{"op":"download","path":"/x"}"#);
        let line = serde_json::to_string(&Message::Absent).unwrap();
        assert_eq!(line, r#"{"kind":"absent"}"#);
    }
}
