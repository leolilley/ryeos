//! The seams between the runtime and a host.
//!
//! The runtime never names a browser, an async executor or a file system. A
//! host supplies these traits through [`Platform`]; the runtime is generic
//! over it. Traits use `async fn` and associated types because they are not
//! object safe. All futures are `?Send`: the browser's futures hold
//! non-thread-safe handles, and native hosts run the runtime on a local task
//! set.
//!
//! Cancellation: dropping a future returned by a platform method must cancel
//! the underlying request.

#![allow(async_fn_in_trait)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use crate::error::RuntimeError;

#[cfg(not(target_arch = "wasm32"))]
pub mod native;
#[cfg(target_arch = "wasm32")]
pub mod web;

/// Pull the daemon's `{code, error}` envelope into a readable line, falling
/// back to the raw body.
pub(crate) fn daemon_error_message(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string())
}

/// The host: one value bundling every seam.
pub trait Platform {
    type Clock: Clock;
    type Spawner: Spawner;
    type Transport: Transport;
    type Auth: Auth;
    type Store: Store;

    fn clock(&self) -> &Self::Clock;
    fn spawner(&self) -> &Self::Spawner;
    fn transport(&self) -> &Self::Transport;
    fn auth(&self) -> &Self::Auth;
    fn store(&self) -> &Self::Store;

    /// Fill `out` with cryptographically secure random bytes.
    fn random_bytes(&self, out: &mut [u8]);
}

/// A deadline passed before the future completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

/// Wall clock and timers.
pub trait Clock {
    /// Milliseconds since the Unix epoch, for `observed_at_ms` stamps. The
    /// only time source the runtime uses (`SystemTime::now` panics on wasm32).
    fn now_ms(&self) -> u64;

    /// Complete after `duration`. Must not busy-wait.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()>;

    /// Run `future`, dropping it (and so cancelling it) if `duration` passes
    /// first.
    fn timeout<F: Future>(
        &self,
        duration: Duration,
        future: F,
    ) -> impl Future<Output = Result<F::Output, Elapsed>>;
}

/// A spawned task.
///
/// Dropping the handle aborts the task; a task that must outlive its owner is
/// kept in a retained handle, never detached.
pub trait TaskHandle {
    /// Request cancellation at the task's next await point.
    fn abort(&self);

    fn is_finished(&self) -> bool;
}

/// Task execution on the host's local executor.
pub trait Spawner {
    type Handle: TaskHandle;

    /// Run `future` on the calling thread's executor. The future is not
    /// `Send`.
    fn spawn(&self, future: impl Future<Output = ()> + 'static) -> Self::Handle;
}

/// A pull source of body bytes: an event stream or a POST response body.
///
/// Cancel-safe: dropping the future loses no bytes.
pub trait ByteSource {
    /// The next transport read. `Ok(None)` is a clean end of stream; an error
    /// is a transport failure and the caller decides whether to reconnect.
    /// Framing (SSE lines, frame bounds) is the runtime's job.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, RuntimeError>;
}

/// Authenticated request/response transport to the daemon. Paths are
/// origin-relative (`/ui/api/...`). Bodies are text the runtime has already
/// encoded and measured; the host signs and sends exactly those bytes, so a
/// retry is byte-identical.
pub trait Transport {
    /// Hints and chain tails: a GET `text/event-stream`.
    type Sse: ByteSource;
    /// Seat history: a POST whose response is an event stream.
    type Frames: ByteSource;

    /// GET; the body as text, decoded by the runtime. A non-2xx answer is
    /// [`RuntimeError::Daemon`].
    async fn get(&self, path: &str) -> Result<String, RuntimeError>;

    /// The only POST: send `body` unchanged. Failure to deliver is
    /// [`RuntimeError::Network`]; an unreadable 2xx body is
    /// [`RuntimeError::UnreadableResponse`]; a non-2xx answer is
    /// [`RuntimeError::Daemon`].
    async fn post_encoded(&self, path: &str, body: String) -> Result<String, RuntimeError>;

    /// Open a GET event stream.
    async fn open_sse(&self, path: &str) -> Result<Self::Sse, RuntimeError>;

    /// POST `body` and return the event-stream response. A non-2xx status, a
    /// missing event-stream content type or an empty body is an error before
    /// the source is returned.
    async fn open_history(&self, path: &str, body: String) -> Result<Self::Frames, RuntimeError>;
}

/// How a session is established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EstablishMode {
    /// Mint a launch URL and redeem it.
    Fresh,
    /// Adopt an existing session by its credential.
    Adopt(String),
    /// The launch cookie is already present; only read the session.
    CookiePresent,
}

/// What replacing the session means for this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementMode {
    /// Load the successor's launch URL in a new document.
    Navigate,
    /// Redeem the successor's launch URL in this process.
    InPlace,
}

/// The authenticated UI session a host has established.
#[derive(Debug, Clone, PartialEq)]
pub struct EstablishedSession {
    pub session_id: String,
    /// The unwrapped `/ui/api/session/current` document.
    pub current: serde_json::Value,
}

/// Session establishment. Request signing is the transport's concern.
pub trait Auth {
    async fn establish(&self, mode: EstablishMode) -> Result<EstablishedSession, RuntimeError>;

    /// Redeem a successor's one-shot launch URL ([`ReplacementMode::InPlace`]).
    async fn redeem(&self, launch_url: &str) -> Result<EstablishedSession, RuntimeError>;

    fn replacement(&self) -> ReplacementMode;
}

/// Small durable key-value storage (pending append, layout preferences).
pub trait Store {
    fn get(&self, key: &str) -> Option<String>;

    /// A refused write must be reported ([`RuntimeError::StoreWrite`]): the
    /// caller fences new mutations before sending anything it could not retain.
    fn set(&self, key: &str, value: &str) -> Result<(), RuntimeError>;

    fn remove(&self, key: &str);

    /// True when the host keeps keys where the session id (a bearer
    /// credential) must not appear, such as file names. The runtime then names
    /// the pending-append record by the session id's SHA-256 instead of the
    /// session id.
    fn hashes_session_keys(&self) -> bool {
        false
    }
}

/// In-memory [`Store`] for tests and for hosts with no durable storage.
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: RefCell<HashMap<String, String>>,
    fail_writes: RefCell<bool>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every later `set` fail, to exercise write-failure fences.
    pub fn fail_writes(&self, fail: bool) {
        *self.fail_writes.borrow_mut() = fail;
    }

    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }
}

impl Store for MemoryStore {
    fn get(&self, key: &str) -> Option<String> {
        self.entries.borrow().get(key).cloned()
    }

    fn set(&self, key: &str, value: &str) -> Result<(), RuntimeError> {
        if *self.fail_writes.borrow() {
            return Err(RuntimeError::StoreWrite);
        }
        self.entries
            .borrow_mut()
            .insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    fn remove(&self, key: &str) {
        self.entries.borrow_mut().remove(key);
    }
}
