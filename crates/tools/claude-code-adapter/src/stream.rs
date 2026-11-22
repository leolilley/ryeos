//! Claude Code's `stream-json` output, reduced to what the adapter reports.
//!
//! `claude -p --output-format stream-json --verbose` prints one JSON event per
//! line. The parser keeps only four things from the stream: the latest
//! top-level assistant text, the per-message usage snapshots, the `result`
//! event and the session id. Everything else is dropped, because the profile
//! maps nothing else. This module does no I/O, so every behaviour in it is
//! tested from recorded lines.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use ryeos_accounting::UsdNanos;
use serde_json::{Map, Value, json};

/// The longest result text a turn reports.
pub const MAX_RESULT_TEXT_BYTES: usize = 128 * 1024;
/// The longest structured output a turn reports. A larger one fails the turn
/// rather than being cut, because a cut structured result is wrong data that
/// still parses.
pub const MAX_STRUCTURED_OUTPUT_BYTES: usize = 64 * 1024;
/// The profile bounds the whole completion payload at 262144 bytes; stay
/// under it with room for the envelope.
const MAX_RESULT_BYTES: usize = 250_000;
/// One progress message, text included.
pub const MAX_PROGRESS_MESSAGE_BYTES: usize = 1024;
/// Progress notifications per turn.
pub const MAX_PROGRESS_EVENTS: u64 = 1_000;
/// The least time between two progress notifications.
pub const MIN_PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
const MIN_REDACTED_VALUE_BYTES: usize = 4;
const REDACTED: &str = "[redacted]";

/// Strips the host-provided values the adapter holds from everything it emits.
#[derive(Clone, Debug, Default)]
pub struct Redactor {
    values: Vec<String>,
}

impl Redactor {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut values: Vec<String> = values
            .into_iter()
            .filter(|value| value.len() >= MIN_REDACTED_VALUE_BYTES)
            .collect();
        // Longest first, so a value that contains another is replaced whole.
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        Self { values }
    }

    pub fn text(&self, text: &str) -> String {
        let mut redacted = text.to_owned();
        for value in &self.values {
            redacted = redacted.replace(value.as_str(), REDACTED);
        }
        redacted
    }

    pub fn value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(text)),
            Value::Array(values) => Value::Array(values.iter().map(|v| self.value(v)).collect()),
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| (self.text(key), self.value(value)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

pub fn truncate_to_boundary(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// How the process ended, as far as the adapter can tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// Claude Code exited on its own, with this code (`None`: by a signal).
    Exited(Option<i32>),
    /// The bridge asked for the turn to stop.
    Interrupted,
    /// The adapter is closing.
    Shutdown,
    /// The turn's deadline passed.
    Deadline,
    /// Claude Code could not be started.
    NotStarted(String),
}

/// A progress notification the parser decided to emit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub sequence: u64,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    input: u64,
    output: u64,
}

impl Counts {
    /// Input includes cache reads and writes, as the reference counts it.
    fn from_message_usage(usage: &Value) -> Self {
        let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
        Self {
            input: field("input_tokens")
                + field("cache_read_input_tokens")
                + field("cache_creation_input_tokens"),
            output: field("output_tokens"),
        }
    }

    fn from_model_usage(usage: &Value) -> Self {
        let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
        Self {
            input: field("inputTokens")
                + field("cacheReadInputTokens")
                + field("cacheCreationInputTokens"),
            output: field("outputTokens"),
        }
    }
}

pub struct StreamParser {
    declared_model: Option<String>,
    redactor: Redactor,
    claude_session_id: Option<String>,
    latest_text: String,
    called_models: BTreeSet<String>,
    /// Latest usage snapshot per API message id.
    messages: BTreeMap<String, (String, Counts)>,
    result: Option<Value>,
    progress_sent: u64,
    last_progress: Option<Instant>,
    ignored_lines: u64,
}

