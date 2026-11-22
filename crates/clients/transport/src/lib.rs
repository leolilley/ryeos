//! Signed client transport shared by every Rye OS client (CLI, terminal, web).
//!
//! Owns daemon URL discovery, audience discovery, operator request signing,
//! and the signed HTTP/SSE calls. It carries no node, engine, or presentation
//! code, so a client that only talks to a daemon compiles none of them.

pub mod discovery;
pub mod error;
pub mod http;
pub mod signing;

pub use error::TransportError;
