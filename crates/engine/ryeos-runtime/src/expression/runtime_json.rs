use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::error::{ErrorPhase, ExpressionError, SourceSpan};
use super::evaluator::{Budget, ContextView, Evaluator};
use super::{EvaluationContext, EvaluationLimits};

/// Incremental validator for a runtime-owned JSON array.
///
/// Foreach and fanout integrations receive one child value at a time. Checking
/// each child independently is insufficient because many individually bounded
/// values can still form an unbounded aggregate; rebuilding and rescanning the
/// whole array after every child would instead make validation quadratic. This
/// accumulator visits each value once and carries only the aggregate shape.
#[derive(Debug, Clone)]
pub struct RuntimeJsonArrayBudget {
    limits: EvaluationLimits,
    field: Arc<str>,
    elements: usize,
    nodes: usize,
    bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct RuntimeJsonValueShape {
    depth: usize,
    nodes: usize,
    bytes: usize,
}

impl RuntimeJsonValueShape {
    fn empty() -> Self {
        Self {
            depth: 0,
            nodes: 0,
            bytes: 0,
        }
    }
}

/// Incremental validator for a runtime-owned JSON object whose top-level
/// entries are replaced over time.
///
/// Sequential foreach assignment has exactly these semantics: every assign
/// map replaces top-level keys in one node-local candidate. Revalidating that
/// whole candidate after every item makes work proportional to
/// `item_count * state_size`. This accumulator measures the base object once,
/// then visits only each newly assigned value. A small depth multiset keeps
/// replacement accounting exact even when the deepest entry is overwritten.
#[derive(Debug, Clone)]
pub struct RuntimeJsonObjectBudget {
    limits: EvaluationLimits,
    field: Arc<str>,
    entries: BTreeMap<String, RuntimeJsonValueShape>,
    depth_counts: BTreeMap<usize, usize>,
    nodes: usize,
    bytes: usize,
}

impl RuntimeJsonObjectBudget {
    pub fn from_object(
        value: &Map<String, Value>,
        field: impl Into<String>,
    ) -> Result<Self, ExpressionError> {
        Self::with_limits(value, field, EvaluationLimits::default())
    }

    pub fn with_limits(
        value: &Map<String, Value>,
        field: impl Into<String>,
        limits: EvaluationLimits,
    ) -> Result<Self, ExpressionError> {
        let mut budget = Self {
            limits,
            field: Arc::from(field.into()),
            entries: BTreeMap::new(),
            depth_counts: BTreeMap::new(),
            nodes: 1,
            bytes: 2,
        };
        budget.check_shape(1, 1, 2)?;
        for (key, value) in value {
            budget.replace(key, value)?;
        }
        Ok(budget)
    }

    /// Replace one top-level object entry. Counters are changed only after the
    /// complete candidate shape has passed all limits.
    pub fn replace(&mut self, key: &str, value: &Value) -> Result<(), ExpressionError> {
        let shape = self.measure_value(value)?;
        let previous = self.entries.get(key).copied();
        let is_new = previous.is_none();

        if key.len() > self.limits.max_scalar_bytes {
            return Err(self.limit_error("object key exceeds scalar byte limit"));
        }
        if is_new && self.entries.len() >= self.limits.max_container_elements {
            return Err(self.limit_error("container traversal element limit exceeded"));
        }

        let previous_nodes = previous.map_or(0, |entry| entry.nodes);
        let nodes = self
            .nodes
            .checked_sub(previous_nodes)
            .and_then(|nodes| nodes.checked_add(shape.nodes))
            .ok_or_else(|| self.limit_error("result JSON node counter overflow"))?;

        let key_bytes = super::evaluator::json_string_bytes(key).saturating_add(1);
        let previous_bytes = previous.map_or(0, |entry| key_bytes.saturating_add(entry.bytes));
        let bytes = self
            .bytes
            .checked_sub(previous_bytes)
            .and_then(|bytes| bytes.checked_add(key_bytes))
            .and_then(|bytes| bytes.checked_add(shape.bytes))
            .and_then(|bytes| bytes.checked_add(usize::from(is_new && !self.entries.is_empty())))
            .ok_or_else(|| self.limit_error("result JSON byte counter overflow"))?;

        let deepest_child = self.prospective_deepest_child(previous, shape);
        let depth = deepest_child.saturating_add(1).max(1);
        self.check_shape(depth, nodes, bytes)?;

        if let Some(previous) = previous {
            decrement_count(&mut self.depth_counts, previous.depth);
        }
        *self.depth_counts.entry(shape.depth).or_default() += 1;
        self.entries.insert(key.to_string(), shape);
        self.nodes = nodes;
        self.bytes = bytes;
        Ok(())
    }

