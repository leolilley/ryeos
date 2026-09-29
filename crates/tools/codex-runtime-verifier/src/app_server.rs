//! Verifier-owned, bounded Codex app-server observation pipe.
//!
//! This collector records native frames from the exact child. It does not
//! turn a relay report or scripted peer log into qualification testimony.

use anyhow::{Context as _, Result, ensure};
use lillux::subordinate_process::{
    PinnedSubordinateProcessRequest, SubordinateDiagnosticDrain, SubordinateDiagnosticDrainEnd,
    SubordinateProcess, SubordinateProcessExit, SubordinateProcessInput, SubordinateProcessOutput,
};
use lillux::time::MonotonicDeadline;
use serde_json::{Value, json};
use std::fmt;

use crate::routing_observation::{MAX_EVENT_BYTES, MAX_EVENTS, MAX_TOTAL_BYTES};

#[must_use = "retain the app-server owner until stop proves exact-child and reader settlement"]
pub struct AppServerObservation {
    child: SubordinateProcess,
    input: Option<SubordinateProcessInput>,
    output: SubordinateProcessOutput,
    diagnostics: SubordinateDiagnosticDrain,
    notifications: Vec<Value>,
    frames: usize,
    bytes: usize,
    deadline: MonotonicDeadline,
    scripted_phase: ScriptedPhase,
    force_attempted: bool,
}

enum ScriptedPhase {
    Fresh,
    Initialized,
    ThreadStarted(String),
    TurnStarted {
        thread_id: String,
        turn_id: String,
        notification_cursor: usize,
    },
    TurnCompleted,
    Failed,
}

pub struct ScriptedThreadIdentity {
    pub thread_id: String,
}

/// Exact direct-child outcome only. A clean exit does not prove that Codex's
/// command-environment descendants or workspace writers are gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppServerStopOutcome {
    Exited(SubordinateProcessExit),
    Forced(SubordinateProcessExit),
}

/// Startup failure after a child is born retains its exact process owner.
/// A caller must settle this value or keep it under its enclosing process
/// authority; converting it to a plain error would lose cleanup ownership.
#[must_use = "retain and settle a born app-server child after startup failure"]
pub enum AppServerLaunchFailure {
    BeforeSpawn(anyhow::Error),
    AfterSpawn {
        error: anyhow::Error,
        child: SubordinateProcess,
    },
}

impl fmt::Debug for AppServerLaunchFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeSpawn(error) => formatter.debug_tuple("BeforeSpawn").field(error).finish(),
            Self::AfterSpawn { error, .. } => formatter
                .debug_struct("AfterSpawn")
                .field("error", error)
                .field("child", &"retained")
                .finish(),
        }
    }
}

impl AppServerLaunchFailure {
    /// Exact-child settlement only. Timeout or failure returns the unchanged
    /// ownership obligation to the caller; descendant containment remains
    /// with the enclosing RyeOS process scope.
    pub fn settle(self, deadline: MonotonicDeadline) -> std::result::Result<anyhow::Error, Self> {
        match self {
            Self::BeforeSpawn(error) => Ok(error),
            Self::AfterSpawn { error, mut child } => match child.kill_exact_child_until(deadline) {
                Ok(Some(_)) => Ok(error),
                Ok(None) | Err(_) => Err(Self::AfterSpawn { error, child }),
            },
        }
    }
}

impl AppServerObservation {
    /// Launch one exact parent app-server child from a pinned executable and
    /// cwd. This owns only the direct child and its pipes; RyeOS's enclosing
    /// execution authority must still settle any descendants.
    pub fn launch(
        request: PinnedSubordinateProcessRequest,
        deadline: MonotonicDeadline,
    ) -> std::result::Result<Self, AppServerLaunchFailure> {
        let child = SubordinateProcess::spawn_pinned(request)
            .map_err(anyhow::Error::msg)
            .context("start pinned app-server child")
            .map_err(AppServerLaunchFailure::BeforeSpawn)?;
        Self::from_spawned_child(child, deadline)
    }

