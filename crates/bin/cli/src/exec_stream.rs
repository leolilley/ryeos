//! Light terminal renderer for a streamed `ryeos execute` run.
//!
//! Consumes SSE events from the gateway `/execute/stream` and prints a readable
//! live log: the launch id, the assistant's text deltas streamed inline, and a
//! final status line on the terminal event. Deliberately NOT the TUI timeline
//! widget — this is a plain stdout log for the CLI.
//!
//! What happened between those lines comes from the daemon's timeline
//! projection. On each durable event a [`TimelineFeed`] fetches the chain's
//! timeline upserts, prints the entries that are new and a short update line
//! when a printed entry's state changes. At most one fetch is in flight;
//! events that arrive meanwhile coalesce into the next one. Streams that carry
//! no feed (a thread tail) keep the generic one-line event summary.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ryeos_state::event_types::{ThreadOutcomeKind, thread_terminal_outcome};
use serde_json::Value;
use tokio::sync::Notify;

use crate::timeline_text::{Applied, LiveLine, LiveView, render_live_line};
use crate::transport::http::SseEvent;

/// What the caller should do after an event.
pub enum StreamOutcome {
    /// Keep reading.
    Continue,
    /// Run finished successfully — stop, exit zero.
    Done,
    /// Run failed — stop, exit non-zero with this detail.
    Failed(String),
}

/// A line the renderer wants written, other than an inline text fragment.
#[derive(Debug, Clone, PartialEq)]
enum Emit {
    Text(String),
    Error(String),
}

/// One event, decided but not yet written.
struct Interpreted {
    /// Assistant text, written inline with no line break.
    fragment: Option<String>,
    emits: Vec<Emit>,
    outcome: StreamOutcome,
}

/// Decide what one streamed event prints and what it means for the run.
/// `generic_lines` keeps the one-line `- event key=value` summary for events
/// with no text; a stream that has a timeline feed turns it off.
fn interpret(ev: &SseEvent, data: &Value, generic_lines: bool) -> Interpreted {
    let done = |emits: Vec<Emit>, outcome: StreamOutcome| Interpreted {
        fragment: None,
        emits,
        outcome,
    };
    match ev.event.as_str() {
        "stream_started" => done(
            data.get("thread_id")
                .and_then(|v| v.as_str())
                .map(|tid| Emit::Text(format!("thread: {tid}")))
                .into_iter()
                .collect(),
            StreamOutcome::Continue,
        ),
        "stream_error" => {
            let code = data
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("stream_error");
            let msg = failure_reason(data);
            let detail = format!("{code}: {msg}");
            done(
                vec![Emit::Error(detail.clone())],
                StreamOutcome::Failed(detail),
            )
        }
        event => match thread_terminal_outcome(event) {
            Some(ThreadOutcomeKind::Success) => {
                done(vec![Emit::Text(format!("\n{event}"))], StreamOutcome::Done)
            }
            Some(ThreadOutcomeKind::Failure) => {
                // Persisted lifecycle events wrap the real payload under `payload`.
                let inner = data.get("payload").unwrap_or(data);
                let reason = failure_reason(inner);
                if reason.is_empty() {
                    done(
                        vec![Emit::Error(event.to_string())],
                        StreamOutcome::Failed(event.to_string()),
                    )
                } else {
                    let detail = format!("{event}: {reason}");
                    done(
                        vec![Emit::Error(detail.clone())],
                        StreamOutcome::Failed(detail),
                    )
                }
            }
            None => {
                // Best-effort: stream the assistant's text inline. Persisted events
                // wrap their payload under `payload`; fall back to top-level for
                // synthetic envelopes (stream_started/error already handled above).
                let inner = data.get("payload").unwrap_or(data);
                if let Some(text) = human_text(inner) {
                    return Interpreted {
                        fragment: Some(text),
                        emits: Vec::new(),
                        outcome: StreamOutcome::Continue,
                    };
                }
                let mut emits = Vec::new();
                if generic_lines {
                    // The event already carries a rich payload; surface it instead
                    // of dropping it. `payload_summary` reflects whatever fields are
                    // there generically — it names no event's fields, so a new event
                    // kind needs no change here (the vocabulary stays in the events,
                    // not this binary).
                    let summary = payload_summary(inner);
                    emits.push(Emit::Text(if summary.is_empty() {
                        format!("- {}", ev.event)
                    } else {
                        format!("- {}  {summary}", ev.event)
                    }));
                }
                if let Some(detail) = nonterminal_failure_detail(&ev.event, inner) {
                    emits.push(Emit::Error(format!("{} failure: {detail}", ev.event)));
                }
                done(emits, StreamOutcome::Continue)
            }
        },
    }
}

