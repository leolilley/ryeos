//! The stub driver: no model, no network, no Claude Code.
//!
//! Every behaviour comes from the turn's goal payload, so a test or an
//! operator can exercise each outcome through the signed routes alone:
//!
//! ```json
//! "stub": {
//!   "duration_ms": 0,            // how long the turn runs (default 0)
//!   "outcome": "completed",      // completed | crash | interrupt
//!   "text": "result text",       // the turn's result text
//!   "progress_events": 3,        // turn/progress notifications (default 3)
//!   "structured_output": null    // any JSON, reported as given
//! }
//! ```
//!
//! `crash` and `interrupt` still end with `turn/completed`, carrying status
//! `crashed` and `interrupted`: a turn that cannot report its own end would
//! leave the session running forever.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::driver::{Driver, RpcError, TurnLaunch};
use crate::wire::Emitter;

/// The longest result text a turn reports. The bridge bounds one durable
/// payload well below its event limit; the stub stays far inside it.
pub const MAX_RESULT_TEXT_BYTES: usize = 128 * 1024;
/// The longest structured output a turn reports. A larger one is refused as
/// a failed turn rather than cut, because a cut structured result is wrong
/// data that still parses.
pub const MAX_STRUCTURED_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_PROGRESS_EVENTS: u64 = 1_000;
const MAX_DURATION_MS: u64 = 600_000;
/// Host-provided values the adapter holds. Result text is durable, so these
/// never appear in it.
const REDACTED_ENVIRONMENT: [&str; 2] = ["ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL"];
const MIN_REDACTED_VALUE_BYTES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Completed,
    Crash,
    Interrupt,
}

#[derive(Clone, Debug)]
struct Plan {
    duration_ms: u64,
    outcome: Outcome,
    text: String,
    progress_events: u64,
    structured_output: Value,
}

impl Plan {
    fn from_params(params: &Value) -> Result<Self, RpcError> {
        let default = Self {
            duration_ms: 0,
            outcome: Outcome::Completed,
            text: String::new(),
            progress_events: 3,
            structured_output: Value::Null,
        };
        let Some(stub) = params.get("stub").filter(|value| !value.is_null()) else {
            return Ok(default);
        };
        let stub = stub
            .as_object()
            .ok_or_else(|| RpcError::invalid("stub must be an object"))?;
        let mut plan = default;
        if let Some(value) = stub.get("duration_ms") {
            plan.duration_ms = value
                .as_u64()
                .filter(|value| *value <= MAX_DURATION_MS)
                .ok_or_else(|| RpcError::invalid("stub.duration_ms is out of range"))?;
        }
        if let Some(value) = stub.get("outcome") {
            plan.outcome = match value.as_str() {
                Some("completed") => Outcome::Completed,
                Some("crash") => Outcome::Crash,
                Some("interrupt") => Outcome::Interrupt,
                _ => return Err(RpcError::invalid("stub.outcome is not admitted")),
            };
        }
        if let Some(value) = stub.get("text") {
            plan.text = value
                .as_str()
                .ok_or_else(|| RpcError::invalid("stub.text must be text"))?
                .to_owned();
        }
        if let Some(value) = stub.get("progress_events") {
            plan.progress_events = value
                .as_u64()
                .filter(|value| *value <= MAX_PROGRESS_EVENTS)
                .ok_or_else(|| RpcError::invalid("stub.progress_events is out of range"))?;
        }
        if let Some(value) = stub.get("structured_output") {
            plan.structured_output = value.clone();
        }
        Ok(plan)
    }
}

struct Running {
    turn_id: String,
    cancel: Sender<()>,
}

#[derive(Default)]
struct Session {
    running: Option<Running>,
}

#[derive(Default)]
struct State {
    sessions: BTreeMap<String, Session>,
    next_session: u64,
    next_turn: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct StubDriver {
    state: Arc<Mutex<State>>,
    /// Distinguishes ids minted by this process from those of an earlier one.
    epoch: String,
    redactions: Arc<Vec<String>>,
}

impl StubDriver {
    /// A stub that redacts the values of the host-provided variables it was
    /// started with.
    pub fn from_environment() -> Self {
        let values = REDACTED_ENVIRONMENT
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .collect();
        Self::new(values)
    }

