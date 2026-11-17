//! Script format for the chat-completions stub.
//!
//! A script is a YAML (or JSON, which YAML subsumes) document holding an
//! ordered list of steps. The server pops one step per incoming request;
//! a step with `repeat: N` serves N consecutive requests before the next
//! step begins. When every step is spent, further requests get HTTP 500.
//!
//! ```yaml
//! steps:
//!   - repeat: 2000                 # serve this step 2000 times (default 1)
//!     tool_calls:                  # parallel calls are just several entries
//!       - name: noop               # id defaults to call_<request>_<position>
//!         arguments: {}            # object/array/string; objects are serialised
//!   - text: "done"                 # finish_reason defaults to stop
//!     usage: { prompt_tokens: 120, completion_tokens: 3 }
//!   - fail: { status: 529, body: "overloaded" }
//!   - delay_ms: 1500000            # silent for 25 minutes before the first byte
//!     text: "late"
//!   - text: "cut"                  # headers and two chunks, then the connection drops
//!     cut_after_chunks: 2
//!   - text: "stalled"              # headers, then nothing for 60 s, then the rest
//!     stall: { after_chunks: 0, ms: 60000 }
//! ```
//!
//! Step fields (all optional):
//!
//! - `repeat`: how many consecutive requests this step serves.
//! - `delay_ms`: wait before any response byte (applies to every response
//!   shape, including `fail`).
//! - `fail`: `{status, body}`; the response is that HTTP status with that
//!   body and nothing else in the step is used.
//! - `text`, `reasoning`: assistant text and reasoning text.
//! - `tool_calls`: list of `{id?, name, arguments}`.
//! - `finish_reason`: `stop`, `tool_calls` or `length` (any string is
//!   passed through). Defaults to `tool_calls` when calls are present,
//!   otherwise `stop`.
//! - `usage`: `{prompt_tokens, completion_tokens, ...}` (defaults 10 and
//!   5); extra keys are copied into the usage object verbatim.
//! - `stream`: how a streamed reply is cut into SSE events:
//!   `text_chunk_bytes`, `argument_chunk_bytes` (split text and tool-call
//!   arguments over several events), `tool_call_index: false` (omit the
//!   per-call `index` field), `tool_call_ids: false` (omit ids from
//!   deltas), `usage_chunk: false` (omit usage from the final event),
//!   `done_marker: false` (omit the `[DONE]` sentinel).
//! - `cut_after_chunks`: streamed replies only; the connection is aborted
//!   after that many data events (0 means right after the headers).
//! - `stall`: streamed replies only; `{after_chunks, ms}` pauses the body
//!   for `ms` milliseconds once `after_chunks` events were sent (0 means
//!   straight after the headers), then resumes.
//!
//! In `text`, tool-call `id` and string `arguments`, the token `{n}` is
//! replaced by the zero-based request ordinal, so repeated steps yield
//! distinct ids.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default = "one")]
    pub repeat: u32,
    #[serde(default)]
    pub delay_ms: u64,
    #[serde(default)]
    pub fail: Option<Failure>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub finish_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub stream: StreamShape,
    #[serde(default)]
    pub cut_after_chunks: Option<usize>,
    #[serde(default)]
    pub stall: Option<Stall>,
}

impl Default for Step {
    fn default() -> Self {
        Self {
            repeat: 1,
            delay_ms: 0,
            fail: None,
            text: None,
            reasoning: None,
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
            stream: StreamShape::default(),
            cut_after_chunks: None,
            stall: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub status: u16,
    #[serde(default)]
    pub body: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    /// A string is sent as is; any other JSON value is serialised.
    #[serde(default = "empty_object")]
    pub arguments: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Usage {
    #[serde(default = "ten")]
    pub prompt_tokens: u64,
    #[serde(default = "five")]
    pub completion_tokens: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Usage {
    fn default() -> Self {
        Self {
            prompt_tokens: 10,
            completion_tokens: 5,
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StreamShape {
    #[serde(default)]
    pub text_chunk_bytes: Option<usize>,
    #[serde(default)]
    pub argument_chunk_bytes: Option<usize>,
    #[serde(default = "yes")]
    pub tool_call_index: bool,
    #[serde(default = "yes")]
    pub tool_call_ids: bool,
    #[serde(default = "yes")]
    pub usage_chunk: bool,
    #[serde(default = "yes")]
    pub done_marker: bool,
}

impl Default for StreamShape {
    fn default() -> Self {
        Self {
            text_chunk_bytes: None,
            argument_chunk_bytes: None,
            tool_call_index: true,
            tool_call_ids: true,
            usage_chunk: true,
            done_marker: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Stall {
    #[serde(default)]
    pub after_chunks: usize,
    pub ms: u64,
}

fn one() -> u32 {
    1
}
fn ten() -> u64 {
    10
}
fn five() -> u64 {
    5
}
fn yes() -> bool {
    true
}
fn empty_object() -> Value {
    Value::Object(Map::new())
}

impl Script {
    pub fn from_yaml_str(text: &str) -> anyhow::Result<Self> {
        Ok(serde_yaml::from_str(text)?)
    }

    pub fn from_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read script {}: {e}", path.display()))?;
        Self::from_yaml_str(&text)
            .map_err(|e| anyhow::anyhow!("parse script {}: {e}", path.display()))
    }
}