    /// Apply an assignment map using the graph's top-level replacement
    /// semantics. A caller may discard the tracker and candidate if any entry
    /// fails; successful earlier replacements are never committed globally.
    pub fn apply(&mut self, value: &Map<String, Value>) -> Result<(), ExpressionError> {
        for (key, value) in value {
            self.replace(key, value)?;
        }
        Ok(())
    }

    fn prospective_deepest_child(
        &self,
        previous: Option<RuntimeJsonValueShape>,
        replacement: RuntimeJsonValueShape,
    ) -> usize {
        let current = self
            .depth_counts
            .last_key_value()
            .map_or(0, |(depth, _)| *depth);
        if replacement.depth >= current {
            return replacement.depth;
        }
        let Some(previous) = previous else {
            return current;
        };
        if previous.depth != current
            || self.depth_counts.get(&current).copied().unwrap_or_default() > 1
        {
            return current;
        }
        self.depth_counts
            .range(..current)
            .next_back()
            .map_or(replacement.depth, |(depth, _)| {
                (*depth).max(replacement.depth)
            })
    }

    fn check_shape(&self, depth: usize, nodes: usize, bytes: usize) -> Result<(), ExpressionError> {
        if depth > self.limits.max_result_depth {
            return Err(self.limit_error("result exceeds JSON depth limit"));
        }
        if nodes > self.limits.max_result_nodes {
            return Err(self.limit_error("result exceeds JSON node limit"));
        }
        if bytes > self.limits.max_result_bytes {
            return Err(self.limit_error("result exceeds JSON byte limit"));
        }
        Ok(())
    }

    fn measure_value(&self, value: &Value) -> Result<RuntimeJsonValueShape, ExpressionError> {
        let mut shape = RuntimeJsonValueShape::empty();
        self.measure_value_at(value, 1, &mut shape)?;
        Ok(shape)
    }

    fn measure_value_at(
        &self,
        value: &Value,
        depth: usize,
        shape: &mut RuntimeJsonValueShape,
    ) -> Result<(), ExpressionError> {
        if depth > self.limits.max_result_depth {
            return Err(self.limit_error("result exceeds JSON depth limit"));
        }
        shape.depth = shape.depth.max(depth);
        shape.nodes = shape.nodes.saturating_add(1);
        if shape.nodes > self.limits.max_result_nodes {
            return Err(self.limit_error("result exceeds JSON node limit"));
        }
        match value {
            Value::Null => shape.bytes = shape.bytes.saturating_add(4),
            Value::Bool(value) => {
                shape.bytes = shape.bytes.saturating_add(if *value { 4 } else { 5 });
            }
            Value::Number(value) => {
                shape.bytes = shape.bytes.saturating_add(value.to_string().len());
            }
            Value::String(value) => {
                if value.len() > self.limits.max_scalar_bytes {
                    return Err(self.limit_error("string exceeds scalar byte limit"));
                }
                shape.bytes = shape
                    .bytes
                    .saturating_add(super::evaluator::json_string_bytes(value));
            }
            Value::Array(values) => {
                if values.len() > self.limits.max_container_elements {
                    return Err(self.limit_error("container traversal element limit exceeded"));
                }
                shape.bytes = shape
                    .bytes
                    .saturating_add(2usize.saturating_add(values.len().saturating_sub(1)));
                for value in values {
                    self.measure_value_at(value, depth.saturating_add(1), shape)?;
                }
            }
            Value::Object(values) => {
                if values.len() > self.limits.max_container_elements {
                    return Err(self.limit_error("container traversal element limit exceeded"));
                }
                shape.bytes = shape
                    .bytes
                    .saturating_add(2usize.saturating_add(values.len().saturating_sub(1)));
                for (key, value) in values {
                    if key.len() > self.limits.max_scalar_bytes {
                        return Err(self.limit_error("object key exceeds scalar byte limit"));
                    }
                    shape.bytes = shape
                        .bytes
                        .saturating_add(super::evaluator::json_string_bytes(key))
                        .saturating_add(1);
                    self.measure_value_at(value, depth.saturating_add(1), shape)?;
                }
            }
        }
        if shape.bytes > self.limits.max_result_bytes {
            return Err(self.limit_error("result exceeds JSON byte limit"));
        }
        Ok(())
    }

    fn limit_error(&self, message: impl Into<String>) -> ExpressionError {
        let source: Arc<str> = Arc::from("<runtime JSON object>");
        let span = SourceSpan::new(0, source.len());
        ExpressionError::new(
            ErrorPhase::Limit,
            Some(self.field.clone()),
            source,
            span,
            message,
        )
    }
}

fn decrement_count(counts: &mut BTreeMap<usize, usize>, key: usize) {
    let remove = match counts.get_mut(&key) {
        Some(count) if *count > 1 => {
            *count -= 1;
            false
        }
        Some(_) => true,
        None => false,
    };
    if remove {
        counts.remove(&key);
    }
}

impl RuntimeJsonArrayBudget {
    pub fn new(field: impl Into<String>) -> Self {
        Self::with_limits(field, EvaluationLimits::default())
    }

