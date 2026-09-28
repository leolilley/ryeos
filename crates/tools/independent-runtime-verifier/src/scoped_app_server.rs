//! Codex app-server conversation over one daemon-owned scoped attempt.
//!
//! The daemon owns launch, byte delivery and capture. This verifier owns the
//! Codex protocol interpretation. A completed conversation is not process,
//! whole-scope, provider, or frozen-workspace qualification evidence.

use anyhow::{Context as _, Result, ensure};
use ryeos_runtime::callback::CallbackError;
use ryeos_runtime::callback_uds::UdsRuntimeClient;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::routing_observation::{MAX_EVENT_BYTES, MAX_EVENTS, MAX_TOTAL_BYTES};

// This product-local, statically dispatched protocol is used only by the
// verifier binary and its fixtures; it promises no Send or object-safe API.
#[allow(async_fn_in_trait)]
pub trait AppServerTransport {
    async fn write_frame(&mut self, frame: &[u8]) -> Result<()>;
    async fn read_chunk(&mut self) -> Result<Vec<u8>>;
    async fn close_input(&mut self) -> Result<()>;
}

/// Exact attempts and owner callback authority are supplied by the enclosing
/// verifier. No executable, path or process coordinate comes from Codex.
pub struct ScopedAppServerTransport<'a> {
    client: &'a UdsRuntimeClient,
    root_thread_id: &'a str,
    attempt_id: &'a str,
    input_sequence: u32,
    stdout_offset: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryAck {
    schema: String,
    attempt_id: String,
    sequence: u32,
    kind: String,
    payload_digest: String,
    byte_count: u32,
    delivery: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureRead {
    schema: String,
    attempt_id: String,
    offset: u64,
    bytes_sha256: String,
    eof: bool,
    bytes: Vec<u8>,
}

impl<'a> ScopedAppServerTransport<'a> {
    pub fn new(client: &'a UdsRuntimeClient, root_thread_id: &'a str, attempt_id: &'a str) -> Self {
        Self {
            client,
            root_thread_id,
            attempt_id,
            input_sequence: 0,
            stdout_offset: 0,
        }
    }

    fn check_ack(&self, value: Value, kind: &str, digest: &str, byte_count: u32) -> Result<()> {
        let ack: DeliveryAck = serde_json::from_value(value)
            .context("scoped input returned a noncanonical delivery acknowledgment")?;
        ensure!(
            ack.schema == "ryeos.scoped_input_delivery.v1"
                && ack.attempt_id == self.attempt_id
                && ack.sequence == self.input_sequence
                && ack.kind == kind
                && ack.payload_digest == digest
                && ack.byte_count == byte_count
                && ack.delivery == "delivered",
            "scoped input acknowledgment differs from exact requested delivery"
        );
        Ok(())
    }
}

impl AppServerTransport for ScopedAppServerTransport<'_> {
    async fn write_frame(&mut self, frame: &[u8]) -> Result<()> {
        ensure!(
            !frame.is_empty() && frame.len() <= MAX_EVENT_BYTES,
            "scoped app-server input frame exceeds bound"
        );
        let digest = lillux::sha256_hex(frame);
        let count = u32::try_from(frame.len())?;
        let mut response = self
            .client
            .write_scoped_child(
                self.root_thread_id,
                self.attempt_id,
                self.input_sequence,
                frame,
            )
            .await;
        if matches!(response, Err(CallbackError::Transport(_))) {
            // The same exact coordinates can recover a delivered ACK. A
            // reserved operation refuses; never advance to another frame.
            response = self
                .client
                .write_scoped_child(
                    self.root_thread_id,
                    self.attempt_id,
                    self.input_sequence,
                    frame,
                )
                .await;
        }
        self.check_ack(response?, "write", &digest, count)?;
        self.input_sequence = self
            .input_sequence
            .checked_add(1)
            .context("scoped app-server input sequence exhausted")?;
        Ok(())
    }

    async fn read_chunk(&mut self) -> Result<Vec<u8>> {
        let mut response = self
            .client
            .read_scoped_child(
                self.root_thread_id,
                self.attempt_id,
                self.stdout_offset,
                4096,
            )
            .await;
        if matches!(response, Err(CallbackError::Transport(_))) {
            response = self
                .client
                .read_scoped_child(
                    self.root_thread_id,
                    self.attempt_id,
                    self.stdout_offset,
                    4096,
                )
                .await;
        }
        let read: CaptureRead = serde_json::from_value(response?)
            .context("scoped stdout returned a noncanonical capture slice")?;
        ensure!(
            read.schema == "ryeos.scoped_stdout_read.v1"
                && read.attempt_id == self.attempt_id
                && read.offset == self.stdout_offset
                && read.bytes.len() <= 4096
                && read.bytes_sha256 == lillux::sha256_hex(&read.bytes)
                && read.eof == read.bytes.is_empty(),
            "scoped stdout slice differs from exact capture offset"
        );
        self.stdout_offset = self
            .stdout_offset
            .checked_add(u64::try_from(read.bytes.len())?)
            .context("scoped stdout offset exhausted")?;
        Ok(read.bytes)
    }

    async fn close_input(&mut self) -> Result<()> {
        let mut response = self
            .client
            .close_scoped_child_input(self.root_thread_id, self.attempt_id, self.input_sequence)
            .await;
        if matches!(response, Err(CallbackError::Transport(_))) {
            response = self
                .client
                .close_scoped_child_input(self.root_thread_id, self.attempt_id, self.input_sequence)
                .await;
        }
        self.check_ack(response?, "close", &lillux::sha256_hex(b""), 0)
    }
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

pub struct ScopedAppServerConversation<T> {
    transport: T,
    unread: Vec<u8>,
    notifications: Vec<Value>,
    frames: usize,
    bytes: usize,
    phase: ScriptedPhase,
}

impl<T: AppServerTransport> ScopedAppServerConversation<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            unread: Vec::new(),
            notifications: Vec::new(),
            frames: 0,
            bytes: 0,
            phase: ScriptedPhase::Fresh,
        }
    }

    pub fn notifications(&self) -> &[Value] {
        &self.notifications
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        let mut frame = serde_json::to_vec(&message)?;
        frame.push(b'\n');
        ensure!(
            frame.len() <= MAX_EVENT_BYTES,
            "app-server request frame bound"
        );
        self.transport.write_frame(&frame).await
    }

    async fn next(&mut self) -> Result<Value> {
        ensure!(
            self.frames < MAX_EVENTS + 16,
            "app-server frame count bound"
        );
        while !self.unread.contains(&b'\n') {
            ensure!(
                self.unread.len() < MAX_EVENT_BYTES,
                "app-server frame exceeds bound"
            );
            let chunk = self.transport.read_chunk().await?;
            ensure!(!chunk.is_empty(), "app-server ended mid-protocol");
            self.unread.extend_from_slice(&chunk);
        }
        let end = self.unread.iter().position(|byte| *byte == b'\n').unwrap() + 1;
        ensure!(end <= MAX_EVENT_BYTES, "app-server frame exceeds bound");
        let frame: Vec<u8> = self.unread.drain(..end).collect();
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

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Result<Value> {
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let message = self.next().await?;
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

    async fn initialize_scripted(&mut self) -> Result<()> {
        ensure!(
            matches!(&self.phase, ScriptedPhase::Fresh),
            "scripted initialization out of order"
        );
        self.phase = ScriptedPhase::Failed;
        self.request(
            1,
            "initialize",
            json!({"clientInfo":{"name":"ryeos-routed-verifier","version":"1"},
                "capabilities":{"experimentalApi":true}}),
        )
        .await?;
        self.send(json!({"method":"initialized","params":{}}))
            .await?;
        self.phase = ScriptedPhase::Initialized;
        Ok(())
    }

    async fn start_scripted_thread(&mut self) -> Result<String> {
        ensure!(
            matches!(&self.phase, ScriptedPhase::Initialized),
            "scripted thread start out of order"
        );
        self.phase = ScriptedPhase::Failed;
        let response = self
            .request(
                2,
                "thread/start",
                json!({"cwd":"/workspace","experimentalRawEvents":true,
                "environments":[{"environmentId":"ryeos-external-candidate",
                    "cwd":"/workspace","runtimeWorkspaceRoots":["/workspace"]}]}),
            )
            .await?;
        let thread_id = response["thread"]["id"]
            .as_str()
            .context("scripted thread ID absent")?;
        ensure!(
            (1..=256).contains(&thread_id.len()) && !thread_id.chars().any(char::is_control),
            "scripted thread ID invalid"
        );
        let thread_id = thread_id.to_owned();
        self.phase = ScriptedPhase::ThreadStarted(thread_id.clone());
        Ok(thread_id)
    }

    async fn start_scripted_turn(&mut self, thread_id: &str) -> Result<String> {
        ensure!(
            matches!(&self.phase, ScriptedPhase::ThreadStarted(expected) if expected == thread_id),
            "scripted turn start out of order or changed thread identity"
        );
        self.phase = ScriptedPhase::Failed;
        ensure!(
            !self
                .notifications
                .iter()
                .any(|event| event["method"] == "turn/completed"),
            "scripted turn completed before start"
        );
        let notification_cursor = self.notifications.len();
        let response = self
            .request(
                3,
                "turn/start",
                json!({"threadId":thread_id,
                "input":[{"type":"text",
                    "text":"Perform the bounded scripted routing scenario.",
                    "text_elements":[]}]}),
            )
            .await?;
        let turn_id = response["turn"]["id"]
            .as_str()
            .context("scripted turn ID absent")?;
        ensure!(
            (1..=256).contains(&turn_id.len()) && !turn_id.chars().any(char::is_control),
            "scripted turn ID invalid"
        );
        let turn_id = turn_id.to_owned();
        self.phase = ScriptedPhase::TurnStarted {
            thread_id: thread_id.to_owned(),
            turn_id: turn_id.clone(),
            notification_cursor,
        };
        Ok(turn_id)
    }

    async fn await_scripted_turn_completed(
        &mut self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<()> {
        let cursor = match &self.phase {
            ScriptedPhase::TurnStarted {
                thread_id: expected_thread,
                turn_id: expected_turn,
                notification_cursor,
            } if expected_thread == thread_id && expected_turn == turn_id => *notification_cursor,
            _ => anyhow::bail!("scripted turn completion is out of order or changed identity"),
        };
        self.phase = ScriptedPhase::Failed;
        let mut checked = cursor;
        loop {
            while checked < self.notifications.len() {
                let notification = &self.notifications[checked];
                checked += 1;
                if notification["method"] == "turn/completed" {
                    ensure!(
                        notification["params"]["threadId"] == thread_id
                            && notification["params"]["turn"]["id"] == turn_id
                            && notification["params"]["turn"]["status"] == "completed",
                        "scripted terminal turn differs from started identity or status"
                    );
                    self.phase = ScriptedPhase::TurnCompleted;
                    return Ok(());
                }
            }
            ensure!(
                self.next().await?.get("id").is_none(),
                "unexpected app-server response awaiting turn completion"
            );
        }
    }

    pub async fn run_scripted_turn(&mut self) -> Result<(String, String)> {
        self.initialize_scripted().await?;
        let thread_id = self.start_scripted_thread().await?;
        let turn_id = self.start_scripted_turn(&thread_id).await?;
        self.await_scripted_turn_completed(&thread_id, &turn_id)
            .await?;
        Ok((thread_id, turn_id))
    }

    pub async fn close_input(&mut self) -> Result<()> {
        ensure!(
            matches!(&self.phase, ScriptedPhase::TurnCompleted),
            "scripted turn is incomplete"
        );
        self.transport.close_input().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FixtureTransport {
        chunks: VecDeque<Vec<u8>>,
        writes: Vec<Vec<u8>>,
        closed: bool,
    }

    impl AppServerTransport for FixtureTransport {
        async fn write_frame(&mut self, frame: &[u8]) -> Result<()> {
            self.writes.push(frame.to_vec());
            Ok(())
        }

        async fn read_chunk(&mut self) -> Result<Vec<u8>> {
            Ok(self.chunks.pop_front().unwrap_or_default())
        }

        async fn close_input(&mut self) -> Result<()> {
            self.closed = true;
            Ok(())
        }
    }

    fn fixture_chunks() -> VecDeque<Vec<u8>> {
        let frames = concat!(
            "{\"id\":1,\"result\":{}}\n",
            "{\"id\":2,\"result\":{\"thread\":{\"id\":\"thread-1\"}}}\n",
            "{\"id\":3,\"result\":{\"turn\":{\"id\":\"turn-1\"}}}\n",
            "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"thread-1\",",
            "\"turn\":{\"id\":\"turn-1\",\"status\":\"completed\"}}}\n"
        )
        .as_bytes();
        frames.chunks(7).map(|chunk| chunk.to_vec()).collect()
    }

    #[tokio::test]
    async fn scripted_conversation_handles_split_capture_and_closes_after_completion() {
        let transport = FixtureTransport {
            chunks: fixture_chunks(),
            writes: Vec::new(),
            closed: false,
        };
        let mut conversation = ScopedAppServerConversation::new(transport);
        assert_eq!(
            conversation.run_scripted_turn().await.unwrap(),
            ("thread-1".to_owned(), "turn-1".to_owned())
        );
        assert_eq!(conversation.notifications().len(), 1);
        conversation.close_input().await.unwrap();
        assert!(conversation.transport.closed);
        assert_eq!(conversation.transport.writes.len(), 4);
        for frame in &conversation.transport.writes {
            assert_eq!(frame.last(), Some(&b'\n'));
        }
    }

    #[tokio::test]
    async fn changed_terminal_identity_refuses_without_input_close() {
        let mut chunks = fixture_chunks();
        let mut bytes = chunks.into_iter().flatten().collect::<Vec<_>>();
        let original = b"\"turn-1\",\"status\"";
        let changed = b"\"turn-2\",\"status\"";
        let at = bytes
            .windows(original.len())
            .position(|part| part == original)
            .unwrap();
        bytes.splice(at..at + original.len(), changed.iter().copied());
        chunks = bytes.chunks(11).map(|chunk| chunk.to_vec()).collect();
        let transport = FixtureTransport {
            chunks,
            writes: Vec::new(),
            closed: false,
        };
        let mut conversation = ScopedAppServerConversation::new(transport);
        assert!(conversation.run_scripted_turn().await.is_err());
        assert!(conversation.close_input().await.is_err());
        assert!(!conversation.transport.closed);
    }
}
