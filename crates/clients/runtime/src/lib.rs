//! The shared client runtime.
//!
//! One implementation of the seat protocol's client side: exact JSON, request
//! construction, the ordered effect inbox and the retained pending append,
//! written against small platform traits so the terminal, the browser and a
//! native window drive the same code.
//!
//! This slice holds the parts that need no async runtime and no platform
//! binding. Platform bindings are selected by target architecture, never by
//! cargo feature.

pub mod api;
pub mod codec;
pub mod error;
pub mod inbox;
pub mod platform;
pub mod retention;

pub use error::RuntimeError;
