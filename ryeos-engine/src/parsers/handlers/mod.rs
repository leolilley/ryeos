//! Native parser handlers — the only `with_builtins()` left in the
//! engine. Everything else parser-related is data (YAML descriptors).

pub mod regex_kv;
pub mod yaml_document;
pub mod yaml_header_document;

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::error::EngineError;

/// Input to a parser handler.
pub struct ParseInput<'a> {
    /// Source content with the signature line already stripped.
    pub content: &'a str,
    /// Optional source file path (diagnostic only).
    pub path: Option<&'a Path>,
}

/// A native, in-process parser implementation.
///
/// Handlers live in the engine. A handler is selected by the
/// dispatcher via the descriptor's `handler` (`handler:<ref>`).
pub trait NativeParserHandler: Send + Sync {
    /// Validate `parser_config` shape ahead of time. Returning an
    /// error here surfaces in `boot_validation` so misconfigured
    /// descriptors fail loud at boot, not at first parse.
    fn validate_config(&self, config: &Value) -> Result<(), String>;

    /// Run the parser. `config` is the descriptor's `parser_config`
    /// blob; `input.content` is signature-stripped.
    fn parse(&self, config: &Value, input: ParseInput<'_>) -> Result<Value, EngineError>;
}

/// Registry of native parser handlers.
///
/// `with_builtins` registers exactly the three handlers shipped with
/// the engine. Tests can use `new` to start empty and inject mocks.
pub struct NativeParserHandlerRegistry {
    handlers: HashMap<String, Box<dyn NativeParserHandler>>,
}

impl NativeParserHandlerRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        reg.register(
            "parser_yaml_document",
            Box::new(yaml_document::YamlDocumentHandler),
        );
        reg.register(
            "parser_yaml_header_document",
            Box::new(yaml_header_document::YamlHeaderDocumentHandler),
        );
        reg.register("parser_regex_kv", Box::new(regex_kv::RegexKvHandler));
        reg
    }

    pub fn register(&mut self, name: &str, handler: Box<dyn NativeParserHandler>) {
        self.handlers.insert(name.to_owned(), handler);
    }

    pub fn get(&self, name: &str) -> Option<&dyn NativeParserHandler> {
        self.handlers.get(name).map(|b| b.as_ref())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.handlers.keys().map(|s| s.as_str())
    }
}

impl Default for NativeParserHandlerRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl std::fmt::Debug for NativeParserHandlerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeParserHandlerRegistry")
            .field("handlers", &self.handlers.keys().collect::<Vec<_>>())
            .finish()
    }
}