    fn from_spawned_child(
        mut child: SubordinateProcess,
        deadline: MonotonicDeadline,
    ) -> std::result::Result<Self, AppServerLaunchFailure> {
        let streams = (|| -> Result<_> {
            let input = child.take_input().map_err(anyhow::Error::msg)?;
            let output = child.take_output().map_err(anyhow::Error::msg)?;
            let diagnostics = child
                .take_error()
                .map_err(anyhow::Error::msg)?
                .start_discarding()?;
            Ok((input, output, diagnostics))
        })();
        match streams {
            Ok((input, output, diagnostics)) => {
                Ok(Self::new(child, input, output, diagnostics, deadline))
            }
            Err(error) => Err(AppServerLaunchFailure::AfterSpawn {
                error: error.context("app-server startup stream acquisition failed"),
                child,
            }),
        }
    }

    pub fn new(
        child: SubordinateProcess,
        input: SubordinateProcessInput,
        output: SubordinateProcessOutput,
        diagnostics: SubordinateDiagnosticDrain,
        deadline: MonotonicDeadline,
    ) -> Self {
        Self {
            child,
            input: Some(input),
            output,
            diagnostics,
            notifications: Vec::new(),
            frames: 0,
            bytes: 0,
            deadline,
            scripted_phase: ScriptedPhase::Fresh,
            force_attempted: false,
        }
    }

    pub fn notifications(&self) -> &[Value] {
        &self.notifications
    }

