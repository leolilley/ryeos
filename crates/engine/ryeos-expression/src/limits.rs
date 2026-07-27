/// Parser and template-scanner resource limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilationLimits {
    pub max_source_bytes: usize,
    pub max_template_bytes: usize,
    pub max_expressions_per_template: usize,
    pub max_tokens: usize,
    pub max_ast_depth: usize,
    pub max_literal_elements: usize,
    pub max_function_arguments: usize,
}

impl Default for CompilationLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 64 * 1024,
            max_template_bytes: 256 * 1024,
            max_expressions_per_template: 256,
            max_tokens: 8 * 1024,
            max_ast_depth: 128,
            max_literal_elements: 4 * 1024,
            max_function_arguments: 64,
        }
    }
}

/// Evaluator, traversal, and produced-value resource limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationLimits {
    pub fuel: usize,
    pub max_traversal_depth: usize,
    pub max_container_elements: usize,
    pub max_scalar_bytes: usize,
    pub max_regex_pattern_bytes: usize,
    pub max_regex_haystack_bytes: usize,
    pub max_from_json_bytes: usize,
    pub max_result_depth: usize,
    pub max_result_nodes: usize,
    pub max_result_bytes: usize,
    pub max_produced_string_bytes: usize,
    pub max_allocation_bytes: usize,
}

impl Default for EvaluationLimits {
    fn default() -> Self {
        Self {
            fuel: 200_000,
            max_traversal_depth: 128,
            max_container_elements: 100_000,
            max_scalar_bytes: 1024 * 1024,
            max_regex_pattern_bytes: 16 * 1024,
            max_regex_haystack_bytes: 1024 * 1024,
            max_from_json_bytes: 1024 * 1024,
            max_result_depth: 128,
            max_result_nodes: 100_000,
            max_result_bytes: 4 * 1024 * 1024,
            max_produced_string_bytes: 4 * 1024 * 1024,
            max_allocation_bytes: 8 * 1024 * 1024,
        }
    }
}