impl StreamParser {
    pub fn new(declared_model: Option<String>, redactor: Redactor) -> Self {
        Self {
            declared_model,
            redactor,
            claude_session_id: None,
            latest_text: String::new(),
            called_models: BTreeSet::new(),
            messages: BTreeMap::new(),
            result: None,
            progress_sent: 0,
            last_progress: None,
            ignored_lines: 0,
        }
    }

    #[cfg(test)]
    pub fn claude_session_id(&self) -> Option<&str> {
        self.claude_session_id.as_deref()
    }

    #[cfg(test)]
    pub fn ignored_lines(&self) -> u64 {
        self.ignored_lines
    }

    /// Feed one output line. Returns a progress notification when the line
    /// carries something worth reporting and the throttle admits it.
    pub fn on_line(&mut self, line: &[u8], now: Instant) -> Option<Progress> {
        let event = match serde_json::from_slice::<Value>(line) {
            Ok(event) if event.is_object() => event,
            _ => {
                self.ignored_lines += 1;
                return None;
            }
        };
        if let Some(id) = event.get("session_id").and_then(Value::as_str) {
            self.claude_session_id = Some(id.to_owned());
        }
        match event.get("type").and_then(Value::as_str) {
            Some("assistant") => self.on_assistant(&event, now),
            Some("result") => {
                self.result = Some(event);
                None
            }
            _ => None,
        }
    }

