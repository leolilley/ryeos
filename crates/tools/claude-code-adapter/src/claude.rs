//! The Claude Code driver: one `claude -p` process per turn.
//!
//! `turn/start` is answered at once; the turn then runs on its own thread,
//! which starts Claude Code in its own process group, feeds the prompt on
//! stdin, parses the `stream-json` output line by line and always ends with
//! `turn/completed`, whatever happened to the process. The adapter owns the
//! session id the bridge sees and maps it to Claude Code's (see `sessions`).

use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::driver::{Driver, RpcError, TurnLaunch};
use crate::invocation::{Config, SessionFlag, SessionSettings, TurnSettings, build_argv};
use crate::sessions::{Record, SessionMap, Verdict};
use crate::stream::{End, Redactor, StreamParser};
use crate::wire::Emitter;
use lillux::child_group::{self, GroupSignal};

/// Host-provided credential values the adapter holds. They never appear in
/// anything the adapter reports.
const REDACTED_ENVIRONMENT: [&str; 3] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
];
/// The longest line read from Claude Code. A longer one is dropped whole.
const MAX_STREAM_LINE_BYTES: usize = 8 * 1024 * 1024;
const STDERR_TAIL_BYTES: usize = 512;
const TERMINATE_GRACE: Duration = Duration::from_secs(3);
const DRAIN_GRACE: Duration = Duration::from_secs(2);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(10);
const SESSION_ID_PREFIX: &str = "cc-session-";

enum Control {
    Interrupt,
    Shutdown,
}

enum Message {
    Line(Vec<u8>),
    Closed,
    Control(Control),
}

struct Running {
    turn_id: String,
    messages: Sender<Message>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct ClaudeCodeDriver {
    config: Arc<Config>,
    redactor: Redactor,
    sessions: Arc<SessionMap>,
    running: Arc<Mutex<BTreeMap<String, Running>>>,
    closing: Arc<AtomicBool>,
}

impl ClaudeCodeDriver {
    pub fn from_environment() -> Result<Self, String> {
        let get = |name: &str| std::env::var(name).ok();
        let config = Config::from_environment(get)?;
        let redactor = Redactor::new(REDACTED_ENVIRONMENT.iter().filter_map(|name| get(name)));
        std::fs::create_dir_all(config.adapter_directory())
            .map_err(|error| format!("create the adapter directory: {error}"))?;
        Ok(Self::new(config, redactor))
    }

    pub fn new(config: Config, redactor: Redactor) -> Self {
        let sessions = SessionMap::new(config.home.clone(), config.session_map_path());
        Self {
            config: Arc::new(config),
            redactor,
            sessions: Arc::new(sessions),
            running: Arc::new(Mutex::new(BTreeMap::new())),
            closing: Arc::new(AtomicBool::new(false)),
        }
    }

    fn session_id_param(params: &Value) -> Result<&str, RpcError> {
        params
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .ok_or_else(|| RpcError::invalid("session_id is required"))
    }

    /// `resume` and `read` answer the same question: is the conversation
    /// intact, and is a turn running? Neither changes anything.
    fn session_state(&self, session_id: &str) -> &'static str {
        if lock(&self.running).contains_key(session_id) {
            return "running";
        }
        match self.sessions.verify(session_id) {
            Verdict::Intact => "idle",
            Verdict::Unknown | Verdict::Lost(_) | Verdict::Interrupted => "lost",
        }
    }
}

fn optional_text(params: &Value, name: &str, limit: usize) -> Result<Option<String>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.len() <= limit => Ok(Some(text.clone())),
        Some(_) => Err(RpcError::invalid(format!("{name} is not admitted"))),
    }
}

impl Driver for ClaudeCodeDriver {
    fn initialize(&self, _params: &Value) -> Result<Value, RpcError> {
        Ok(json!({"adapter": "claude-code-adapter", "mode": "live", "protocol": 1}))
    }

