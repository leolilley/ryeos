//! Scheduler module — internal timer module for ryeosd.
//!
//! Owns exactly one problem: *when to fire*. Schedule specs are signed
//! node-config YAML at `.ai/node/schedules/`. Fire history is append-only
//! JSONL at `.ai/state/schedules/`. The projection DB (`scheduler.sqlite3`)
//! is rebuildable from CAS.

pub mod crontab;
pub mod db;
pub mod misfire;
pub mod overlap;
pub mod projection;
pub mod reconcile;
pub mod timer;
pub mod types;

pub use types::{FireRecord, ReloadSignal, ScheduleSpecRecord};
