#[cfg(not(feature = "common-handler"))]
compile_error!(
    "select an explicit handler composition: common-handler, basic-parsers, or full-handlers"
);

#[cfg(feature = "full-handlers")]
pub mod direct_execution_evidence;
#[cfg(feature = "full-handlers")]
pub mod directive_launch;
#[cfg(feature = "full-handlers")]
pub mod extends_chain;
#[cfg(feature = "full-handlers")]
pub mod graph_effective_validator;
#[cfg(feature = "full-handlers")]
pub mod graph_execution_evidence;
#[cfg(feature = "full-handlers")]
pub mod graph_launch;
#[cfg(feature = "full-handlers")]
pub mod identity;
#[cfg(feature = "basic-parsers")]
pub mod regex_kv;
#[cfg(feature = "full-handlers")]
pub mod scoped_qualification_execution_evidence;
#[cfg(feature = "basic-parsers")]
pub mod yaml_document;
#[cfg(feature = "basic-parsers")]
pub mod yaml_header_document;

#[cfg(feature = "common-handler")]
mod stdio;
#[cfg(feature = "common-handler")]
pub use stdio::run_handler;
