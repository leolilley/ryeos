//! Effects — platform actions returned by the update reducer.
//!
//! Core returns effects; the terminal/web shell performs them.

use crate::ids::ThreadId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    Execute {
        project_path: PathBuf,
        item_ref: String,
        parameters: serde_json::Value,
    },
    SendThreadCommand {
        thread_id: ThreadId,
        command: ThreadCommand,
    },
    RefreshState,
    PersistSession,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ThreadCommand {
    Cancel,
    Kill,
    Interrupt,
}
