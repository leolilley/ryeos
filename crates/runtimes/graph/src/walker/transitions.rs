use crate::model::{ErrorMode, ErrorRecord, GraphConfig, GraphNode};

use super::NextOnError;

/// Whether a node whose current dispatch just failed has retry attempts left.
///
/// `retry_attempt` is the number of attempts already spent BEFORE this one, so
/// the attempt that just failed is `retry_attempt + 1`. Returns that 1-based
/// failed-attempt number when a further attempt is allowed under the node's
/// `retry.attempts` (the total, incl. the first), and `None` when the policy is
/// absent or exhausted (route through `on_error`).
pub(super) fn retry_attempts_remaining(node: &GraphNode, retry_attempt: u32) -> Option<u32> {
    let rc = node.retry.as_ref()?;
    let failed_attempt = retry_attempt + 1;
    (failed_attempt < rc.attempts).then_some(failed_attempt)
}

/// Resolve what to do on error based on node-level `on_error` and
/// graph-level `on_error` mode.
pub(super) fn resolve_next_on_error(node: &GraphNode, cfg: &GraphConfig) -> NextOnError {
    if let Some(ref target) = node.on_error {
        NextOnError::Redirect(target.clone())
    } else {
        match cfg.on_error {
            ErrorMode::Continue => NextOnError::PolicyContinue,
            ErrorMode::Fail => NextOnError::PolicyFail,
        }
    }
}

/// Build one combined diagnostic for a foreach node whose per-item failures
/// trip a fail/redirect policy. Leads with the count and the first item's error
/// (which carries the leaf stderr excerpt).
pub(super) fn foreach_failure_summary(node: &str, errors: &[ErrorRecord]) -> String {
    let first = errors
        .first()
        .map(|e| e.error.as_str())
        .unwrap_or("unknown error");
    format!(
        "foreach node `{node}` failed: {} of its iterations errored; first: {first}",
        errors.len()
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::model::{NodeType, RetryConfig};

    use super::*;

    fn node() -> GraphNode {
        GraphNode {
            node_type: NodeType::Action,
            action: None,
            assign: None,
            next: None,
            on_error: None,
            cache_result: false,
            cache: false,
            follow: false,
            detach: false,
            facets: None,
            over: None,
            r#as: None,
            collect: None,
            parallel: false,
            max_concurrency: None,
            output: None,
            env_requires: Vec::new(),
            retry: None,
        }
    }

    fn config(on_error: ErrorMode) -> GraphConfig {
        GraphConfig {
            start: "start".to_string(),
            max_steps: 100,
            on_error,
            nodes: HashMap::new(),
            hooks: Vec::new(),
            config_schema: None,
            env_requires: Vec::new(),
            state: None,
            max_concurrency: None,
            segment_steps: None,
        }
    }

    #[test]
    fn node_error_target_overrides_graph_policy() {
        let node = GraphNode {
            on_error: Some("handler".to_string()),
            ..node()
        };

        let next = resolve_next_on_error(&node, &config(ErrorMode::Continue));

        assert!(matches!(next, NextOnError::Redirect(ref target) if target == "handler"));
    }

    #[test]
    fn graph_error_policy_applies_without_node_target() {
        assert!(matches!(
            resolve_next_on_error(&node(), &config(ErrorMode::Fail)),
            NextOnError::PolicyFail
        ));
        assert!(matches!(
            resolve_next_on_error(&node(), &config(ErrorMode::Continue)),
            NextOnError::PolicyContinue
        ));
    }

    #[test]
    fn retry_budget_counts_the_initial_dispatch() {
        let node = GraphNode {
            retry: Some(RetryConfig {
                attempts: 3,
                backoff_ms: 10,
                max_backoff_ms: None,
            }),
            ..node()
        };

        assert_eq!(retry_attempts_remaining(&node, 0), Some(1));
        assert_eq!(retry_attempts_remaining(&node, 1), Some(2));
        assert_eq!(retry_attempts_remaining(&node, 2), None);
    }

    #[test]
    fn foreach_summary_preserves_count_and_first_error() {
        let errors = vec![
            ErrorRecord {
                step: 2,
                node: "fanout".to_string(),
                error: "first failure".to_string(),
            },
            ErrorRecord {
                step: 2,
                node: "fanout".to_string(),
                error: "second failure".to_string(),
            },
        ];

        assert_eq!(
            foreach_failure_summary("fanout", &errors),
            "foreach node `fanout` failed: 2 of its iterations errored; first: first failure"
        );
    }
}
