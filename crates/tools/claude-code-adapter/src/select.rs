//! Chooses, per session, between the Claude Code driver and the stub.
//!
//! The rule:
//! - A binary built without the `stub-driver` feature has no stub. Its
//!   `session/start` refuses a payload that carries a `stub` object, every
//!   session is a Claude Code session, and an unusable Claude Code
//!   configuration fails `initialize` loudly.
//! - A binary built with the feature (opt-in, for the test bundle refresh) starts a stub
//!   session only when the `session/start` payload carries a `stub` object.
//!   A session without one is always a Claude Code session, and refuses when
//!   Claude Code is not configured: a missing executable never silently
//!   yields scripted results.
//! - Every later call follows the session id: stub ids belong to the stub,
//!   everything else to Claude Code.

use serde_json::Value;

use crate::claude::ClaudeCodeDriver;
use crate::driver::{Driver, RpcError, TurnLaunch};
use crate::wire::Emitter;

#[cfg(feature = "stub-driver")]
use crate::stub::StubDriver;

pub struct Adapter {
    live: Result<ClaudeCodeDriver, String>,
    #[cfg(feature = "stub-driver")]
    stub: StubDriver,
}

impl Adapter {
    pub fn from_environment() -> Self {
        Self {
            live: ClaudeCodeDriver::from_environment(),
            #[cfg(feature = "stub-driver")]
            stub: StubDriver::from_environment(),
        }
    }

    fn live(&self) -> Result<&ClaudeCodeDriver, RpcError> {
        self.live
            .as_ref()
            .map_err(|why| RpcError::refused(format!("claude code is not configured: {why}")))
    }

    /// The stub, when it owns this session by its id.
    #[cfg(feature = "stub-driver")]
    fn stub_for(&self, session_id: Option<&str>) -> Option<&StubDriver> {
        session_id
            .is_some_and(|id| id.starts_with("stub-session-"))
            .then_some(&self.stub)
    }
}

#[cfg(feature = "stub-driver")]
fn session_id(params: &Value) -> Option<&str> {
    params.get("session_id").and_then(Value::as_str)
}

impl Driver for Adapter {
    fn initialize(&self, params: &Value) -> Result<Value, RpcError> {
        #[cfg(feature = "stub-driver")]
        if self.live.is_err() {
            return self.stub.initialize(params);
        }
        self.live()?.initialize(params)
    }

    fn session_start(&self, params: &Value) -> Result<Value, RpcError> {
        let asks_for_stub = params.get("stub").is_some_and(|stub| !stub.is_null());
        #[cfg(feature = "stub-driver")]
        if asks_for_stub {
            return self.stub.session_start(params);
        }
        if asks_for_stub {
            return Err(RpcError::refused(
                "this adapter was built without the stub driver",
            ));
        }
        self.live()?.session_start(params)
    }

    fn session_resume(&self, params: &Value) -> Result<Value, RpcError> {
        #[cfg(feature = "stub-driver")]
        if let Some(stub) = self.stub_for(session_id(params)) {
            return stub.session_resume(params);
        }
        self.live()?.session_resume(params)
    }

    fn session_read(&self, params: &Value) -> Result<Value, RpcError> {
        #[cfg(feature = "stub-driver")]
        if let Some(stub) = self.stub_for(session_id(params)) {
            return stub.session_read(params);
        }
        self.live()?.session_read(params)
    }

    fn turn_start(&self, params: &Value, events: Emitter) -> Result<TurnLaunch, RpcError> {
        #[cfg(feature = "stub-driver")]
        if let Some(stub) = self.stub_for(session_id(params)) {
            return stub.turn_start(params, events);
        }
        self.live()?.turn_start(params, events)
    }

    fn turn_interrupt(&self, params: &Value) -> Result<Value, RpcError> {
        #[cfg(feature = "stub-driver")]
        if let Some(stub) = self.stub_for(session_id(params)) {
            return stub.turn_interrupt(params);
        }
        self.live()?.turn_interrupt(params)
    }

    fn shutdown(&self) {
        #[cfg(feature = "stub-driver")]
        self.stub.shutdown();
        if let Ok(live) = &self.live {
            live.shutdown();
        }
    }
}
