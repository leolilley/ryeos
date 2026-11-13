//! The shared client runtime.
//!
//! One implementation of the seat protocol's client side, written against
//! small platform traits so the terminal, the browser and a native window
//! drive the same code:
//!
//! * [`codec`], [`api`], [`error`], [`retention`]: exact JSON, request
//!   construction, the single error type and the retained pending append;
//! * [`sse`], [`history`], [`replay`]: the event-stream parser, seat history
//!   consumption and the private replay candidate over the shared core;
//! * [`admission`], [`seat`]: which seat a session names, and the seat state
//!   machine (claim, append, touch, relinquish, reproof, replacement);
//! * [`streams`], [`effects`], [`inbox`], [`recovery`]: hints and tails, effect
//!   execution, the ordered inbox, and what to offer when a seat is lost;
//! * [`documents`]: independent presentation documents offered as links;
//! * [`runtime`]: [`ClientRuntime`], the object a host drives.
//!
//! Platform bindings (HTTP, timers, storage) are selected by target
//! architecture, never by cargo feature; this crate itself touches no clock,
//! thread or I/O except through [`platform`].

pub mod admission;
pub mod api;
pub mod codec;
pub mod documents;
pub mod effects;
pub mod error;
pub mod history;
pub mod inbox;
pub mod layout;
pub mod platform;
pub mod recovery;
pub mod replay;
pub mod retention;
pub mod runtime;
pub mod seat;
pub mod sse;
pub mod streams;

pub use error::RuntimeError;
pub use runtime::ClientRuntime;

// The in-crate flow tests drive a `ClientRuntime` through its private state and
// share the integration tests' deterministic platform.
#[cfg(test)]
extern crate self as ryeos_client_runtime;
#[cfg(test)]
#[path = "../tests/common/sim.rs"]
mod sim;