    pub fn new(redactions: Vec<String>) -> Self {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis());
        Self {
            state: Arc::new(Mutex::new(State::default())),
            epoch: format!("{millis:x}"),
            redactions: Arc::new(
                redactions
                    .into_iter()
                    .filter(|value| value.len() >= MIN_REDACTED_VALUE_BYTES)
                    .collect(),
            ),
        }
    }

    fn session_id_param(params: &Value) -> Result<&str, RpcError> {
        params
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .ok_or_else(|| RpcError::invalid("session_id is required"))
    }

    /// Stub sessions carry no conversation, so any id this adapter family
    /// minted can be resumed by a later process. A real driver must verify
    /// the id against Claude Code's own state instead.
    fn is_stub_id(id: &str) -> bool {
        id.starts_with("stub-session-")
    }
}

fn redact_text(text: &str, redactions: &[String]) -> String {
    let mut redacted = text.to_owned();
    for value in redactions {
        redacted = redacted.replace(value.as_str(), "[redacted]");
    }
    redacted
}

fn redact_value(value: &Value, redactions: &[String]) -> Value {
    match value {
        Value::String(text) => Value::String(redact_text(text, redactions)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| redact_value(value, redactions))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), redact_value(value, redactions)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn truncate_to_boundary(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// Wait out one slice of the turn. `true` means the turn was cancelled.
fn cancelled_during(cancel: &Receiver<()>, slice: Duration) -> bool {
    match cancel.recv_timeout(slice) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        Err(RecvTimeoutError::Timeout) => false,
    }
}

impl Driver for StubDriver {
    fn initialize(&self, _params: &Value) -> Result<Value, RpcError> {
        Ok(json!({"adapter": "claude-code-adapter", "mode": "stub", "protocol": 1}))
    }

    fn session_start(&self, params: &Value) -> Result<Value, RpcError> {
        let access = match params.get("access").and_then(Value::as_str) {
            None | Some("read_write") => "read_write",
            Some("read_only") => "read_only",
            Some(_) => return Err(RpcError::invalid("access is not admitted")),
        };
        let mut state = lock(&self.state);
        state.next_session += 1;
        let session_id = format!("stub-session-{}-{}", self.epoch, state.next_session);
        state
            .sessions
            .insert(session_id.clone(), Session::default());
        Ok(json!({"session_id": session_id, "access": access}))
    }

    fn session_resume(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = Self::session_id_param(params)?;
        let mut state = lock(&self.state);
        if !state.sessions.contains_key(session_id) {
            if !Self::is_stub_id(session_id) {
                return Ok(json!({"session_id": session_id, "state": "lost"}));
            }
            state
                .sessions
                .insert(session_id.to_owned(), Session::default());
        }
        let running = state
            .sessions
            .get(session_id)
            .is_some_and(|session| session.running.is_some());
        Ok(json!({
            "session_id": session_id,
            "state": if running { "running" } else { "idle" },
        }))
    }

    fn session_read(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = Self::session_id_param(params)?;
        let state = lock(&self.state);
        let state_name = match state.sessions.get(session_id) {
            Some(session) if session.running.is_some() => "running",
            Some(_) => "idle",
            None if Self::is_stub_id(session_id) => "idle",
            None => "lost",
        };
        Ok(json!({"session_id": session_id, "state": state_name}))
    }

    fn turn_start(&self, params: &Value, events: Emitter) -> Result<TurnLaunch, RpcError> {
        let session_id = Self::session_id_param(params)?.to_owned();
        let prompt = params
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid("prompt is required"))?;
        let plan = Plan::from_params(params)?;
        let (cancel, cancelled) = mpsc::channel();
        let turn_id = {
            let mut state = lock(&self.state);
            state.next_turn += 1;
            let turn_id = format!("stub-turn-{}-{}", self.epoch, state.next_turn);
            let session = state
                .sessions
                .get_mut(&session_id)
                .ok_or_else(|| RpcError::refused("session is not known to this adapter"))?;
            if session.running.is_some() {
                return Err(RpcError::refused("a turn is already running"));
            }
            session.running = Some(Running {
                turn_id: turn_id.clone(),
                cancel,
            });
            turn_id
        };
        let input_tokens = (prompt.len() / 4) as u64;
        let state = Arc::clone(&self.state);
        let redactions = Arc::clone(&self.redactions);
        let run_turn_id = turn_id.clone();
        Ok(TurnLaunch {
            response: json!({"turn_id": turn_id}),
            run: Box::new(move || {
                run_turn(
                    &events,
                    &state,
                    &redactions,
                    &cancelled,
                    &session_id,
                    &run_turn_id,
                    &plan,
                    input_tokens,
                );
            }),
        })
    }

    fn turn_interrupt(&self, params: &Value) -> Result<Value, RpcError> {
        let session_id = Self::session_id_param(params)?;
        let turn_id = params
            .get("turn_id")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid("turn_id is required"))?;
        let state = lock(&self.state);
        let interrupted = state
            .sessions
            .get(session_id)
            .and_then(|session| session.running.as_ref())
            .filter(|running| running.turn_id == turn_id)
            .is_some_and(|running| running.cancel.send(()).is_ok());
        Ok(json!({"interrupted": interrupted}))
    }

    fn shutdown(&self) {
        let state = lock(&self.state);
        for session in state.sessions.values() {
            if let Some(running) = &session.running {
                let _ = running.cancel.send(());
            }
        }
    }
}