    fn session_start(&self, params: &Value) -> Result<Value, RpcError> {
        let read_only = match params.get("access").and_then(Value::as_str) {
            Some("read_write") => false,
            Some("read_only") => true,
            _ => return Err(RpcError::invalid("access is not admitted")),
        };
        let tools = match params.get("tools") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(tools)) => tools
                .iter()
                .map(|tool| tool.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| RpcError::invalid("tools must be names"))?,
            Some(_) => return Err(RpcError::invalid("tools must be a list")),
        };
        let settings = SessionSettings {
            read_only,
            model: optional_text(params, "model", 128)?,
            effort: optional_text(params, "effort", 16)?,
            tools,
            system_prompt: optional_text(params, "system_prompt", 100_000)?,
        };
        settings.validate().map_err(RpcError::invalid)?;
        let session_id = format!("{SESSION_ID_PREFIX}{}", uuid::Uuid::new_v4());
        self.sessions
            .insert(
                &session_id,
                Record {
                    claude_session_id: uuid::Uuid::new_v4().to_string(),
                    settings,
                    turns_completed: 0,
                    transcript_bytes: 0,
                    turn_in_flight: false,
                },
            )
            .map_err(|_| RpcError::refused("the session map cannot be written"))?;
        Ok(json!({
            "session_id": session_id,
            "access": if read_only { "read_only" } else { "read_write" },
        }))
    }

    fn session_resume(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = Self::session_id_param(params)?;
        Ok(json!({"session_id": session_id, "state": self.session_state(session_id)}))
    }

    fn session_read(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = Self::session_id_param(params)?;
        Ok(json!({"session_id": session_id, "state": self.session_state(session_id)}))
    }

    fn turn_start(&self, params: &Value, events: Emitter) -> Result<TurnLaunch, RpcError> {
        if self.closing.load(Ordering::SeqCst) {
            return Err(RpcError::refused("the adapter is closing"));
        }
        let session_id = Self::session_id_param(params)?.to_owned();
        let prompt = params
            .get("prompt")
            .and_then(Value::as_str)
            .filter(|prompt| !prompt.is_empty())
            .ok_or_else(|| RpcError::invalid("prompt is required"))?
            .to_owned();
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| RpcError::invalid("cwd must be an absolute path"))?;
        let deadline = match params.get("deadline_seconds") {
            None | Some(Value::Null) => None,
            Some(value) => Some(Duration::from_secs(
                value
                    .as_u64()
                    .filter(|seconds| (1..=86_400).contains(seconds))
                    .ok_or_else(|| RpcError::invalid("deadline_seconds is out of range"))?,
            )),
        };
        let output_schema = match params.get("output_schema") {
            None | Some(Value::Null) => None,
            Some(schema @ Value::Object(_)) => Some(schema.clone()),
            Some(_) => return Err(RpcError::invalid("output_schema must be an object")),
        };
        let max_budget_usd = match params.get("max_budget_usd") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_f64()
                    .filter(|usd| usd.is_finite() && *usd > 0.0 && *usd <= 10_000.0)
                    .ok_or_else(|| RpcError::invalid("max_budget_usd is out of range"))?,
            ),
        };
        let system_prompt = optional_text(params, "system_prompt", 100_000)?;

        let (messages, inbox) = mpsc::channel();
        let turn_id = format!("cc-turn-{}", uuid::Uuid::new_v4());
        let (record, session_flag) = {
            let mut running = lock(&self.running);
            if running.contains_key(&session_id) {
                return Err(RpcError::refused("a turn is already running"));
            }
            match self.sessions.verify(&session_id) {
                Verdict::Intact => {}
                Verdict::Unknown => {
                    return Err(RpcError::refused("session is not known to this adapter"));
                }
                Verdict::Lost(_) | Verdict::Interrupted => {
                    return Err(RpcError::refused("the session cannot be continued"));
                }
            }
            let record = self
                .sessions
                .get(&session_id)
                .ok_or_else(|| RpcError::refused("session is not known to this adapter"))?;
            let session_flag = if self
                .sessions
                .transcript(&record.claude_session_id)
                .is_some()
            {
                SessionFlag::Resume(record.claude_session_id.clone())
            } else {
                SessionFlag::Create(record.claude_session_id.clone())
            };
            self.sessions
                .update(&session_id, |record| record.turn_in_flight = true)
                .map_err(|_| RpcError::refused("the session map cannot be written"))?;
            running.insert(
                session_id.clone(),
                Running {
                    turn_id: turn_id.clone(),
                    messages: messages.clone(),
                },
            );
            (record, session_flag)
        };
        let context = TurnContext {
            events,
            config: Arc::clone(&self.config),
            redactor: self.redactor.clone(),
            sessions: Arc::clone(&self.sessions),
            running: Arc::clone(&self.running),
            session_id,
            turn_id: turn_id.clone(),
            settings: record.settings,
            turn: TurnSettings {
                system_prompt,
                output_schema,
                max_budget_usd,
                session: session_flag,
            },
            claude_session_id: record.claude_session_id,
            prompt,
            cwd,
            deadline,
            messages,
            inbox,
        };
        Ok(TurnLaunch {
            response: json!({"turn_id": turn_id}),
            run: Box::new(move || run_turn(context)),
        })
    }

    fn turn_interrupt(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = Self::session_id_param(params)?;
        let turn_id = params
            .get("turn_id")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid("turn_id is required"))?;
        let running = lock(&self.running);
        let interrupted = running
            .get(session_id)
            .filter(|running| running.turn_id == turn_id)
            .is_some_and(|running| {
                running
                    .messages
                    .send(Message::Control(Control::Interrupt))
                    .is_ok()
            });
        Ok(json!({"interrupted": interrupted}))
    }

    fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        for running in lock(&self.running).values() {
            let _ = running.messages.send(Message::Control(Control::Shutdown));
        }
        // Wait for each turn to kill Claude Code and report, so the process
        // exits only after the whole group is gone.
        let give_up = Instant::now() + SHUTDOWN_WAIT;
        while !lock(&self.running).is_empty() && Instant::now() < give_up {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

struct TurnContext {
    events: Emitter,
    config: Arc<Config>,
    redactor: Redactor,
    sessions: Arc<SessionMap>,
    running: Arc<Mutex<BTreeMap<String, Running>>>,
    session_id: String,
    turn_id: String,
    settings: SessionSettings,
    turn: TurnSettings,
    claude_session_id: String,
    prompt: String,
    cwd: PathBuf,
    deadline: Option<Duration>,
    messages: Sender<Message>,
    inbox: Receiver<Message>,
}

fn run_turn(context: TurnContext) {
    let _ = context.events.notify(
        "turn/started",
        json!({"session_id": context.session_id, "turn_id": context.turn_id}),
    );
    // A panic must not leave the session running forever.
    let (status, result) = catch_unwind(AssertUnwindSafe(|| drive(&context)))
        .unwrap_or_else(|_| crashed_without_a_stream(&context));
    // Record the turn and release the session before announcing the end: the
    // bridge may start the next turn the moment it sees the completion.
    let transcript = context.sessions.transcript(&context.claude_session_id);
    let _ = context.sessions.update(&context.session_id, |record| {
        record.turn_in_flight = false;
        if let Some((_, length)) = &transcript {
            record.turns_completed += 1;
            record.transcript_bytes = *length;
        }
    });
    lock(&context.running).remove(&context.session_id);
    let _ = context.events.notify(
        "turn/completed",
        json!({
            "session_id": context.session_id,
            "completed_turn_id": context.turn_id,
            "status": status,
            "result": result,
        }),
    );
}

fn crashed_without_a_stream(context: &TurnContext) -> (&'static str, Value) {
    StreamParser::new(context.settings.model.clone(), context.redactor.clone())
        .complete(&End::Exited(None), "the adapter's turn thread failed")
}

fn drive(context: &TurnContext) -> (&'static str, Value) {
    let mut parser = StreamParser::new(context.settings.model.clone(), context.redactor.clone());
    let not_started = |parser: &StreamParser, why: String| {
        parser.complete(&End::NotStarted(context.redactor.text(&why)), "")
    };
    if let Err(error) = write_mcp_config(&context.config) {
        return not_started(&parser, format!("write the MCP config: {error}"));
    }
    if !context.cwd.is_dir() {
        return not_started(&parser, "the working directory does not exist".to_owned());
    }
    let argv = build_argv(
        &context.config,
        &context.settings,
        &context.turn,
        &context.config.mcp_config_path(),
    );
    let mut command = Command::new(&context.config.executable);
    command
        .args(argv)
        .env_clear()
        .envs(context.config.child_environment())
        .current_dir(&context.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match child_group::spawn_in_own_group(&mut command) {
        Ok(child) => child,
        Err(error) => return not_started(&parser, format!("spawn failed: {error}")),
    };
    let stderr_tail = Arc::new(Mutex::new(Vec::<u8>::new()));
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = context.prompt.clone();
        std::thread::spawn(move || {
            let _ = stdin.write_all(prompt.as_bytes());
        });
    }
    if let Some(stdout) = child.stdout.take() {
        let messages = context.messages.clone();
        std::thread::spawn(move || read_lines(stdout, &messages));
    }
    if let Some(stderr) = child.stderr.take() {
        let tail = Arc::clone(&stderr_tail);
        std::thread::spawn(move || keep_tail(stderr, &tail));
    }

    let started = Instant::now();
    let mut end: Option<End> = None;
    while end.is_none() {
        let wait = match context.deadline {
            Some(deadline) => deadline.saturating_sub(started.elapsed()),
            None => Duration::from_secs(3600),
        };
        match context.inbox.recv_timeout(wait) {
            Ok(Message::Line(line)) => {
                if let Some(progress) = parser.on_line(&line, Instant::now()) {
                    let _ = context.events.notify(
                        "turn/progress",
                        json!({
                            "session_id": context.session_id,
                            "turn_id": context.turn_id,
                            "sequence": progress.sequence,
                            "message": progress.message,
                        }),
                    );
                }
            }
            Ok(Message::Closed) | Err(RecvTimeoutError::Disconnected) => {
                end = Some(End::Exited(None));
            }
            Ok(Message::Control(Control::Interrupt)) => end = Some(End::Interrupted),
            Ok(Message::Control(Control::Shutdown)) => end = Some(End::Shutdown),
            Err(RecvTimeoutError::Timeout) => {
                if context
                    .deadline
                    .is_some_and(|deadline| started.elapsed() >= deadline)
                {
                    end = Some(End::Deadline);
                }
            }
        }
    }
    let mut end = end.unwrap_or(End::Exited(None));
    let status = if matches!(end, End::Exited(_)) {
        // Stdout closed: the process is exiting, so let it.
        wait_for(&mut child, TERMINATE_GRACE)
    } else {
        None
    };
    stop_group(&mut child);
    let status = status.or_else(|| child.try_wait().ok().flatten());
    if let End::Exited(code) = &mut end {
        *code = status.and_then(|status| status.code());
    }
    // Lines written just before the stop can still carry the result.
    let drain_until = Instant::now() + DRAIN_GRACE;
    while let Some(left) = drain_until.checked_duration_since(Instant::now()) {
        match context.inbox.recv_timeout(left) {
            Ok(Message::Line(line)) => {
                parser.on_line(&line, Instant::now());
            }
            Ok(Message::Control(_)) => {}
            Ok(Message::Closed) | Err(_) => break,
        }
    }
    let tail = String::from_utf8_lossy(&lock(&stderr_tail)).into_owned();
    parser.complete(&end, &context.redactor.text(&tail))
}

/// Stop Claude Code and everything it started. Terminate first so it can
/// close its own children, then kill whatever is left of the group.
fn stop_group(child: &mut Child) {
    let leader = child.id();
    child_group::signal_group(leader, GroupSignal::Terminate);
    wait_for(child, TERMINATE_GRACE);
    child_group::signal_group(leader, GroupSignal::Kill);
    let _ = child.wait();
}

fn wait_for(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let give_up = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < give_up => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => return None,
        }
    }
}

