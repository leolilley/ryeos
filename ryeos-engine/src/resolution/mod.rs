/// Resolution pipeline — preprocessing steps before execution.
/// Walks extends/references DAGs, expands aliases recursively, detects cycles.

pub mod alias;
pub mod decl;
pub mod types;

pub use alias::AliasResolver;
pub use decl::ResolutionStepDecl;
pub use types::{
    AliasHop, ChainHop, ResolutionEdge, ResolutionError, ResolutionOutput, ResolutionStepName,
    TrustClass,
};