fn run_turn(
    events: &Emitter,
    state: &Mutex<State>,
    redactions: &[String],
    cancelled: &Receiver<()>,
    session_id: &str,
    turn_id: &str,
    plan: &Plan,
    input_tokens: u64,
) {
    let _ = events.notify(
        "turn/started",
        json!({"session_id": session_id, "turn_id": turn_id}),
    );
    // Progress is spread evenly across the turn; whatever is left after the
    // last progress event is waited out before the turn ends.
    let slice_ms = plan.duration_ms / (plan.progress_events + 1);
    let remainder_ms = plan.duration_ms - slice_ms * plan.progress_events;
    let mut was_interrupted = false;
    for sequence in 1..=plan.progress_events {
        if cancelled_during(cancelled, Duration::from_millis(slice_ms)) {
            was_interrupted = true;
            break;
        }
        let _ = events.notify(
            "turn/progress",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "sequence": sequence,
                "message": format!("stub progress {sequence}"),
            }),
        );
    }
    if !was_interrupted {
        was_interrupted = cancelled_during(cancelled, Duration::from_millis(remainder_ms));
    }
    let status = if was_interrupted {
        "interrupted"
    } else {
        match plan.outcome {
            Outcome::Completed => "completed",
            Outcome::Crash => "crashed",
            Outcome::Interrupt => "interrupted",
        }
    };
    let (status, result) = build_result(plan, status, input_tokens, redactions);
    // Release the session before announcing the end: the bridge may start the
    // next turn the moment it observes the completion.
    if let Some(session) = lock(state).sessions.get_mut(session_id) {
        session.running = None;
    }
    let _ = events.notify(
        "turn/completed",
        json!({
            "session_id": session_id,
            "completed_turn_id": turn_id,
            "status": status,
            "result": result,
        }),
    );
}