fn read_lines(stdout: impl Read, messages: &Sender<Message>) {
    let mut reader = std::io::BufReader::new(stdout);
    loop {
        let mut line = Vec::new();
        let read = (&mut reader)
            .take((MAX_STREAM_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .unwrap_or(0);
        if read == 0 {
            let _ = messages.send(Message::Closed);
            return;
        }
        let complete = line.last() == Some(&b'\n');
        if !complete && line.len() > MAX_STREAM_LINE_BYTES {
            // Drop an oversize line whole, to its end.
            let mut sink = Vec::new();
            loop {
                sink.clear();
                match (&mut reader).take(64 * 1024).read_until(b'\n', &mut sink) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if sink.last() == Some(&b'\n') => break,
                    Ok(_) => {}
                }
            }
            continue;
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if !line.is_empty() && messages.send(Message::Line(line)).is_err() {
            return;
        }
    }
}

fn keep_tail(stderr: impl Read, tail: &Mutex<Vec<u8>>) {
    let mut reader = std::io::BufReader::new(stderr);
    let mut chunk = [0u8; 4096];
    while let Ok(read) = reader.read(&mut chunk) {
        if read == 0 {
            return;
        }
        let mut tail = lock(tail);
        tail.extend_from_slice(&chunk[..read]);
        let excess = tail.len().saturating_sub(STDERR_TAIL_BYTES);
        tail.drain(..excess);
    }
}

/// The adapter writes the MCP config itself, from the signed list in its
/// environment, so no other MCP definition can reach Claude Code.
fn write_mcp_config(config: &Config) -> std::io::Result<()> {
    std::fs::create_dir_all(config.adapter_directory())?;
    let path = config.mcp_config_path();
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(&config.mcp_config)?)?;
    std::fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::Server;
    use std::os::unix::fs::PermissionsExt;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct Fixture {
        home: PathBuf,
        driver: Arc<ClaudeCodeDriver>,
        sink: Sink,
    }

    /// A driver whose "Claude Code" is a shell script printing `script`.
    fn fixture(script: &str) -> Fixture {
        let home = std::env::temp_dir().join(format!("cc-adapter-driver-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let executable = home.join("fake-claude");
        std::fs::write(&executable, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = Config {
            executable,
            home: home.join("config"),
            mcp_config: json!({"mcpServers": {}}),
            bare: false,
            environment: vec![("PATH".into(), "/usr/bin:/bin".into())],
        };
        Fixture {
            home,
            driver: Arc::new(ClaudeCodeDriver::new(
                config,
                Redactor::new(["sk-secret-key-1234".to_owned()]),
            )),
            sink: Sink::default(),
        }
    }

    impl Fixture {
        fn call(&self, method: &str, params: Value) -> Value {
            let mut server = Server::new(self.driver.clone(), Emitter::new(self.sink.clone()));
            let request = json!({"id": 1, "method": method, "params": params});
            let input = std::io::Cursor::new(format!("{request}\n"));
            let before = lock(&self.sink.0).len();
            // `run` returns at input EOF after shutting the driver down, so
            // wait for the completion in the sink first when a turn runs.
            let sink = self.sink.clone();
            let watcher = (method == "turn/start").then(|| {
                std::thread::spawn(move || {
                    let give_up = Instant::now() + Duration::from_secs(20);
                    // A refused turn answers with an error and never completes.
                    while Instant::now() < give_up && {
                        let seen = String::from_utf8_lossy(&lock(&sink.0)).into_owned();
                        !seen.contains("turn/completed") && !seen.contains("\"error\"")
                    } {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                })
            });
            struct Delayed(std::io::Cursor<String>, Option<std::thread::JoinHandle<()>>);
            impl Read for Delayed {
                fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                    let read = self.0.read(buffer)?;
                    if read == 0
                        && let Some(handle) = self.1.take()
                    {
                        let _ = handle.join();
                    }
                    Ok(read)
                }
            }
            let reader = std::io::BufReader::new(Delayed(input, watcher));
            server.run(reader).unwrap();
            let text = String::from_utf8_lossy(&lock(&self.sink.0)[before..]).into_owned();
            // Fresh driver state after `shutdown`: reopen for the next call.
            self.driver.closing.store(false, Ordering::SeqCst);
            Value::Array(
                text.lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect(),
            )
        }
    }

    fn completion(lines: &Value) -> &Value {
        lines
            .as_array()
            .unwrap()
            .iter()
            .find(|line| line["method"] == "turn/completed")
            .expect("a turn/completed notification")
    }

    fn start_session(fixture: &Fixture, extra: Value) -> String {
        let mut params = json!({"access": "read_write", "cwd": fixture.home.display().to_string()});
        params
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        fixture.call("session/start", params)[0]["result"]["session_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn turn(fixture: &Fixture, session: &str) -> Value {
        fixture.call(
            "turn/start",
            json!({"session_id": session, "cwd": fixture.home.display().to_string(), "prompt": "go"}),
        )
    }

    #[test]
    fn a_turn_streams_progress_and_completes_with_the_result_and_usage() {
        let fixture = fixture(
            r#"cat > /dev/null
echo '{"type":"system","subtype":"init","session_id":"s"}'
echo '{"type":"assistant","message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"text","text":"working on it"}],"usage":{"input_tokens":5,"output_tokens":2}},"parent_tool_use_id":null}'
echo '{"type":"result","subtype":"success","result":"all done sk-secret-key-1234","total_cost_usd":0.5,"modelUsage":{"claude-opus-5-5":{"inputTokens":5,"outputTokens":7}}}'"#,
        );
        let session = start_session(&fixture, json!({"tools": ["Agent"]}));
        let lines = turn(&fixture, &session);
        // The response is written before any notification.
        assert!(lines[0]["result"]["turn_id"].is_string());
        assert_eq!(lines[1]["method"], "turn/started");
        let done = completion(&lines);
        assert_eq!(done["params"]["status"], "completed");
        assert_eq!(done["params"]["result"]["text"], "all done [redacted]");
        assert_eq!(done["params"]["result"]["usage"]["cost_micro_usd"], 500_000);
        assert!(
            lines
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line["method"] == "turn/progress")
        );
        // The map records the turn only if Claude Code left a transcript, and
        // the fake left none.
        assert_eq!(fixture.driver.session_state(&session), "idle");
    }

    #[test]
    fn a_crash_mid_stream_still_completes_the_turn() {
        let fixture = fixture(
            r#"cat > /dev/null
echo '{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"half way"}],"usage":{}}}'
echo 'boom' >&2
exit 3"#,
        );
        let session = start_session(&fixture, json!({}));
        let lines = turn(&fixture, &session);
        let done = completion(&lines);
        assert_eq!(done["params"]["status"], "crashed");
        let text = done["params"]["result"]["text"].as_str().unwrap();
        assert_eq!(text, "half way");
        assert_eq!(done["params"]["result"]["reason"], "crash");
    }

    #[test]
    fn a_missing_executable_fails_the_turn_instead_of_hanging_it() {
        let fixture = fixture("exit 0");
        std::fs::remove_file(fixture.driver.config.executable.clone()).unwrap();
        let session = start_session(&fixture, json!({}));
        let lines = turn(&fixture, &session);
        assert_eq!(completion(&lines)["params"]["status"], "failed");
    }

    #[test]
    fn the_deadline_kills_the_whole_group_and_reports_failed() {
        let fixture = fixture("cat > /dev/null\nsleep 60 &\nsleep 60");
        let session = start_session(&fixture, json!({}));
        let started = Instant::now();
        let lines = fixture.call(
            "turn/start",
            json!({"session_id": session, "cwd": fixture.home.display().to_string(),
                   "prompt": "go", "deadline_seconds": 1}),
        );
        assert!(started.elapsed() < Duration::from_secs(15));
        let done = completion(&lines);
        assert_eq!(done["params"]["status"], "failed");
        assert_eq!(done["params"]["result"]["reason"], "deadline");
    }

    #[test]
    fn unknown_and_unbacked_sessions_answer_lost_never_idle() {
        let fixture = fixture("exit 0");
        assert_eq!(fixture.driver.session_state("cc-session-nope"), "lost");
        let session = start_session(&fixture, json!({}));
        assert_eq!(fixture.driver.session_state(&session), "idle");
        // A session that has spoken, with no transcript behind it, is lost.
        fixture
            .driver
            .sessions
            .update(&session, |record| record.turns_completed = 1)
            .unwrap();
        assert_eq!(fixture.driver.session_state(&session), "lost");
        let refused = fixture.call(
            "turn/start",
            json!({"session_id": session,
            "cwd": fixture.home.display().to_string(), "prompt": "go"}),
        );
        assert!(
            refused[0]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("cannot be continued")
        );
    }

    #[test]
    fn a_read_only_session_refuses_write_tools_at_start() {
        let fixture = fixture("exit 0");
        let refused = fixture.call(
            "session/start",
            json!({"access": "read_only", "cwd": "/", "tools": ["Write"]}),
        );
        assert!(refused[0]["error"].is_object());
    }
}
