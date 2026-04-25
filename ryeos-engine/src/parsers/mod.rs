//! Parsers as tools — engine-local synchronous dispatch.
//!
//! Parsers are a kind. Descriptors live at `.ai/parsers/**/*.yaml` and
//! are addressed by canonical refs of the form `parser:rye/core/...`.
//! The kind identity is implicit from location — there is no
//! discriminator field on the descriptor.
//!
//! The engine ships only a small set of `native:` handlers
//! (`yaml_document`, `yaml_header_document`, `regex_kv`) — all other
//! parser composition happens by writing more parser descriptors that
//! point at one of those handlers.
//!
//! Bootstrap order (cycle break):
//!   trust store → kind schemas (raw signed YAML loader)
//!     → parser descriptors (raw signed YAML loader, NOT through
//!       the normal tool-kind parser path — would be circular)
//!     → native handler registry
//!     → composer registry
//!     → boot validator
//!     → ready

pub mod descriptor;
pub mod dispatcher;
pub mod handlers;
pub mod registry;

#[cfg(test)]
pub(crate) mod test_helpers;

pub use descriptor::ParserDescriptor;
pub use dispatcher::ParserDispatcher;
pub use handlers::{NativeParserHandler, NativeParserHandlerRegistry, ParseInput};
pub use registry::{DuplicateRef, ParserRegistry};