fn parse_data(ev: &SseEvent) -> Value {
    serde_json::from_str(&ev.data).unwrap_or(Value::Null)
}

/// Terminal output shared by the event callback and the timeline worker. The
/// assistant's text is written without a line break, so a line written while
/// the cursor is mid-line starts on a fresh one.
pub struct SharedOut {
    console: crate::tty::Console,
    mid_line: Mutex<bool>,
}

impl SharedOut {
    pub fn new(console: crate::tty::Console) -> Self {
        Self {
            console,
            mid_line: Mutex::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, bool> {
        self.mid_line
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn fragment(&self, text: &str) -> io::Result<()> {
        let mut mid_line = self.lock();
        self.console.stream_fragment(text)?;
        if !text.is_empty() {
            *mid_line = !text.ends_with('\n');
        }
        Ok(())
    }

    fn break_line(&self, mid_line: &mut bool) -> io::Result<()> {
        if std::mem::take(mid_line) {
            self.console.stream_fragment("\n")?;
        }
        Ok(())
    }

    fn emit(&self, emit: &Emit) -> io::Result<()> {
        let mut mid_line = self.lock();
        self.break_line(&mut mid_line)?;
        match emit {
            Emit::Text(text) => self.console.text(text),
            Emit::Error(detail) => self.console.error(&crate::tty::Diagnostic::error(detail)),
        }
    }

    fn lines(&self, lines: &[String]) -> io::Result<()> {
        let mut mid_line = self.lock();
        self.break_line(&mut mid_line)?;
        lines.iter().try_for_each(|line| self.console.text(line))
    }

    fn warning(&self, message: &str) -> io::Result<()> {
        let mut mid_line = self.lock();
        self.break_line(&mut mid_line)?;
        self.console
            .warning(&crate::tty::Diagnostic::warning(message))
    }
}

type FetchFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, String>> + Send + 'a>>;

/// Where timeline responses come from: `service:threads/timeline`, called with
/// `since_chain_seq` once a window is held.
pub trait TimelineSource: Send + Sync + 'static {
    fn fetch(&self, chain_root_id: &str, since: Option<i64>) -> FetchFuture<'_>;
}

/// One fetch-and-apply step. `Ok(None)` means the held window could not take
/// the upsert twice running.
async fn timeline_round(
    source: &dyn TimelineSource,
    chain_root_id: &str,
    view: &mut LiveView,
) -> Result<Option<Vec<LiveLine>>, String> {
    // An upsert the held window cannot absorb resets the view; one refetch
    // of the whole window follows.
    for _ in 0..2 {
        let response = source.fetch(chain_root_id, view.since()).await?;
        if let Applied::Lines(lines) = view.apply(response) {
            return Ok(Some(lines));
        }
    }
    Ok(None)
}

/// The worker behind a [`TimelineFeed`]: one round per wake-up. A wake-up that
/// arrives during a round is remembered once, so events that pile up while a
/// fetch is in flight cost one more fetch, never one each. After the first
/// failure it stops fetching and says so once.
async fn run_timeline_worker(
    source: Arc<dyn TimelineSource>,
    wake: Arc<Notify>,
    closing: Arc<AtomicBool>,
    chain_root: Arc<Mutex<Option<String>>>,
    mut print: impl FnMut(&[LiveLine]) -> io::Result<()>,
    mut warn: impl FnMut(&str) -> io::Result<()>,
) {
    let mut view = LiveView::default();
    let mut working = true;
    loop {
        wake.notified().await;
        let last = closing.load(Ordering::Acquire);
        let chain = chain_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if working && let Some(chain) = chain {
            let failure = match timeline_round(source.as_ref(), &chain, &mut view).await {
                Ok(Some(lines)) => print(&lines).err().map(|error| error.to_string()),
                Ok(None) => Some("the held window could not be brought current".to_string()),
                Err(error) => Some(error),
            };
            if let Some(failure) = failure {
                working = false;
                let _ = warn(&format!("timeline unavailable: {failure}"));
            }
        }
        if last {
            break;
        }
    }
}

/// Fetches and prints timeline updates for a running chain, off the event
/// callback so the stream is never held up by a fetch.
pub struct TimelineFeed {
    wake: Arc<Notify>,
    closing: Arc<AtomicBool>,
    chain_root: Arc<Mutex<Option<String>>>,
    worker: tokio::task::JoinHandle<()>,
}

impl TimelineFeed {
    pub fn spawn(source: Arc<dyn TimelineSource>, out: Arc<SharedOut>) -> Self {
        let capabilities = out.console.capabilities();
        let wake = Arc::new(Notify::new());
        let closing = Arc::new(AtomicBool::new(false));
        let chain_root = Arc::new(Mutex::new(None));
        let print_out = out.clone();
        let worker = tokio::spawn(run_timeline_worker(
            source,
            wake.clone(),
            closing.clone(),
            chain_root.clone(),
            move |lines| {
                let rendered: Vec<String> = lines
                    .iter()
                    .map(|line| render_live_line(line, capabilities.color, capabilities.unicode))
                    .collect();
                print_out.lines(&rendered)
            },
            move |message| out.warning(message),
        ));
        Self {
            wake,
            closing,
            chain_root,
            worker,
        }
    }

    /// A durable event landed on `chain_root_id`.
    fn kick(&self, chain_root_id: &str) {
        {
            let mut chain = self
                .chain_root
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if chain.as_deref() != Some(chain_root_id) {
                *chain = Some(chain_root_id.to_string());
            }
        }
        self.wake.notify_one();
    }

    /// Run one last round for everything that has landed, then stop.
    async fn finish(self) {
        self.closing.store(true, Ordering::Release);
        self.wake.notify_one();
        let _ = self.worker.await;
    }
}

/// The plain renderer for one streamed run: inline text, the terminal line,
/// and, when a feed is attached, the timeline between them.
pub struct PlainStream {
    out: Arc<SharedOut>,
    feed: Option<TimelineFeed>,
    /// The thread the stream opened on; its chain is the one to follow.
    thread_id: Option<String>,
    /// Output of the event that ended the run, held until the feed has printed
    /// everything that landed before it.
    deferred: Vec<Emit>,
}

impl PlainStream {
    pub fn new(console: crate::tty::Console) -> Self {
        Self {
            out: Arc::new(SharedOut::new(console)),
            feed: None,
            thread_id: None,
            deferred: Vec::new(),
        }
    }

    /// Follow the run's timeline from `source`. Must be called inside the
    /// tokio runtime that drives the stream.
    pub fn attach_timeline(&mut self, source: Arc<dyn TimelineSource>) {
        self.feed = Some(TimelineFeed::spawn(source, self.out.clone()));
    }

    pub fn event(&mut self, ev: &SseEvent) -> io::Result<StreamOutcome> {
        let data = parse_data(ev);
        if ev.event == "stream_started"
            && let Some(thread) = data.get("thread_id").and_then(Value::as_str)
        {
            self.thread_id = Some(thread.to_string());
        }
        let interpreted = interpret(ev, &data, self.feed.is_none());
        if let Some(text) = &interpreted.fragment {
            self.out.fragment(text)?;
        }
        // A durable event carries an id (its chain_seq); streamed deltas do not.
        if let Some(feed) = &self.feed
            && ev.id.is_some()
            && let Some(chain) = data
                .get("chain_root_id")
                .and_then(Value::as_str)
                .or(self.thread_id.as_deref())
        {
            feed.kick(chain);
        }
        let ends_run = !matches!(interpreted.outcome, StreamOutcome::Continue);
        for emit in interpreted.emits {
            if ends_run && self.feed.is_some() {
                self.deferred.push(emit);
            } else {
                self.out.emit(&emit)?;
            }
        }
        Ok(interpreted.outcome)
    }

    /// Let the feed print what landed before the run ended, then the lines the
    /// ending event held back.
    pub async fn finish(&mut self) -> io::Result<()> {
        if let Some(feed) = self.feed.take() {
            feed.finish().await;
        }
        for emit in std::mem::take(&mut self.deferred) {
            self.out.emit(&emit)?;
        }
        Ok(())
    }
}

/// Failure lifecycle events still receive the compact generic milestone, and
/// their actionable diagnostic is rendered separately as a complete line.
fn nonterminal_failure_detail(event: &str, payload: &Value) -> Option<String> {
    let status = payload.get("status").and_then(Value::as_str);
    if event != "graph_step_completed" || !matches!(status, Some("error" | "retry")) {
        return None;
    }
    match payload.get("error") {
        Some(Value::String(error)) if !error.is_empty() => Some(error.clone()),
        Some(error) if !error.is_null() => Some(error.to_string()),
        _ => None,
    }
}

/// Extract a short failure reason from a terminal event's (inner) payload.
fn failure_reason(payload: &Value) -> String {
    if let Some(s) = payload.get("error").and_then(|v| v.as_str()) {
        return s.to_string();
    }
    if let Some(obj) = payload.get("error")
        && !obj.is_null()
    {
        if let Some(summary) = obj.get("summary").and_then(Value::as_str) {
            let locator = obj.get("diagnostic_locator");
            let thread_id = locator
                .and_then(|locator| locator.get("thread_id"))
                .and_then(Value::as_str);
            return thread_id.map_or_else(
                || summary.to_string(),
                |thread_id| crate::thread_diagnostics::child_diagnostic(summary, thread_id),
            );
        }
        return obj.to_string();
    }
    payload
        .get("outcome_code")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Pull a human-readable text fragment out of a payload, if present. Best-effort
/// across common shapes (token/message deltas, content, output).
fn human_text(payload: &Value) -> Option<String> {
    for key in ["delta", "text", "content", "message", "output"] {
        if let Some(s) = payload.get(key).and_then(|v| v.as_str())
            && !s.is_empty()
        {
            return Some(s.to_string());
        }
    }
    // A recorded turn or input carries its text as parts.
    let joined: String = payload
        .get("parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    (!joined.is_empty()).then_some(joined)
}

/// A generic one-line summary of a payload's scalar fields: `key=value` pairs,
/// with one level of nesting flattened dotted (`cost.tokens=…`). The summary
/// remains one complete logical line; the terminal owns visual wrapping.
///
/// This carries NO per-event knowledge — it reflects whatever fields the
/// payload happens to hold, in the payload's own key order — so a new event
/// kind surfaces its detail with no change here. Arrays and deeper nesting are
/// skipped to keep the line honest and tight; the full event is always on the
/// wire for anything richer.
fn payload_summary(payload: &Value) -> String {
    let Some(obj) = payload.as_object() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in obj {
        match value {
            Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                push_field(&mut parts, key, value);
            }
            // One level of descent: flatten a nested object's own scalars dotted.
            Value::Object(sub) => {
                for (subkey, subval) in sub {
                    if matches!(subval, Value::String(_) | Value::Number(_) | Value::Bool(_)) {
                        push_field(&mut parts, &format!("{key}.{subkey}"), subval);
                    }
                }
            }
            // Arrays, null, deeper nesting: skipped (kept off the one-liner).
            _ => {}
        }
    }
    parts.join(" ")
}

/// Format one complete `key=value` field.
fn push_field(parts: &mut Vec<String>, key: &str, value: &Value) {
    let raw = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    parts.push(format!("{key}={raw}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: event.to_string(),
            data: data.to_string(),
            id: None,
        }
    }

    fn render(ev: &SseEvent) -> StreamOutcome {
        let console = crate::tty::Console::new(crate::tty::TerminalCapabilities::plain(80));
        PlainStream::new(console).event(ev).unwrap()
    }

    #[test]
    fn success_terminals_are_done() {
        assert!(matches!(
            render(&ev("thread_completed", "{}")),
            StreamOutcome::Done
        ));
        assert!(matches!(
            render(&ev("thread_continued", "{}")),
            StreamOutcome::Done
        ));
    }

    #[test]
    fn failure_terminals_and_stream_error_fail() {
        assert!(matches!(
            render(&ev("thread_failed", "{\"payload\":{\"error\":\"boom\"}}")),
            StreamOutcome::Failed(d) if d.contains("boom")
        ));
        assert!(matches!(
            render(&ev("thread_cancelled", "{}")),
            StreamOutcome::Failed(_)
        ));
        assert!(matches!(
            render(&ev("stream_error", "{\"code\":\"x\",\"error\":\"y\"}")),
            StreamOutcome::Failed(d) if d.contains("x")
        ));
    }

    #[test]
    fn structured_stream_errors_preserve_the_complete_body() {
        let tail = "diagnostic-tail-marker".repeat(24);
        let event = ev(
            "stream_error",
            &serde_json::json!({
                "code": "dispatch_failed",
                "error": {
                    "phase": "launch",
                    "retryable": false,
                    "detail": tail,
                }
            })
            .to_string(),
        );

        assert!(matches!(
            render(&event),
            StreamOutcome::Failed(detail)
                if detail.contains("dispatch_failed")
                    && detail.contains("launch")
                    && detail.contains(&tail)
        ));
    }

    #[test]
    fn non_terminal_events_continue() {
        assert!(matches!(
            render(&ev("stream_started", "{\"thread_id\":\"T-1\"}")),
            StreamOutcome::Continue
        ));
        assert!(matches!(
            render(&ev("cognition_out", "{\"delta\":\"hi\"}")),
            StreamOutcome::Continue
        ));
    }

    #[test]
    fn human_text_extracts_known_fields() {
        assert_eq!(
            human_text(&serde_json::json!({"delta": "hi"})),
            Some("hi".into())
        );
        assert_eq!(
            human_text(&serde_json::json!({"text": "yo"})),
            Some("yo".into())
        );
        assert_eq!(human_text(&serde_json::json!({"delta": ""})), None);
        assert_eq!(
            human_text(&serde_json::json!({
                "parts": [
                    {"type": "text", "text": "a"},
                    {"type": "tool_call", "call_id": "c", "name": "t", "arguments": {}},
                    {"type": "text", "text": "b"}
                ]
            })),
            Some("ab".to_string())
        );
        assert_eq!(human_text(&serde_json::json!({"other": 1})), None);
    }

    #[test]
    fn payload_summary_renders_scalars_and_one_level_nesting() {
        // Scalars become key=value in payload order; a nested object flattens
        // dotted; arrays/deeper nesting are skipped.
        let p = serde_json::json!({
            "call_id": "gr-eb7d9e3da2bc:30:aim",
            "step": 30,
            "ok": true,
            "cost": { "tokens": 812 },
            "items": [1, 2, 3],
            "effective_definition_digest": "1154fd1bf7f56dfe623e3ec8c0a6b5f12c561fad7a4398a4c40a328fcec67ac8"
        });
        let s = payload_summary(&p);
        assert!(s.contains("call_id=gr-eb7d9e3da2bc:30:aim"));
        assert!(s.contains("step=30"));
        assert!(s.contains("ok=true"));
        assert!(
            s.contains("cost.tokens=812"),
            "one level of nesting, dotted"
        );
        assert!(!s.contains("items="), "arrays are skipped");
        assert!(s.contains(
            "effective_definition_digest=1154fd1bf7f56dfe623e3ec8c0a6b5f12c561fad7a4398a4c40a328fcec67ac8"
        ));
        assert!(!s.contains('…'), "values are not presentation-truncated");
    }

    #[test]
    fn payload_summary_empty_for_non_object() {
        assert_eq!(payload_summary(&serde_json::json!("scalar")), "");
        assert_eq!(payload_summary(&serde_json::json!([1, 2])), "");
        assert_eq!(payload_summary(&serde_json::json!({})), "");
    }

    #[test]
    fn structured_event_renders_summary_not_bare_name() {
        // A tool_call_start with no human_text field surfaces its payload
        // fields generically instead of the bare `· event` line.
        assert!(matches!(
            render(&ev(
                "tool_call_start",
                "{\"payload\":{\"call_id\":\"abc\",\"tool\":\"bash\"}}"
            )),
            StreamOutcome::Continue
        ));
    }

    #[test]
    fn graph_step_failure_detail_is_not_field_truncated() {
        let detail = "child runtime failed: ".to_string() + &"diagnostic-tail-marker".repeat(24);
        let mut payload = serde_json::json!({
            "status": "error",
            "error": detail,
        });
        assert_eq!(
            nonterminal_failure_detail("graph_step_completed", &payload),
            payload
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        );
        assert!(
            nonterminal_failure_detail("graph_step_completed", &payload)
                .unwrap()
                .ends_with("diagnostic-tail-marker")
        );
        payload
            .as_object_mut()
            .unwrap()
            .insert("status".to_string(), serde_json::json!("retry"));
        assert!(
            nonterminal_failure_detail("graph_step_completed", &payload)
                .unwrap()
                .ends_with("diagnostic-tail-marker")
        );
        assert!(nonterminal_failure_detail("tool_call_result", &payload).is_none());
    }

    #[test]
    fn nested_payload_delta_extracted_by_render() {
        // cognition_out wraps the delta under `payload` (PersistedEventRecord);
        // render must treat it as a continue (text), not a `· event` line.
        assert!(matches!(
            render(&ev("cognition_out", "{\"payload\":{\"delta\":\"hi\"}}")),
            StreamOutcome::Continue
        ));
    }

    /// A source that serves scripted responses, gating each fetch on a permit
    /// and recording how many were in flight at once.
    struct ScriptedSource {
        responses: Mutex<std::collections::VecDeque<Result<Value, String>>>,
        sinces: Mutex<Vec<Option<i64>>>,
        gate: Option<tokio::sync::Semaphore>,
        in_flight: std::sync::atomic::AtomicUsize,
        most_in_flight: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedSource {
        fn new(responses: Vec<Result<Value, String>>, gated: bool) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
                sinces: Mutex::new(Vec::new()),
                gate: gated.then(|| tokio::sync::Semaphore::new(0)),
                in_flight: Default::default(),
                most_in_flight: Default::default(),
            })
        }

        fn fetches(&self) -> usize {
            self.sinces.lock().unwrap().len()
        }
    }

    impl TimelineSource for ScriptedSource {
        fn fetch(&self, _chain_root_id: &str, since: Option<i64>) -> FetchFuture<'_> {
            Box::pin(async move {
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.most_in_flight.fetch_max(now, Ordering::SeqCst);
                self.sinces.lock().unwrap().push(since);
                if let Some(gate) = &self.gate {
                    gate.acquire().await.unwrap().forget();
                }
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Ok(window(Vec::new(), 99, None)))
            })
        }
    }

    fn activity(state: &str, detail: Option<&str>) -> Value {
        serde_json::json!({
            "item": "entry", "id": "a", "type": "activity", "state": state, "tone": null,
            "thread": "T-1", "parent": null, "exchange": "x1", "counts_as": null,
            "slots": {"headline": "fetch", "detail": detail}, "seq": [1, 1], "ts": ["t", "t"]
        })
    }

    fn window(items: Vec<Value>, highest: i64, order: Option<Value>) -> Value {
        let mut response = serde_json::json!({
            "chain_root_id": "T-1", "older": 0, "highest_chain_seq": highest, "items": items,
        });
        if let Some(order) = order {
            response["order"] = order;
        }
        response
    }

    #[tokio::test]
    async fn a_state_change_prints_one_update_line_and_fetches_since_the_last_seq() {
        let source = ScriptedSource::new(
            vec![
                Ok(window(vec![activity("running", None)], 3, None)),
                Ok(window(
                    vec![activity("done", Some("3.2 KiB"))],
                    5,
                    Some(serde_json::json!(["a"])),
                )),
            ],
            false,
        );
        let mut view = LiveView::default();
        let first = timeline_round(source.as_ref(), "T-1", &mut view)
            .await
            .unwrap()
            .unwrap();
        let first: Vec<String> = first
            .iter()
            .map(|line| render_live_line(line, false, true))
            .collect();
        assert_eq!(first, ["fetch, running"]);
        let second = timeline_round(source.as_ref(), "T-1", &mut view)
            .await
            .unwrap()
            .unwrap();
        let second: Vec<String> = second
            .iter()
            .map(|line| render_live_line(line, false, true))
            .collect();
        assert_eq!(second, ["↻ fetch, done: 3.2 KiB"]);
        assert_eq!(*source.sinces.lock().unwrap(), vec![None, Some(3)]);
    }

    async fn until(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !done() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition reached");
    }

    #[tokio::test]
    async fn events_that_arrive_during_a_fetch_share_the_next_one() {
        let source = ScriptedSource::new(Vec::new(), true);
        let wake = Arc::new(Notify::new());
        let closing = Arc::new(AtomicBool::new(false));
        let chain = Arc::new(Mutex::new(Some("T-1".to_string())));
        let worker = tokio::spawn(run_timeline_worker(
            source.clone(),
            wake.clone(),
            closing.clone(),
            chain,
            |_| Ok(()),
            |_| Ok(()),
        ));
        // Five events: one starts a fetch, the other four land while it is
        // in flight.
        wake.notify_one();
        until(|| source.fetches() == 1).await;
        for _ in 0..4 {
            wake.notify_one();
        }
        source.gate.as_ref().unwrap().add_permits(1);
        until(|| source.fetches() == 2).await;
        source.gate.as_ref().unwrap().add_permits(1);
        // The run ends: one last fetch, then the worker stops.
        closing.store(true, Ordering::Release);
        wake.notify_one();
        until(|| source.fetches() == 3).await;
        source.gate.as_ref().unwrap().add_permits(1);
        worker.await.unwrap();
        assert_eq!(source.fetches(), 3, "four coalesced events cost one fetch");
        assert_eq!(source.most_in_flight.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_failed_fetch_is_reported_once_and_ends_fetching() {
        let source = ScriptedSource::new(vec![Err("not found".to_string())], false);
        let wake = Arc::new(Notify::new());
        let closing = Arc::new(AtomicBool::new(false));
        let chain = Arc::new(Mutex::new(Some("T-1".to_string())));
        let warnings = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = warnings.clone();
        let worker = tokio::spawn(run_timeline_worker(
            source.clone(),
            wake.clone(),
            closing.clone(),
            chain,
            |_| Ok(()),
            move |message| {
                sink.lock().unwrap().push(message.to_string());
                Ok(())
            },
        ));
        wake.notify_one();
        until(|| !warnings.lock().unwrap().is_empty()).await;
        wake.notify_one();
        closing.store(true, Ordering::Release);
        wake.notify_one();
        worker.await.unwrap();
        assert_eq!(source.fetches(), 1);
        assert_eq!(
            *warnings.lock().unwrap(),
            vec!["timeline unavailable: not found".to_string()]
        );
    }
}
