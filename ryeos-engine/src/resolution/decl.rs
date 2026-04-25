use serde::{Deserialize, Serialize};

/// Tagged enum of declared resolution steps.
/// Unknown step → parse error at schema load time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum ResolutionStepDecl {
    /// Resolve extends field as a DAG, topologically order bottom-up.
    ResolveExtendsChain {
        /// Field name to walk (default "extends").
        #[serde(default = "default_extends_field")]
        field: String,
        /// Maximum chain depth (default 16).
        #[serde(default = "default_extends_depth")]
        max_depth: usize,
    },
    /// Resolve references field laterally (no hierarchy, cycles allowed).
    ResolveReferences {
        /// Field name to walk (default "references").
        #[serde(default = "default_references_field")]
        field: String,
        /// Maximum chain depth (default 3).
        #[serde(default = "default_references_depth")]
        max_depth: usize,
    },
}

fn default_extends_field() -> String {
    "extends".to_string()
}

fn default_extends_depth() -> usize {
    16
}

fn default_references_field() -> String {
    "references".to_string()
}

fn default_references_depth() -> usize {
    3
}
