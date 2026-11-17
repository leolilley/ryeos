//! The bridge's stdio wire: one JSON object per line.
//!
//! Requests are `{id, method, params}` with integer ids, responses are
//! `{id, result}` or `{id, error}`, and notifications are `{method, params}`
//! with no id. There is no `jsonrpc` member. Writes are serialised so a
//! notification emitted from a turn thread can never interleave with a
//! response written by the request loop.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// The longest input line the adapter accepts. The bridge never sends more
/// than its own 256 KiB goal ceiling plus a small envelope, so this is a
/// bound against a corrupt peer rather than a working limit.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// One message read from the bridge.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A message with a method and no id. The bridge sends `initialized`
    /// this way; the adapter never answers one.
    Notification,
    /// A response to something the adapter sent. The adapter sends no
    /// requests, so these are ignored.
    Response,
}

pub fn parse(line: &[u8]) -> Result<Incoming, String> {
    let message: Value =
        serde_json::from_slice(line).map_err(|error| format!("invalid JSON line: {error}"))?;
    let object = message
        .as_object()
        .ok_or_else(|| "message is not an object".to_owned())?;
    let id = object.get("id").filter(|value| !value.is_null());
    let method = object.get("method").and_then(Value::as_str);
    match (id, method) {
        (Some(id), Some(method)) => Ok(Incoming::Request {
            id: id.clone(),
            method: method.to_owned(),
            params: object.get("params").cloned().unwrap_or_else(|| json!({})),
        }),
        (None, Some(_)) => Ok(Incoming::Notification),
        (Some(_), None) => Ok(Incoming::Response),
        (None, None) => Err("message has neither an id nor a method".to_owned()),
    }
}

/// A cloneable handle that writes whole lines to the bridge.
#[derive(Clone)]
pub struct Emitter {
    out: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Emitter {
    pub fn new(out: impl Write + Send + 'static) -> Self {
        Self {
            out: Arc::new(Mutex::new(Box::new(out))),
        }
    }

    pub fn respond(&self, id: &Value, result: Value) -> io::Result<()> {
        self.line(&json!({"id": id, "result": result}))
    }

    pub fn respond_error(&self, id: &Value, code: i64, message: &str) -> io::Result<()> {
        self.line(&json!({"id": id, "error": {"code": code, "message": message}}))
    }

    pub fn notify(&self, method: &str, params: Value) -> io::Result<()> {
        self.line(&json!({"method": method, "params": params}))
    }

    fn line(&self, message: &Value) -> io::Result<()> {
        let mut frame = serde_json::to_vec(message).map_err(io::Error::other)?;
        frame.push(b'\n');
        let mut out = self
            .out
            .lock()
            .map_err(|_| io::Error::other("adapter output is poisoned"))?;
        out.write_all(&frame)?;
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_notifications_and_responses_are_told_apart() {
        assert_eq!(
            parse(br#"{"id":4,"method":"turn/start","params":{"a":1}}"#).unwrap(),
            Incoming::Request {
                id: json!(4),
                method: "turn/start".into(),
                params: json!({"a":1}),
            }
        );
        assert_eq!(
            parse(br#"{"id":5,"method":"initialize"}"#).unwrap(),
            Incoming::Request {
                id: json!(5),
                method: "initialize".into(),
                params: json!({}),
            }
        );
        assert_eq!(
            parse(br#"{"method":"initialized","params":{}}"#).unwrap(),
            Incoming::Notification
        );
        assert_eq!(
            parse(br#"{"id":1,"result":{}}"#).unwrap(),
            Incoming::Response
        );
        assert!(parse(b"[]").is_err());
        assert!(parse(b"{}").is_err());
        assert!(parse(b"not json").is_err());
    }
}
