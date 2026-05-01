//! Parsers as tools — subprocess-dispatched via `HandlerRegistry`.
//!
//! Parsers are a kind. Descriptors live at `.ai/parsers/**/*.yaml` and
//! are addressed by canonical refs of the form `parser:rye/core/...`.
//! The kind identity is implicit from location — there is no
//! discriminator field on the descriptor.
//!
//! Each descriptor names a `handler:rye/core/<name>` ref; the
//! `ParserDispatcher` resolves that through `HandlerRegistry` and
//! spawns the handler binary as an env-cleared subprocess
//! (via `lillux::exec::lib_run`). There are no in-process native
//! parser handlers in the engine anymore.
//!
//! Bootstrap order (cycle break):
//!   trust store → kind schemas (raw signed YAML loader)
//!     → parser descriptors (raw signed YAML loader, NOT through
//!       the normal tool-kind parser path — would be circular)
//!     → handler registry (signed binaries on disk)
//!     → composer registry
//!     → boot validator
//!     → ready

pub mod descriptor;
pub mod dispatcher;
pub mod registry;

#[cfg(test)]
pub(crate) mod test_helpers;

pub use descriptor::ParserDescriptor;
pub use dispatcher::ParserDispatcher;
pub use registry::{DuplicateRef, ParserRegistry};
