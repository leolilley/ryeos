//! A scripted chat-completions stub server.
//!
//! It speaks the chat-completions wire format (streaming SSE and
//! non-streaming) and replays a [`Script`]: an ordered list of scripted
//! replies and failures. It records every request body so a test can assert
//! on exactly what a runtime sent. See [`script`] for the script format.
//!
//! Library use:
//!
//! ```ignore
//! let stub = StubServer::start(Script::from_yaml_str(text)?).await;
//! // point the provider base URL at stub.base_url ...
//! let bodies = stub.request_bodies().await;
//! ```
//!
//! Standalone use: `ryeos-chat-stub <script-file> [--port N] [--record F]`.
//! Recorded requests are also served as JSON at `GET /_stub/requests`.

pub mod render;
pub mod script;
mod server;

pub use script::Script;
pub use server::{RecordedRequest, StubServer};
