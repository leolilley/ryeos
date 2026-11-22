//! The seam between the wire and whatever produces turns.
//!
//! The server owns framing, method dispatch and turn threads. A `Driver`
//! owns behaviour: the stub in this slice, a Claude Code driver later. A
//! driver never touches the wire directly except through the `Emitter` it
//! hands to a turn closure.

use std::io::{BufRead, Read};
use std::sync::Arc;
use std::thread::JoinHandle;

use serde_json::Value;

use crate::wire::{Emitter, Incoming, MAX_LINE_BYTES, parse};

/// A refusal carried back to the bridge as `{id, error}`.
#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
        }
    }
}

/// What `turn/start` produces: the immediate response, and the body of the
/// turn. The server writes the response first, then runs the body on its own
/// thread, so a turn can never announce itself before the bridge has seen the
/// turn id.
pub struct TurnLaunch {
    pub response: Value,
    pub run: Box<dyn FnOnce() + Send + 'static>,
}

pub trait Driver: Send + Sync {
    fn initialize(&self, params: &Value) -> Result<Value, RpcError>;
    fn session_start(&self, params: &Value) -> Result<Value, RpcError>;
    fn session_resume(&self, params: &Value) -> Result<Value, RpcError>;
    fn session_read(&self, params: &Value) -> Result<Value, RpcError>;
    fn turn_start(&self, params: &Value, events: Emitter) -> Result<TurnLaunch, RpcError>;
    fn turn_interrupt(&self, params: &Value) -> Result<Value, RpcError>;
    /// The bridge closed its input. Every running turn must wind down and
    /// still emit its completion.
    fn shutdown(&self);
}

pub struct Server {
    driver: Arc<dyn Driver>,
    events: Emitter,
    turns: Vec<JoinHandle<()>>,
}

impl Server {
    pub fn new(driver: Arc<dyn Driver>, events: Emitter) -> Self {
        Self {
            driver,
            events,
            turns: Vec::new(),
        }
    }

    /// Serve until the input closes. A line that is not a valid message ends
    /// the run: the adapter has no way to resynchronise with a peer that sent
    /// garbage, and the bridge treats a closed stdout as the adapter's death.
    pub fn run(&mut self, mut input: impl BufRead) -> Result<(), String> {
        loop {
            let mut line = Vec::new();
            let read = (&mut input)
                .take((MAX_LINE_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)
                .map_err(|error| format!("read input: {error}"))?;
            if read == 0 {
                break;
            }
            if line.len() > MAX_LINE_BYTES {
                self.finish();
                return Err("input line exceeds its bound".to_owned());
            }
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            match parse(&line) {
                Ok(Incoming::Request { id, method, params }) => {
                    if let Err(error) = self.dispatch(&id, &method, &params) {
                        self.finish();
                        return Err(error);
                    }
                }
                Ok(Incoming::Notification | Incoming::Response) => {}
                Err(error) => {
                    self.finish();
                    return Err(error);
                }
            }
        }
        self.finish();
        Ok(())
    }

    fn finish(&mut self) {
        self.driver.shutdown();
        for turn in self.turns.drain(..) {
            let _ = turn.join();
        }
    }

    fn dispatch(&mut self, id: &Value, method: &str, params: &Value) -> Result<(), String> {
        let written = match method {
            "turn/start" => match self.driver.turn_start(params, self.events.clone()) {
                Ok(launch) => {
                    let written = self.events.respond(id, launch.response);
                    if written.is_ok() {
                        self.turns.push(std::thread::spawn(launch.run));
                    }
                    written
                }
                Err(error) => self.events.respond_error(id, error.code, &error.message),
            },
            _ => {
                let outcome = match method {
                    "initialize" => self.driver.initialize(params),
                    "session/start" | "session/start_read_only" => {
                        self.driver.session_start(params)
                    }
                    "session/resume" => self.driver.session_resume(params),
                    "session/read" => self.driver.session_read(params),
                    "turn/interrupt" => self.driver.turn_interrupt(params),
                    _ => Err(RpcError {
                        code: -32601,
                        message: format!("method `{method}` is not supported"),
                    }),
                };
                match outcome {
                    Ok(result) => self.events.respond(id, result),
                    Err(error) => self.events.respond_error(id, error.code, &error.message),
                }
            }
        };
        written.map_err(|error| format!("write to the bridge: {error}"))
    }
}