    fn on_assistant(&mut self, event: &Value, now: Instant) -> Option<Progress> {
        let message = event.get("message")?;
        let top_level = event.get("parent_tool_use_id").is_none_or(Value::is_null);
        if let Some(id) = message.get("id").and_then(Value::as_str) {
            let model = message
                .get("model")
                .and_then(Value::as_str)
                .or(self.declared_model.as_deref())
                .unwrap_or("unknown")
                .to_owned();
            if message.get("model").and_then(Value::as_str).is_some() {
                self.called_models.insert(model.clone());
            }
            // One API message arrives as several events, each carrying the
            // usage so far: keep the latest.
            let counts = Counts::from_message_usage(message.get("usage").unwrap_or(&Value::Null));
            self.messages.insert(id.to_owned(), (model, counts));
        }
        let mut note = None;
        for block in message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match block.get("type").and_then(Value::as_str) {
                Some("text") if top_level => {
                    let text = block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if !text.is_empty() {
                        self.latest_text = text.to_owned();
                        note = Some(text.to_owned());
                    }
                }
                Some("tool_use") => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                    note = Some(format!("tool: {name}"));
                }
                _ => {}
            }
        }
        self.admit_progress(note?, now)
    }

    fn admit_progress(&mut self, note: String, now: Instant) -> Option<Progress> {
        if self.progress_sent >= MAX_PROGRESS_EVENTS {
            return None;
        }
        if self
            .last_progress
            .is_some_and(|last| now.duration_since(last) < MIN_PROGRESS_INTERVAL)
        {
            return None;
        }
        self.progress_sent += 1;
        self.last_progress = Some(now);
        let (message, _) =
            truncate_to_boundary(&self.redactor.text(&note), MAX_PROGRESS_MESSAGE_BYTES);
        Some(Progress {
            sequence: self.progress_sent,
            message,
        })
    }

    /// The turn's final status and `result` payload. `stderr_tail` is the
    /// last of Claude Code's own error output, used only when it crashed.
    pub fn complete(&self, end: &End, stderr_tail: &str) -> (&'static str, Value) {
        let result = self.result.as_ref();
        let result_text = result
            .and_then(|event| event.get("result"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let errored = result.is_some_and(|event| {
            event.get("is_error").and_then(Value::as_bool) == Some(true)
                || event
                    .get("subtype")
                    .and_then(Value::as_str)
                    .is_some_and(|subtype| subtype != "success")
        });
        let (mut status, mut reason): (&'static str, String) = match (result, end) {
            (Some(event), _) if errored => (
                "failed",
                event
                    .get("subtype")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_owned(),
            ),
            (Some(_), _) => ("completed", "result".to_owned()),
            (None, End::Interrupted) => ("interrupted", "interrupt".to_owned()),
            (None, End::Shutdown) => ("interrupted", "shutdown".to_owned()),
            (None, End::Deadline) => ("failed", "deadline".to_owned()),
            (None, End::NotStarted(_)) => ("failed", "not_started".to_owned()),
            (None, End::Exited(_)) => ("crashed", "crash".to_owned()),
        };
        let mut text = result_text
            .map(str::to_owned)
            .filter(|_| result.is_some())
            .unwrap_or_else(|| self.latest_text.clone());
        if text.is_empty() {
            text = match (status, end) {
                (_, End::NotStarted(why)) => format!("claude code did not start: {why}"),
                ("crashed", End::Exited(code)) => {
                    let code = code.map_or("a signal".to_owned(), |code| format!("code {code}"));
                    let tail = stderr_tail.trim();
                    if tail.is_empty() {
                        format!("claude code exited with {code} before reporting a result")
                    } else {
                        format!("claude code exited with {code} before reporting a result: {tail}")
                    }
                }
                ("interrupted", _) => "turn interrupted".to_owned(),
                ("failed", _) if reason == "deadline" => "turn deadline reached".to_owned(),
                ("failed", _) => format!("claude code reported {reason}"),
                _ => "turn completed".to_owned(),
            };
        }
        let mut structured_output = result
            .and_then(|event| event.get("structured_output"))
            .map_or(Value::Null, |value| self.redactor.value(value));
        let structured_bytes =
            serde_json::to_vec(&structured_output).map_or(usize::MAX, |bytes| bytes.len());
        if structured_bytes > MAX_STRUCTURED_OUTPUT_BYTES {
            status = "failed";
            reason = "structured_output_too_large".to_owned();
            structured_output = Value::Null;
            text = format!("structured output exceeds {MAX_STRUCTURED_OUTPUT_BYTES} bytes");
        }
        let (usage, usage_source, by_model) = self.usage();
        let mut limit = MAX_RESULT_TEXT_BYTES;
        loop {
            let (shown, truncated) = truncate_to_boundary(&self.redactor.text(&text), limit);
            let payload = json!({
                "text": shown,
                "truncated": truncated,
                "structured_output": structured_output,
                "usage": usage,
                "usage_source": usage_source,
                "usage_by_model": by_model,
                "reason": reason,
                "models": {
                    "declared": self.declared_model,
                    "called": self.called_models.iter().collect::<Vec<_>>(),
                },
            });
            let encoded = serde_json::to_vec(&payload).map_or(usize::MAX, |bytes| bytes.len());
            if encoded <= MAX_RESULT_BYTES || limit < 1024 {
                return (status, payload);
            }
            // Escapes can make the encoded text much longer than the text.
            limit /= 2;
        }
    }

    /// Usage from the `result` event when there is one; otherwise the
    /// per-message snapshots, whose output counts are a lower bound and whose
    /// cost is unknown. An unknown cost reports `cost_micro_usd: null` with
    /// `cost_status: "unknown"`, never zero: a truncated turn must not look
    /// free to anything that totals spend.
    fn usage(&self) -> (Value, &'static str, Value) {
        let mut by_model = Map::new();
        let (counts, cost, source) = match self.result.as_ref() {
            Some(event)
                if event.get("modelUsage").is_some_and(|usage| {
                    usage.as_object().is_some_and(|usage| !usage.is_empty())
                }) =>
            {
                let mut total = Counts::default();
                for (model, usage) in event["modelUsage"].as_object().into_iter().flatten() {
                    let counts = Counts::from_model_usage(usage);
                    total.input += counts.input;
                    total.output += counts.output;
                    by_model.insert(
                        model.clone(),
                        json!({"input_tokens": counts.input, "output_tokens": counts.output}),
                    );
                }
                (total, cost_micro_usd(event), "result")
            }
            other => {
                let mut total = Counts::default();
                for (model, counts) in self.messages.values() {
                    total.input += counts.input;
                    total.output += counts.output;
                    let entry = by_model
                        .entry(model.clone())
                        .or_insert_with(|| json!({"input_tokens": 0, "output_tokens": 0}));
                    entry["input_tokens"] =
                        json!(entry["input_tokens"].as_u64().unwrap_or(0) + counts.input);
                    entry["output_tokens"] =
                        json!(entry["output_tokens"].as_u64().unwrap_or(0) + counts.output);
                }
                (total, other.and_then(cost_micro_usd), "messages")
            }
        };
        (
            json!({
                "input_tokens": counts.input,
                "output_tokens": counts.output,
                "cost_micro_usd": cost,
                "cost_status": if cost.is_some() { "known" } else { "unknown" },
            }),
            source,
            Value::Object(by_model),
        )
    }
}

/// The result event's reported cost in whole micro-dollars, rounded up so a
/// fractional micro-dollar is never dropped. `None` when the event reports no
/// usable figure (absent, not a number, negative, non-finite or beyond the
/// money range); a reported `0` is a known zero. The float enters the exact
/// domain through the accounting crate's single round-up quantizer, the same
/// one the HTTP provider path uses.
fn cost_micro_usd(result: &Value) -> Option<u64> {
    let reported = result.get("total_cost_usd").and_then(Value::as_f64)?;
    let (nanos, _) = UsdNanos::quantize_reported_f64_round_up(reported).ok()?;
    u64::try_from(nanos.as_nanos())
        .ok()
        .map(|nanos| nanos.div_ceil(1_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"0b6f3c1e-5d0a-4c55-9a52-3d7a0e3f6a11","model":"claude-opus-5-5","tools":["Agent"],"mcp_servers":[{"name":"task","status":"connected"}]}"#;
    const FIRST_PARTIAL: &str = r#"{"type":"assistant","message":{"id":"msg_01","model":"claude-opus-5-5","content":[{"type":"text","text":"Reading the instruction."}],"usage":{"input_tokens":12,"cache_read_input_tokens":100,"cache_creation_input_tokens":0,"output_tokens":1}},"parent_tool_use_id":null,"session_id":"0b6f3c1e-5d0a-4c55-9a52-3d7a0e3f6a11"}"#;
    const FIRST_FINAL: &str = r#"{"type":"assistant","message":{"id":"msg_01","model":"claude-opus-5-5","content":[{"type":"tool_use","id":"toolu_1","name":"mcp__task__bash","input":{"command":"ls"}}],"usage":{"input_tokens":12,"cache_read_input_tokens":100,"cache_creation_input_tokens":0,"output_tokens":40}},"parent_tool_use_id":null}"#;
    const SUBAGENT: &str = r#"{"type":"assistant","message":{"id":"msg_sub","model":"claude-haiku-5","content":[{"type":"text","text":"subagent chatter"}],"usage":{"input_tokens":3,"output_tokens":5}},"parent_tool_use_id":"toolu_9"}"#;
    const ANSWER: &str = r#"{"type":"assistant","message":{"id":"msg_02","model":"claude-opus-5-5","content":[{"type":"text","text":"Done: wrote /work/out.txt"}],"usage":{"input_tokens":20,"cache_read_input_tokens":150,"output_tokens":9}},"parent_tool_use_id":null}"#;
    const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"Done: wrote /work/out.txt (summary)","session_id":"0b6f3c1e-5d0a-4c55-9a52-3d7a0e3f6a11","total_cost_usd":0.0421,"structured_output":{"files":["out.txt"]},"modelUsage":{"claude-opus-5-5":{"inputTokens":32,"outputTokens":410,"cacheReadInputTokens":250,"cacheCreationInputTokens":30,"costUSD":0.0421}}}"#;

    fn parser() -> StreamParser {
        StreamParser::new(
            Some("claude-opus-5-5".to_owned()),
            Redactor::new(["sk-secret-key-1234".to_owned()]),
        )
    }

    fn feed(parser: &mut StreamParser, lines: &[&str]) -> Vec<Progress> {
        // One second apart, so the throttle admits each.
        let start = Instant::now();
        lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                parser.on_line(
                    line.as_bytes(),
                    start + Duration::from_secs(2 * index as u64),
                )
            })
            .collect()
    }

    #[test]
    fn a_result_event_gives_text_cost_usage_and_structure() {
        let mut parser = parser();
        feed(
            &mut parser,
            &[INIT, FIRST_PARTIAL, FIRST_FINAL, ANSWER, RESULT],
        );
        assert_eq!(
            parser.claude_session_id(),
            Some("0b6f3c1e-5d0a-4c55-9a52-3d7a0e3f6a11")
        );
        let (status, result) = parser.complete(&End::Exited(Some(0)), "");
        assert_eq!(status, "completed");
        assert_eq!(result["text"], "Done: wrote /work/out.txt (summary)");
        assert_eq!(result["structured_output"], json!({"files": ["out.txt"]}));
        assert_eq!(result["usage"]["input_tokens"], 32 + 250 + 30);
        assert_eq!(result["usage"]["output_tokens"], 410);
        assert_eq!(result["usage"]["cost_micro_usd"], 42_100);
        assert_eq!(result["usage"]["cost_status"], "known");
        assert_eq!(result["usage_source"], "result");
        assert_eq!(result["models"]["declared"], "claude-opus-5-5");
        assert_eq!(result["models"]["called"], json!(["claude-opus-5-5"]));
    }

    #[test]
    fn a_missing_result_falls_back_to_the_latest_snapshot_of_each_message() {
        let mut parser = parser();
        feed(
            &mut parser,
            &[INIT, FIRST_PARTIAL, FIRST_FINAL, SUBAGENT, ANSWER],
        );
        let (status, result) = parser.complete(&End::Exited(Some(0)), "");
        // No result event: even a clean exit is not a completed turn.
        assert_eq!(status, "crashed");
        // msg_01 is counted once, at its last snapshot; the subagent counts too.
        assert_eq!(result["usage"]["input_tokens"], (12 + 100) + 3 + (20 + 150));
        assert_eq!(result["usage"]["output_tokens"], 40 + 5 + 9);
        assert_eq!(result["usage"]["cost_micro_usd"], Value::Null);
        assert_eq!(result["usage"]["cost_status"], "unknown");
        assert_eq!(result["usage_source"], "messages");
        // The subagent's text is not the final message.
        assert_eq!(result["text"], "Done: wrote /work/out.txt");
        assert_eq!(
            result["usage_by_model"]["claude-opus-5-5"]["output_tokens"],
            49
        );
    }

    #[test]
    fn a_crash_mid_stream_reports_crashed_with_the_stderr_tail() {
        let mut parser = parser();
        feed(&mut parser, &[INIT, FIRST_PARTIAL]);
        // A torn last line, as a killed process leaves.
        assert!(
            parser
                .on_line(br#"{"type":"assistant","mess"#, Instant::now())
                .is_none()
        );
        assert_eq!(parser.ignored_lines(), 1);
        let (status, result) = parser.complete(&End::Exited(None), "Segmentation fault");
        assert_eq!(status, "crashed");
        assert_eq!(result["reason"], "crash");
        // The latest message survives as the final text.
        assert_eq!(result["text"], "Reading the instruction.");
        let (_, nothing) =
            StreamParser::new(None, Redactor::default()).complete(&End::Exited(Some(137)), "oom");
        assert!(nothing["text"].as_str().unwrap().contains("code 137"));
        assert!(nothing["text"].as_str().unwrap().contains("oom"));
    }

    #[test]
    fn interrupt_shutdown_deadline_and_start_failure_each_have_a_status() {
        let parser = parser();
        assert_eq!(parser.complete(&End::Interrupted, "").0, "interrupted");
        let (status, result) = parser.complete(&End::Shutdown, "");
        assert_eq!(
            (status, result["reason"].as_str()),
            ("interrupted", Some("shutdown"))
        );
        let (status, result) = parser.complete(&End::Deadline, "");
        assert_eq!(
            (status, result["reason"].as_str()),
            ("failed", Some("deadline"))
        );
        let (status, result) = parser.complete(&End::NotStarted("no such file".into()), "");
        assert_eq!(status, "failed");
        assert!(result["text"].as_str().unwrap().contains("no such file"));
    }

    #[test]
    fn an_error_result_fails_the_turn_with_claude_codes_reason() {
        let mut parser = parser();
        feed(
            &mut parser,
            &[
                r#"{"type":"result","subtype":"error_max_budget_usd","is_error":true,"total_cost_usd":1.5,"modelUsage":{}}"#,
            ],
        );
        let (status, result) = parser.complete(&End::Exited(Some(1)), "");
        assert_eq!(
            (status, result["reason"].as_str()),
            ("failed", Some("error_max_budget_usd"))
        );
        assert_eq!(result["usage"]["cost_micro_usd"], 1_500_000);
    }

    #[test]
    fn oversize_text_is_truncated_and_oversize_structure_fails_the_turn() {
        let big = "é".repeat(MAX_RESULT_TEXT_BYTES);
        let line = json!({"type":"result","subtype":"success","result":big,"total_cost_usd":0})
            .to_string();
        let mut parser = parser();
        feed(&mut parser, &[&line]);
        let (status, result) = parser.complete(&End::Exited(Some(0)), "");
        assert_eq!(status, "completed");
        assert_eq!(result["truncated"], true);
        let text = result["text"].as_str().unwrap();
        assert!(text.len() <= MAX_RESULT_TEXT_BYTES);
        assert!(text.chars().all(|c| c == 'é'));

        let line = json!({"type":"result","subtype":"success","result":"x",
            "structured_output":{"blob":"y".repeat(MAX_STRUCTURED_OUTPUT_BYTES)}})
        .to_string();
        let mut parser = StreamParser::new(None, Redactor::default());
        feed(&mut parser, &[&line]);
        let (status, result) = parser.complete(&End::Exited(Some(0)), "");
        assert_eq!(status, "failed");
        assert_eq!(result["reason"], "structured_output_too_large");
        assert_eq!(result["structured_output"], Value::Null);
    }

    #[test]
    fn escape_heavy_text_still_fits_the_payload_bound() {
        let nasty = "\u{1}".repeat(MAX_RESULT_TEXT_BYTES);
        let line = json!({"type":"result","subtype":"success","result":nasty}).to_string();
        let mut parser = StreamParser::new(None, Redactor::default());
        feed(&mut parser, &[&line]);
        let (_, result) = parser.complete(&End::Exited(Some(0)), "");
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_RESULT_BYTES);
        assert_eq!(result["truncated"], true);
    }

    #[test]
    fn a_delivered_value_never_reaches_text_structure_progress_or_keys() {
        let secret = "sk-secret-key-1234";
        let mut parser = parser();
        let progress = feed(
            &mut parser,
            &[&json!({"type":"assistant","message":{"id":"m","content":[
                {"type":"text","text":format!("key is {secret}")}]}})
            .to_string()],
        );
        assert_eq!(progress[0].message, "key is [redacted]");
        let line = json!({"type":"result","subtype":"success",
            "result":format!("used {secret} ok"),
            "structured_output":{secret:[format!("{secret}!")]}})
        .to_string();
        feed(&mut parser, &[&line]);
        let (_, result) = parser.complete(&End::Exited(Some(0)), secret);
        assert!(!result.to_string().contains(secret));
        assert_eq!(result["text"], "used [redacted] ok");
        // Values too short to be secrets are left alone.
        assert_eq!(Redactor::new(["ab".to_owned()]).text("abab"), "abab");
    }

    #[test]
    fn progress_is_throttled_counted_and_bounded() {
        let mut parser = parser();
        let start = Instant::now();
        let line = |text: &str| {
            json!({"type":"assistant","message":{"id":"m","content":[
                {"type":"text","text":text}]}})
            .to_string()
        };
        let first = parser.on_line(line("a").as_bytes(), start).unwrap();
        assert_eq!(first.sequence, 1);
        assert!(
            parser
                .on_line(line("b").as_bytes(), start + Duration::from_millis(200))
                .is_none()
        );
        let long = "z".repeat(5000);
        let second = parser
            .on_line(line(&long).as_bytes(), start + Duration::from_secs(2))
            .unwrap();
        assert_eq!(second.sequence, 2);
        assert_eq!(second.message.len(), MAX_PROGRESS_MESSAGE_BYTES);
        // Throttled text is still the latest final-message candidate.
        parser.on_line(line("last").as_bytes(), start + Duration::from_secs(2));
        let (_, result) = parser.complete(&End::Interrupted, "");
        assert_eq!(result["text"], "last");
    }

    fn result_with_cost(cost: &str) -> Value {
        serde_json::from_str(&format!(
            r#"{{"type":"result","subtype":"success","total_cost_usd":{cost}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn cost_rounds_up_to_the_next_micro_dollar_and_exact_values_are_unchanged() {
        for (reported, micro) in [
            ("0.0421", Some(42_100)),
            ("0.5", Some(500_000)),
            ("0.000001", Some(1)),
            ("0.0000010000001", Some(2)),
            ("0.0000019999999", Some(2)),
            ("0.0000000001", Some(1)),
            ("1.0000001", Some(1_000_001)),
            ("0", Some(0)),
            ("0.0", Some(0)),
            ("-0.5", None),
            ("1e300", None),
        ] {
            assert_eq!(
                cost_micro_usd(&result_with_cost(reported)),
                micro,
                "{reported}"
            );
        }
        assert_eq!(cost_micro_usd(&json!({"type": "result"})), None);
        assert_eq!(
            cost_micro_usd(&json!({"total_cost_usd": "0.5"})),
            None,
            "a non-number is unknown"
        );
    }

    #[test]
    fn a_truncated_stream_reports_unknown_cost_not_zero() {
        let mut parser = parser();
        feed(&mut parser, &[INIT, FIRST_FINAL]);
        let (_, result) = parser.complete(&End::Exited(None), "");
        assert!(result["usage"]["cost_micro_usd"].is_null());
        assert_eq!(result["usage"]["cost_status"], "unknown");
        // The token counts are still a lower bound, reported as numbers.
        assert_eq!(result["usage"]["input_tokens"], 112);
    }

    #[test]
    fn a_result_event_without_a_cost_is_unknown_and_with_a_zero_cost_is_known() {
        for (event, status, micro) in [
            (
                r#"{"type":"result","subtype":"success","result":"x"}"#,
                "unknown",
                Value::Null,
            ),
            (
                r#"{"type":"result","subtype":"success","result":"x","total_cost_usd":0}"#,
                "known",
                json!(0),
            ),
        ] {
            let mut parser = parser();
            feed(&mut parser, &[INIT, FIRST_FINAL, event]);
            let (_, result) = parser.complete(&End::Exited(Some(0)), "");
            assert_eq!(result["usage"]["cost_status"], status);
            assert_eq!(result["usage"]["cost_micro_usd"], micro);
        }
    }
}