    pub fn send(&mut self, message: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&message)?;
        bytes.push(b'\n');
        ensure!(bytes.len() <= MAX_EVENT_BYTES, "request frame bound");
        self.input
            .as_mut()
            .context("closed app-server input")?
            .write_all_until(&bytes, self.deadline)?;
        Ok(())
    }

    pub fn next(&mut self) -> Result<Value> {
        ensure!(
            self.frames < MAX_EVENTS + 16,
            "app-server frame count bound"
        );
        let frame = self
            .output
            .read_frame_until(b'\n', MAX_EVENT_BYTES, self.deadline)?;
        ensure!(
            frame.last() == Some(&b'\n'),
            "app-server ended mid-protocol"
        );
        self.frames += 1;
        self.bytes = self
            .bytes
            .checked_add(frame.len())
            .context("frame accounting overflow")?;
        ensure!(self.bytes <= MAX_TOTAL_BYTES, "app-server total byte bound");
        let message: Value = serde_json::from_slice(&frame)?;
        ensure!(message.is_object(), "non-object app-server message");
        if message.get("id").is_none() {
            ensure!(
                message["method"].is_string() && message["params"].is_object(),
                "malformed notification"
            );
            self.notifications.push(message.clone());
        } else {
            ensure!(
                message.get("method").is_none(),
                "unexpected app-server permission/tool request"
            );
        }
        Ok(message)
    }

    pub fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        self.send(json!({"id":id,"method":method,"params":params}))?;
        loop {
            let message = self.next()?;
            if message.get("id").is_some() {
                ensure!(
                    message["id"] == id && message.get("error").is_none(),
                    "app-server request refused or response miscorrelated"
                );
                return message
                    .get("result")
                    .cloned()
                    .context("response result absent");
            }
        }
    }

    /// Exact Codex 0.147 app-server opt-in required for raw completion
    /// notifications. This does not start a model turn or contact a provider.
    pub fn initialize_scripted(&mut self) -> Result<()> {
        ensure!(
            matches!(&self.scripted_phase, ScriptedPhase::Fresh),
            "scripted app-server initialization is out of order"
        );
        self.scripted_phase = ScriptedPhase::Failed;
        self.request(
            1,
            "initialize",
            json!({"clientInfo":{"name":"ryeos-routed-verifier","version":"1"},
                "capabilities":{"experimentalApi":true}}),
        )?;
        self.send(json!({"method":"initialized","params":{}}))?;
        self.scripted_phase = ScriptedPhase::Initialized;
        Ok(())
    }

    /// Start only the exact external command environment; the local fallback
    /// remains disabled by the checked Codex home configuration.
    pub fn start_scripted_thread(&mut self) -> Result<ScriptedThreadIdentity> {
        ensure!(
            matches!(&self.scripted_phase, ScriptedPhase::Initialized),
            "scripted thread start is out of order"
        );
        self.scripted_phase = ScriptedPhase::Failed;
        let response = self.request(
            2,
            "thread/start",
            json!({"cwd":"/workspace","experimentalRawEvents":true,
                "environments":[{"environmentId":"ryeos-external-candidate",
                    "cwd":"/workspace","runtimeWorkspaceRoots":["/workspace"]}]}),
        )?;
        let thread_id = response["thread"]["id"]
            .as_str()
            .context("scripted thread ID absent")?;
        ensure!(
            (1..=256).contains(&thread_id.len()) && !thread_id.chars().any(char::is_control),
            "scripted thread ID invalid"
        );
        self.scripted_phase = ScriptedPhase::ThreadStarted(thread_id.to_owned());
        Ok(ScriptedThreadIdentity {
            thread_id: thread_id.to_owned(),
        })
    }

    /// A caller must first bind and retain the exact local scripted provider
    /// peer. This request can cause provider contact. The bounded scenario
    /// driver issues it to gather evidence, but its result cannot qualify the
    /// runtime without an independent collector and settlement witness.
    pub fn start_scripted_turn(&mut self, thread: &ScriptedThreadIdentity) -> Result<String> {
        ensure!(
            matches!(&self.scripted_phase, ScriptedPhase::ThreadStarted(expected) if expected == &thread.thread_id),
            "scripted turn start is out of order or changed thread identity"
        );
        self.scripted_phase = ScriptedPhase::Failed;
        ensure!(
            !self
                .notifications
                .iter()
                .any(|notification| notification["method"] == "turn/completed"),
            "scripted turn completed before its start request"
        );
        let notification_cursor = self.notifications.len();
        let response = self.request(
            3,
            "turn/start",
            json!({"threadId":thread.thread_id,
                "input":[{"type":"text",
                    "text":"Perform the bounded scripted routing scenario.",
                    "text_elements":[]}]}),
        )?;
        let turn_id = response["turn"]["id"]
            .as_str()
            .context("scripted turn ID absent")?;
        ensure!(
            (1..=256).contains(&turn_id.len()) && !turn_id.chars().any(char::is_control),
            "scripted turn ID invalid"
        );
        self.scripted_phase = ScriptedPhase::TurnStarted {
            thread_id: thread.thread_id.clone(),
            turn_id: turn_id.to_owned(),
            notification_cursor,
        };
        Ok(turn_id.to_owned())
    }

    /// Observe a matching terminal turn notification under the collector's
    /// existing absolute deadline and frame/byte bounds. `turn/completed` may
    /// have arrived while the turn-start response was pending. Pipe read order
    /// cannot prove when Codex emitted a buffered notification relative to
    /// our write on the other pipe: this is only a protocol milestone, not
    /// causal execution, process or writer-settlement evidence. Qualification
    /// must cross-check the provider and guest observations independently.
    pub fn await_scripted_turn_completed(
        &mut self,
        thread: &ScriptedThreadIdentity,
        turn_id: &str,
    ) -> Result<()> {
        let notification_cursor = match &self.scripted_phase {
            ScriptedPhase::TurnStarted {
                thread_id,
                turn_id: expected,
                notification_cursor,
            } if thread_id == &thread.thread_id && expected == turn_id => *notification_cursor,
            _ => anyhow::bail!("scripted turn completion is out of order or changed identity"),
        };
        self.scripted_phase = ScriptedPhase::Failed;
        let mut checked = notification_cursor;
        loop {
            while checked < self.notifications.len() {
                let notification = &self.notifications[checked];
                checked += 1;
                if notification["method"] == "turn/completed" {
                    ensure!(
                        notification["params"]["threadId"] == thread.thread_id
                            && notification["params"]["turn"]["id"] == turn_id
                            && notification["params"]["turn"]["status"] == "completed",
                        "scripted terminal turn differs from started identity or status"
                    );
                    self.scripted_phase = ScriptedPhase::TurnCompleted;
                    return Ok(());
                }
            }
            ensure!(
                self.next()?.get("id").is_none(),
                "unexpected app-server response while awaiting turn completion"
            );
        }
    }

    /// Retry on uncertainty while retaining `self`; a failing stop is not
    /// terminal evidence and dropping the owner may invoke blocking cleanup.
    pub fn stop(&mut self) -> Result<AppServerStopOutcome> {
        self.stop_until(
            MonotonicDeadline::after(lillux::time::Duration::from_secs(10)),
            MonotonicDeadline::after(lillux::time::Duration::from_secs(15)),
        )
    }

    /// An expired graceful deadline may forcefully kill the exact child. That
    /// outcome remains explicit and cannot be used as successful turn proof.
    /// Failure retains `self` so the caller can retry or invoke its enclosing
    /// scope owner; it does not authorize dropping process authority.
    pub fn stop_until(
        &mut self,
        graceful_deadline: MonotonicDeadline,
        force_deadline: MonotonicDeadline,
    ) -> Result<AppServerStopOutcome> {
        self.input = None;
        let child_settled = (|| -> Result<AppServerStopOutcome> {
            if let Some(exit) = self
                .child
                .wait_exact_child_until(graceful_deadline)
                .map_err(anyhow::Error::msg)?
            {
                return Ok(if self.force_attempted {
                    AppServerStopOutcome::Forced(exit)
                } else {
                    AppServerStopOutcome::Exited(exit)
                });
            }
            // Record the attempt before a fallible signal/reap operation so a
            // later retry can never relabel an uncertain kill as clean exit.
            self.force_attempted = true;
            let exit = self
                .child
                .kill_exact_child_until(force_deadline)
                .map_err(anyhow::Error::msg)?
                .context("app-server exact-child settlement uncertain")?;
            Ok(AppServerStopOutcome::Forced(exit))
        })();
        let reader_settled = self.diagnostics.cancel_until(MonotonicDeadline::after(
            lillux::time::Duration::from_secs(5),
        ));
        let reader_ok = matches!(
            reader_settled,
            Some(SubordinateDiagnosticDrainEnd::Eof | SubordinateDiagnosticDrainEnd::Cancelled)
        );
        match (child_settled, reader_ok) {
            (Ok(outcome), true) => Ok(outcome),
            (Ok(_), false) => anyhow::bail!("diagnostic reader join failed or remains uncertain"),
            (Err(error), true) => Err(error),
            (Err(error), false) => {
                Err(error.context("diagnostic reader join also failed or remains uncertain"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned_shell() -> lillux::PinnedRegularFile {
        let shell = std::fs::canonicalize("/bin/sh").unwrap();
        let directory = lillux::PinnedDirectory::open(shell.parent().unwrap())
            .unwrap()
            .unwrap();
        directory
            .open_pinned_regular(shell.file_name().unwrap(), false)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn pinned_launch_owns_request_pipe_and_exact_child_settlement() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let executable = pinned_shell();
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(5));
        let mut app = AppServerObservation::launch(
            PinnedSubordinateProcessRequest {
                executable,
                cwd,
                argv0: Some("sh".into()),
                args: vec![
                    "-c".into(),
                    "IFS= read -r line; printf '{\"id\":1,\"result\":{\"ok\":true}}\\n'".into(),
                ],
                envs: Vec::new(),
                limits: None,
                inherited_fds: Vec::new(),
            },
            deadline,
        )
        .unwrap();
        assert_eq!(
            app.request(1, "initialize", json!({})).unwrap(),
            json!({"ok":true})
        );
        assert!(matches!(
            app.stop().unwrap(),
            AppServerStopOutcome::Exited(SubordinateProcessExit { success: true, .. })
        ));
    }

    #[test]
    fn partial_startup_retains_born_child_until_explicit_settlement() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let executable = pinned_shell();
        let mut child = SubordinateProcess::spawn_pinned(PinnedSubordinateProcessRequest {
            executable,
            cwd,
            argv0: Some("sh".into()),
            args: vec!["-c".into(), "IFS= read -r line".into()],
            envs: Vec::new(),
            limits: None,
            inherited_fds: Vec::new(),
        })
        .unwrap();
        // Simulate a pipe already claimed during startup: the assembly path
        // must return the still-owned exact child, not a plain error.
        let input = child.take_input().unwrap();
        let failure = match AppServerObservation::from_spawned_child(
            child,
            MonotonicDeadline::after(lillux::time::Duration::from_secs(2)),
        ) {
            Ok(_) => panic!("partially claimed input was accepted"),
            Err(failure) => failure,
        };
        assert!(matches!(failure, AppServerLaunchFailure::AfterSpawn { .. }));
        drop(input);
        let error = failure
            .settle(MonotonicDeadline::after(lillux::time::Duration::from_secs(
                2,
            )))
            .unwrap();
        assert!(format!("{error:#}").contains("input is absent or already taken"));
    }

    #[test]
    fn scripted_app_server_handshake_is_finite_and_identity_bound() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let executable = pinned_shell();
        let script = r#"IFS= read -r request
printf '%s\n' "$request" >> requests
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r initialized
printf '%s\n' "$initialized" >> requests
IFS= read -r request
printf '%s\n' "$request" >> requests
printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-fixture"}}}'
IFS= read -r request
printf '%s\n' "$request" >> requests
printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-fixture"}}}'
printf '%s\n' '{"method":"turn/completed","params":{"threadId":"thread-fixture","turn":{"id":"turn-fixture","status":"completed"}}}'
"#;
        let mut app = AppServerObservation::launch(
            PinnedSubordinateProcessRequest {
                executable,
                cwd,
                argv0: Some("sh".into()),
                args: vec!["-c".into(), script.into()],
                envs: Vec::new(),
                limits: None,
                inherited_fds: Vec::new(),
            },
            MonotonicDeadline::after(lillux::time::Duration::from_secs(5)),
        )
        .unwrap();
        assert!(app.start_scripted_thread().is_err());
        app.initialize_scripted().unwrap();
        assert!(app.initialize_scripted().is_err());
        let thread = app.start_scripted_thread().unwrap();
        assert_eq!(thread.thread_id, "thread-fixture");
        let turn = app.start_scripted_turn(&thread).unwrap();
        assert_eq!(turn, "turn-fixture");
        app.await_scripted_turn_completed(&thread, &turn).unwrap();
        assert_eq!(
            app.notifications().last().unwrap()["method"],
            "turn/completed"
        );
        assert!(app.await_scripted_turn_completed(&thread, &turn).is_err());
        assert!(app.start_scripted_turn(&thread).is_err());
        assert!(matches!(
            app.stop().unwrap(),
            AppServerStopOutcome::Exited(SubordinateProcessExit { success: true, .. })
        ));
        let requests = std::fs::read_to_string(temp.path().join("requests")).unwrap();
        let requests: Vec<Value> = requests
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            requests,
            vec![
                json!({"id":1,"method":"initialize","params":{
                    "clientInfo":{"name":"ryeos-routed-verifier","version":"1"},
                    "capabilities":{"experimentalApi":true}
                }}),
                json!({"method":"initialized","params":{}}),
                json!({"id":2,"method":"thread/start","params":{
                    "cwd":"/workspace","experimentalRawEvents":true,
                    "environments":[{"environmentId":"ryeos-external-candidate",
                        "cwd":"/workspace","runtimeWorkspaceRoots":["/workspace"]}]
                }}),
                json!({"id":3,"method":"turn/start","params":{
                    "threadId":"thread-fixture",
                    "input":[{"type":"text",
                        "text":"Perform the bounded scripted routing scenario.",
                        "text_elements":[]}]
                }}),
            ]
        );
    }

    #[test]
    fn scripted_terminal_can_arrive_before_turn_start_response_but_must_match() {
        for (notification, accepted, before_turn_start) in [
            (
                r#"{"method":"turn/completed","params":{"threadId":"thread-fixture","turn":{"id":"turn-fixture","status":"completed"}}}"#,
                true,
                false,
            ),
            (
                r#"{"method":"turn/completed","params":{"threadId":"other-thread","turn":{"id":"turn-fixture","status":"completed"}}}"#,
                false,
                false,
            ),
            (
                r#"{"method":"turn/completed","params":{"threadId":"thread-fixture","turn":{"id":"turn-fixture","status":"failed"}}}"#,
                false,
                false,
            ),
            (
                r#"{"method":"turn/completed","params":{"threadId":"thread-fixture","turn":{"id":"turn-fixture","status":"completed"}}}"#,
                false,
                true,
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let cwd = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
            let prefix = [
                "IFS= read -r request",
                "printf '%s\\n' '{\"id\":1,\"result\":{}}'",
                "IFS= read -r initialized",
                "IFS= read -r request",
            ];
            let thread_response =
                "printf '%s\\n' '{\"id\":2,\"result\":{\"thread\":{\"id\":\"thread-fixture\"}}}'";
            let turn_response =
                "printf '%s\\n' '{\"id\":3,\"result\":{\"turn\":{\"id\":\"turn-fixture\"}}}'";
            let terminal = format!("printf '%s\\n' '{notification}'");
            let mut lines = prefix
                .iter()
                .map(|line| (*line).to_owned())
                .collect::<Vec<_>>();
            if before_turn_start {
                lines.extend([terminal, thread_response.to_owned()]);
            } else {
                lines.extend([
                    thread_response.to_owned(),
                    "IFS= read -r request".into(),
                    terminal,
                    turn_response.to_owned(),
                ]);
            }
            let script = lines.join("\n");
            let mut app = AppServerObservation::launch(
                PinnedSubordinateProcessRequest {
                    executable: pinned_shell(),
                    cwd,
                    argv0: Some("sh".into()),
                    args: vec!["-c".into(), script],
                    envs: Vec::new(),
                    limits: None,
                    inherited_fds: Vec::new(),
                },
                MonotonicDeadline::after(lillux::time::Duration::from_secs(5)),
            )
            .unwrap();
            app.initialize_scripted().unwrap();
            let thread = app.start_scripted_thread().unwrap();
            if before_turn_start {
                assert!(app.start_scripted_turn(&thread).is_err());
            } else {
                let turn = app.start_scripted_turn(&thread).unwrap();
                assert_eq!(
                    app.await_scripted_turn_completed(&thread, &turn).is_ok(),
                    accepted
                );
            }
            assert!(matches!(
                app.stop().unwrap(),
                AppServerStopOutcome::Exited(SubordinateProcessExit { success: true, .. })
            ));
        }
    }

    #[test]
    fn forced_direct_child_settlement_is_not_successful_turn_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let executable = pinned_shell();
        let mut app = AppServerObservation::launch(
            PinnedSubordinateProcessRequest {
                executable,
                cwd,
                argv0: Some("sh".into()),
                args: vec!["-c".into(), "exec /bin/sleep 60".into()],
                envs: Vec::new(),
                limits: None,
                inherited_fds: Vec::new(),
            },
            MonotonicDeadline::after(lillux::time::Duration::from_secs(2)),
        )
        .unwrap();
        assert!(matches!(
            app.stop_until(
                MonotonicDeadline::after(lillux::time::Duration::ZERO),
                MonotonicDeadline::after(lillux::time::Duration::from_secs(2)),
            )
            .unwrap(),
            AppServerStopOutcome::Forced(SubordinateProcessExit { success: false, .. })
        ));
        assert!(matches!(
            app.stop_until(
                MonotonicDeadline::after(lillux::time::Duration::ZERO),
                MonotonicDeadline::after(lillux::time::Duration::from_secs(2)),
            )
            .unwrap(),
            AppServerStopOutcome::Forced(SubordinateProcessExit { success: false, .. })
        ));
    }

    #[test]
    fn retained_force_attempt_cannot_be_laundered_by_later_clean_exit() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let executable = pinned_shell();
        let mut app = AppServerObservation::launch(
            PinnedSubordinateProcessRequest {
                executable,
                cwd,
                argv0: Some("sh".into()),
                args: vec!["-c".into(), "IFS= read -r line; exit 0".into()],
                envs: Vec::new(),
                limits: None,
                inherited_fds: Vec::new(),
            },
            MonotonicDeadline::after(lillux::time::Duration::from_secs(2)),
        )
        .unwrap();
        // Represents a previous stop call that attempted force, then returned
        // uncertainty before it could reap. Even a later exit code zero must
        // remain non-qualifying after that attempt.
        app.force_attempted = true;
        assert!(matches!(
            app.stop().unwrap(),
            AppServerStopOutcome::Forced(SubordinateProcessExit { success: true, .. })
        ));
    }
}