    pub fn with_limits(field: impl Into<String>, limits: EvaluationLimits) -> Self {
        Self {
            limits,
            field: Arc::from(field.into()),
            elements: 0,
            // Account for the array root and its `[` / `]` delimiters before
            // the first child arrives.
            nodes: 1,
            bytes: 2,
        }
    }

    /// Validate and append one logical array element to the aggregate shape.
    /// Counters change only on success, so callers can replace a rejected
    /// child with a bounded null placeholder without rollback bookkeeping.
    pub fn append(&mut self, value: &Value) -> Result<(), ExpressionError> {
        let source: Arc<str> = Arc::from("<runtime JSON array>");
        let span = SourceSpan::new(0, source.len());
        if self.limits.max_result_depth < 1 {
            return Err(self.limit_error(source, span, "result exceeds JSON depth limit"));
        }
        if self.elements >= self.limits.max_container_elements {
            return Err(self.limit_error(
                source,
                span,
                "container traversal element limit exceeded",
            ));
        }

        let mut nodes = self.nodes;
        let Some(mut bytes) = self.bytes.checked_add(usize::from(self.elements != 0)) else {
            return Err(self.limit_error(source, span, "result JSON byte counter overflow"));
        };
        let context = EvaluationContext::new();
        let mut budget = Budget::new(&self.limits);
        let mut evaluator = Evaluator::new(
            ContextView::Roots(&context),
            &mut budget,
            source,
            Some(self.field.clone()),
        );
        // Depth one belongs to the aggregate array; the appended value starts
        // at depth two. Existing node/byte counters make the ordinary result
        // inspector enforce the combined maxima without revisiting old values.
        evaluator.inspect_result(value, 2, &mut nodes, &mut bytes, span)?;

        self.elements += 1;
        self.nodes = nodes;
        self.bytes = bytes;
        Ok(())
    }

    pub fn elements(&self) -> usize {
        self.elements
    }

    fn limit_error(
        &self,
        source: Arc<str>,
        span: SourceSpan,
        message: impl Into<String>,
    ) -> ExpressionError {
        ExpressionError::new(
            ErrorPhase::Limit,
            Some(self.field.clone()),
            source,
            span,
            message,
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn aggregate_budget_rejects_combined_values_without_rescanning() {
        let limits = EvaluationLimits {
            max_result_bytes: 12,
            ..EvaluationLimits::default()
        };
        let mut budget = RuntimeJsonArrayBudget::with_limits("foreach results", limits);

        budget.append(&json!("abc")).unwrap();
        let error = budget.append(&json!("def")).unwrap_err();

        assert_eq!(budget.elements(), 1);
        assert!(error.message().contains("JSON byte limit"));
    }

    #[test]
    fn rejected_value_can_be_replaced_by_null() {
        let limits = EvaluationLimits {
            max_result_bytes: 8,
            ..EvaluationLimits::default()
        };
        let mut budget = RuntimeJsonArrayBudget::with_limits("foreach results", limits);

        assert!(budget.append(&json!("too large")).is_err());
        budget.append(&Value::Null).unwrap();

        assert_eq!(budget.elements(), 1);
    }

    #[test]
    fn object_budget_replaces_entries_without_accumulating_old_values() {
        let limits = EvaluationLimits {
            max_result_bytes: 16,
            ..EvaluationLimits::default()
        };
        let base = json!({"value": "old"});
        let mut budget =
            RuntimeJsonObjectBudget::with_limits(base.as_object().unwrap(), "candidate", limits)
                .unwrap();

        budget.replace("value", &json!(1)).unwrap();
        budget.replace("value", &json!(2)).unwrap();
    }

    #[test]
    fn object_budget_rejects_combined_assignments_incrementally() {
        let limits = EvaluationLimits {
            max_result_bytes: 16,
            ..EvaluationLimits::default()
        };
        let mut budget = RuntimeJsonObjectBudget::with_limits(
            json!({}).as_object().unwrap(),
            "candidate",
            limits,
        )
        .unwrap();

        budget.replace("a", &json!("x")).unwrap();
        let error = budget.replace("b", &json!("y")).unwrap_err();

        assert!(error.message().contains("JSON byte limit"));
    }

    #[test]
    fn object_budget_updates_depth_when_deepest_entry_is_replaced() {
        let limits = EvaluationLimits {
            max_result_depth: 3,
            ..EvaluationLimits::default()
        };
        let base = json!({"deep": {"nested": true}});
        let mut budget =
            RuntimeJsonObjectBudget::with_limits(base.as_object().unwrap(), "candidate", limits)
                .unwrap();

        budget.replace("deep", &Value::Null).unwrap();
        budget.replace("next", &json!({"nested": true})).unwrap();
    }
}
