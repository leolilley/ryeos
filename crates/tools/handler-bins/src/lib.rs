#[cfg(not(feature = "common-handler"))]
compile_error!(
    "select an explicit handler composition: core-handlers, extends-composer, graph-handlers, or directive-handlers"
);

#[cfg(feature = "core-handlers")]
pub mod direct_execution_evidence;
#[cfg(feature = "directive-handlers")]
pub mod directive_launch;
#[cfg(feature = "extends-composer")]
pub mod extends_chain;
#[cfg(feature = "graph-handlers")]
pub mod graph_effective_validator;
#[cfg(feature = "graph-handlers")]
pub mod graph_execution_evidence;
#[cfg(feature = "graph-handlers")]
pub mod graph_launch;
#[cfg(feature = "core-handlers")]
pub mod identity;
#[cfg(feature = "core-handlers")]
pub mod scoped_qualification_execution_evidence;