/// The turn's final status and result. A structured output that cannot fit
/// turns the whole turn into `failed`; it is never cut.
fn build_result(
    plan: &Plan,
    status: &'static str,
    input_tokens: u64,
    redactions: &[String],
) -> (&'static str, Value) {
    let mut status = status;
    let mut text = match status {
        "crashed" if plan.text.is_empty() => "stub: claude code crashed".to_owned(),
        "interrupted" if plan.text.is_empty() => "stub: turn interrupted".to_owned(),
        "completed" if plan.text.is_empty() => "stub turn completed".to_owned(),
        _ => plan.text.clone(),
    };
    let mut structured_output = redact_value(&plan.structured_output, redactions);
    let encoded_bytes =
        serde_json::to_vec(&structured_output).map_or(usize::MAX, |bytes| bytes.len());
    if encoded_bytes > MAX_STRUCTURED_OUTPUT_BYTES {
        status = "failed";
        structured_output = Value::Null;
        text = format!("stub: structured output exceeds {MAX_STRUCTURED_OUTPUT_BYTES} bytes");
    }
    let (text, truncated) =
        truncate_to_boundary(&redact_text(&text, redactions), MAX_RESULT_TEXT_BYTES);
    let output_tokens = (text.len() / 4) as u64;
    (
        status,
        json!({
            "text": text,
            "truncated": truncated,
            "structured_output": structured_output,
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cost_micro_usd": 0,
            },
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::Server;
    use std::io::{Cursor, Write};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    struct HeldOpen {
        input: Cursor<String>,
        sink: Sink,
    }

    impl std::io::Read for HeldOpen {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = self.input.read(buffer)?;
            if read > 0 {
                return Ok(read);
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline {
                if String::from_utf8_lossy(&lock(&self.sink.0)).contains("\"turn/completed\"") {
                    return Ok(0);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(0)
        }
    }

    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run a whole conversation and return every line the adapter wrote.
    fn converse(redactions: &[&str], requests: &[Value]) -> Vec<Value> {
        let sink = Sink::default();
        let driver = StubDriver::new(redactions.iter().map(|value| (*value).to_owned()).collect());
        let mut server = Server::new(Arc::new(driver), Emitter::new(sink.clone()));
        let mut input = String::new();
        for request in requests {
            input.push_str(&request.to_string());
            input.push('\n');
        }
        server.run(Cursor::new(input)).unwrap();
        let written = lock(&sink.0).clone();
        String::from_utf8(written)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn start_session() -> Value {
        json!({"id": 2, "method": "session/start", "params": {"cwd": "/work", "access": "read_write"}})
    }

    /// Run one turn against a driver whose session was started first. Extra
    /// requests may name the session and the (first) turn with the
    /// placeholders `$SESSION` and `$TURN`.
    fn run_turn_script(stub: Value, redactions: &[&str], extra: Vec<Value>) -> Vec<Value> {
        let sink = Sink::default();
        let driver = Arc::new(StubDriver::new(
            redactions.iter().map(|value| (*value).to_owned()).collect(),
        ));
        let started = driver.session_start(&json!({"cwd": "/work"})).unwrap();
        let session_id = started["session_id"].as_str().unwrap().to_owned();
        let turn_id = format!("stub-turn-{}-1", driver.epoch);
        let mut requests = vec![json!({
            "id": 3, "method": "turn/start",
            "params": {"session_id": session_id, "cwd": "/work", "prompt": "do the thing", "stub": stub}
        })];
        requests.extend(extra);
        let mut input = String::new();
        for request in &requests {
            input.push_str(
                &request
                    .to_string()
                    .replace("$SESSION", &session_id)
                    .replace("$TURN", &turn_id),
            );
            input.push('\n');
        }
        let mut server = Server::new(driver, Emitter::new(sink.clone()));
        // The bridge keeps stdin open while a turn runs; end of input means
        // the session is over and interrupts the turn. Hold input open until
        // the turn has reported its completion.
        server
            .run(std::io::BufReader::new(HeldOpen {
                input: Cursor::new(input),
                sink: sink.clone(),
            }))
            .unwrap();
        let written = lock(&sink.0).clone();
        String::from_utf8(written)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn response(lines: &[Value], id: u64) -> &Value {
        lines
            .iter()
            .find(|line| line["id"] == id)
            .expect("the request was answered")
    }

    fn completion(lines: &[Value]) -> &Value {
        lines
            .iter()
            .find(|line| line["method"] == "turn/completed")
            .expect("every turn ends with turn/completed")
    }

    #[test]
    fn handshake_and_session_routes_answer() {
        let lines = converse(
            &[],
            &[
                json!({"id": 1, "method": "initialize", "params": {}}),
                json!({"method": "initialized", "params": {}}),
                start_session(),
                json!({"id": 3, "method": "session/read", "params": {"session_id": "stub-session-0-9"}}),
                json!({"id": 4, "method": "session/resume", "params": {"session_id": "stub-session-0-9", "cwd": "/work"}}),
                json!({"id": 5, "method": "session/resume", "params": {"session_id": "foreign", "cwd": "/work"}}),
                json!({"id": 6, "method": "nothing/here", "params": {}}),
            ],
        );
        assert_eq!(lines[0]["id"], 1);
        assert_eq!(lines[0]["result"]["mode"], "stub");
        assert_eq!(lines[1]["id"], 2);
        assert!(
            lines[1]["result"]["session_id"]
                .as_str()
                .unwrap()
                .starts_with("stub-session-")
        );
        assert_eq!(lines[2]["result"]["state"], "idle");
        assert_eq!(lines[3]["result"]["session_id"], "stub-session-0-9");
        assert_eq!(lines[3]["result"]["state"], "idle");
        assert_eq!(lines[4]["result"]["state"], "lost");
        assert_eq!(lines[5]["error"]["code"], -32601);
        assert_eq!(lines.len(), 6, "`initialized` is never answered");
    }

    #[test]
    fn a_completed_turn_answers_first_then_reports_its_result() {
        let lines = run_turn_script(
            json!({"outcome": "completed", "text": "all done", "progress_events": 2,
                   "structured_output": {"verdict": "ok"}}),
            &[],
            vec![],
        );
        assert_eq!(lines[0]["id"], 3, "the turn.start response comes first");
        let turn_id = lines[0]["result"]["turn_id"].as_str().unwrap();
        let methods: Vec<&str> = lines[1..]
            .iter()
            .map(|line| line["method"].as_str().unwrap())
            .collect();
        assert_eq!(
            methods,
            [
                "turn/started",
                "turn/progress",
                "turn/progress",
                "turn/completed"
            ]
        );
        assert_eq!(lines[1]["params"]["turn_id"], turn_id);
        let done = completion(&lines);
        assert_eq!(done["params"]["completed_turn_id"], turn_id);
        assert_eq!(done["params"]["status"], "completed");
        assert_eq!(done["params"]["result"]["text"], "all done");
        assert_eq!(
            done["params"]["result"]["structured_output"]["verdict"],
            "ok"
        );
        assert_eq!(done["params"]["result"]["usage"]["cost_micro_usd"], 0);
    }

    #[test]
    fn a_crash_still_completes_the_turn_with_status_crashed() {
        let lines = run_turn_script(
            json!({"outcome": "crash", "progress_events": 0}),
            &[],
            vec![],
        );
        let done = completion(&lines);
        assert_eq!(done["params"]["status"], "crashed");
        assert_eq!(
            done["params"]["result"]["text"],
            "stub: claude code crashed"
        );
    }

    #[test]
    fn an_interrupt_outcome_completes_as_interrupted() {
        let lines = run_turn_script(
            json!({"outcome": "interrupt", "progress_events": 1}),
            &[],
            vec![],
        );
        assert_eq!(completion(&lines)["params"]["status"], "interrupted");
    }

    #[test]
    fn a_turn_interrupt_request_ends_a_long_turn_as_interrupted() {
        let lines = run_turn_script(
            json!({"duration_ms": 60000, "progress_events": 0}),
            &[],
            vec![
                json!({"id": 4, "method": "turn/interrupt", "params": {"session_id": "$SESSION", "turn_id": "$TURN"}}),
                json!({"id": 5, "method": "turn/interrupt", "params": {"session_id": "$SESSION", "turn_id": "someone-else"}}),
            ],
        );
        assert_eq!(response(&lines, 4)["result"]["interrupted"], true);
        assert_eq!(response(&lines, 5)["result"]["interrupted"], false);
        assert_eq!(completion(&lines)["params"]["status"], "interrupted");
    }

    #[test]
    fn a_delivered_value_in_result_text_is_redacted() {
        let lines = run_turn_script(
            json!({"text": "key sk-secret-value leaked", "structured_output": {"note": "sk-secret-value"}}),
            &["sk-secret-value"],
            vec![],
        );
        let result = &completion(&lines)["params"]["result"];
        assert_eq!(result["text"], "key [redacted] leaked");
        assert_eq!(result["structured_output"]["note"], "[redacted]");
    }

    #[test]
    fn oversize_text_is_truncated_and_oversize_structure_fails_the_turn() {
        let lines = run_turn_script(
            json!({"text": "x".repeat(MAX_RESULT_TEXT_BYTES + 10)}),
            &[],
            vec![],
        );
        let result = &completion(&lines)["params"]["result"];
        assert_eq!(
            result["text"].as_str().unwrap().len(),
            MAX_RESULT_TEXT_BYTES
        );
        assert_eq!(result["truncated"], true);

        let lines = run_turn_script(
            json!({"structured_output": {"blob": "y".repeat(MAX_STRUCTURED_OUTPUT_BYTES)}}),
            &[],
            vec![],
        );
        let done = completion(&lines);
        assert_eq!(done["params"]["status"], "failed");
        assert!(done["params"]["result"]["structured_output"].is_null());
    }

    #[test]
    fn a_second_turn_while_one_runs_is_refused() {
        let lines = run_turn_script(
            json!({"duration_ms": 60000, "progress_events": 0}),
            &[],
            vec![
                json!({"id": 4, "method": "turn/start", "params": {"session_id": "$SESSION", "cwd": "/work", "prompt": "again"}}),
            ],
        );
        assert_eq!(response(&lines, 4)["error"]["code"], -32000);
    }
}
