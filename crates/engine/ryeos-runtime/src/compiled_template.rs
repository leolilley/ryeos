use std::fmt;
use std::mem::size_of;

use serde_json::{Map, Value};

use crate::callback::action_keys;
use crate::expression::{
    compile_template_for, CompilationLimits, CompiledTemplate, EvaluationSession, ExpressionError,
    ReferenceSet,
};

/// Compilation failure for a JSON template tree.
#[derive(Debug)]
pub enum CompiledTemplateError {
    Expression(ExpressionError),
    Structure { field: String, message: String },
}

impl CompiledTemplateError {
    fn structure(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Structure {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for CompiledTemplateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expression(error) => {
                write!(formatter, "{error}; expression {:?}", error.source())
            }
            Self::Structure { field, message } => write!(formatter, "{field}: {message}"),
        }
    }
}

impl std::error::Error for CompiledTemplateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Expression(error) => Some(error),
            Self::Structure { .. } => None,
        }
    }
}

impl From<ExpressionError> for CompiledTemplateError {
    fn from(error: ExpressionError) -> Self {
        Self::Expression(error)
    }
}

#[derive(Debug, Clone, Copy)]
enum StringMode {
    Template,
    Literal,
}

#[derive(Debug, Clone)]
enum CompiledJsonNode {
    Literal {
        value: Value,
        field: String,
    },
    StringTemplate(CompiledTemplate),
    Array {
        values: Vec<CompiledJsonNode>,
        field: String,
    },
    Object {
        entries: Vec<CompiledObjectEntry>,
        field: String,
    },
}

#[derive(Debug, Clone)]
struct CompiledObjectEntry {
    /// Kept as a JSON string so `EvaluationSession::clone_value` accounts for
    /// and validates every key before the rendered map allocates it.
    key: Value,
    key_field: String,
    value: CompiledJsonNode,
}

impl CompiledJsonNode {
    fn field(&self) -> &str {
        match self {
            Self::Literal { field, .. }
            | Self::Array { field, .. }
            | Self::Object { field, .. } => field,
            Self::StringTemplate(template) => template.field().unwrap_or("JSON template"),
        }
    }

    fn render(
        &self,
        session: &mut EvaluationSession<'_>,
    ) -> Result<RenderedJsonNode, ExpressionError> {
        match self {
            Self::Literal { value, field } => {
                let value = session.clone_value(value, field.clone())?;
                let shape = ResultShape::of_value(&value);
                shape.check(session, field)?;
                Ok(RenderedJsonNode { value, shape })
            }
            Self::StringTemplate(template) => {
                let value = session.render_template(template)?;
                let shape = ResultShape::of_value(&value);
                shape.check(session, self.field())?;
                Ok(RenderedJsonNode { value, shape })
            }
            Self::Array { values, field } => {
                let mut shape = ResultShape::empty_container();
                shape.check(session, field)?;
                session.charge_container_elements(values.len(), field.clone())?;
                session.charge_allocation(
                    values.len().saturating_mul(size_of::<Value>()),
                    field.clone(),
                )?;
                let mut rendered = Vec::with_capacity(values.len());
                for (index, value) in values.iter().enumerate() {
                    let value = value.render(session)?;
                    let candidate = shape.with_array_child(value.shape, index);
                    candidate.check(session, field)?;
                    rendered.push(value.value);
                    shape = candidate;
                }
                Ok(RenderedJsonNode {
                    value: Value::Array(rendered),
                    shape,
                })
            }
            Self::Object { entries, field } => {
                let mut shape = ResultShape::empty_container();
                shape.check(session, field)?;
                session.charge_container_elements(entries.len(), field.clone())?;
                session.charge_allocation(
                    entries
                        .len()
                        .saturating_mul(size_of::<String>() + size_of::<Value>()),
                    field.clone(),
                )?;
                let mut rendered = Map::new();
                for (index, entry) in entries.iter().enumerate() {
                    let key = session.clone_value(&entry.key, entry.key_field.clone())?;
                    let Value::String(key) = key else {
                        unreachable!("compiled object keys are always strings")
                    };
                    let value = entry.value.render(session)?;
                    let candidate = shape.with_object_child(&key, value.shape, index);
                    candidate.check(session, field)?;
                    rendered.insert(key, value.value);
                    shape = candidate;
                }
                Ok(RenderedJsonNode {
                    value: Value::Object(rendered),
                    shape,
                })
            }
        }
    }
}

struct RenderedJsonNode {
    value: Value,
    shape: ResultShape,
}

#[derive(Debug, Default, Clone, Copy)]
struct ResultShape {
    depth: usize,
    nodes: usize,
    bytes: usize,
}

impl ResultShape {
    fn empty_container() -> Self {
        Self {
            depth: 1,
            nodes: 1,
            bytes: 2,
        }
    }

    fn of_value(value: &Value) -> Self {
        // Expression-produced leaves may themselves be JSON containers. They
        // are measured once at that boundary; compiled template containers
        // combine these shapes as they build and are never rescanned.
        let mut shape = Self::default();
        shape.measure_value(value, 1);
        shape
    }

    fn measure_value(&mut self, value: &Value, depth: usize) {
        self.depth = self.depth.max(depth);
        self.nodes = self.nodes.saturating_add(1);
        match value {
            Value::Null => self.bytes = self.bytes.saturating_add(4),
            Value::Bool(value) => {
                self.bytes = self.bytes.saturating_add(if *value { 4 } else { 5 });
            }
            Value::Number(number) => {
                self.bytes = self.bytes.saturating_add(number.to_string().len());
            }
            Value::String(value) => {
                self.bytes = self
                    .bytes
                    .saturating_add(crate::expression::json_string_bytes(value));
            }
            Value::Array(values) => {
                self.bytes = self
                    .bytes
                    .saturating_add(2usize.saturating_add(values.len().saturating_sub(1)));
                for value in values {
                    self.measure_value(value, depth + 1);
                }
            }
            Value::Object(values) => {
                self.bytes = self
                    .bytes
                    .saturating_add(2usize.saturating_add(values.len().saturating_sub(1)));
                for (key, value) in values {
                    self.bytes = self
                        .bytes
                        .saturating_add(crate::expression::json_string_bytes(key))
                        .saturating_add(1);
                    self.measure_value(value, depth + 1);
                }
            }
        }
    }

    fn with_array_child(mut self, child: Self, index: usize) -> Self {
        self.depth = self.depth.max(child.depth.saturating_add(1));
        self.nodes = self.nodes.saturating_add(child.nodes);
        self.bytes = self
            .bytes
            .saturating_add(usize::from(index != 0))
            .saturating_add(child.bytes);
        self
    }

    fn with_object_child(mut self, key: &str, child: Self, index: usize) -> Self {
        self.depth = self.depth.max(child.depth.saturating_add(1));
        self.nodes = self.nodes.saturating_add(child.nodes);
        self.bytes = self
            .bytes
            .saturating_add(usize::from(index != 0))
            .saturating_add(crate::expression::json_string_bytes(key))
            .saturating_add(1)
            .saturating_add(child.bytes);
        self
    }

    fn check(
        self,
        session: &mut EvaluationSession<'_>,
        field: &str,
    ) -> Result<(), ExpressionError> {
        session.check_result_shape(self.depth, self.nodes, self.bytes, field)
    }
}

fn render_root(
    root: &CompiledJsonNode,
    session: &mut EvaluationSession<'_>,
) -> Result<Value, ExpressionError> {
    let rendered = root.render(session)?;
    session.charge_result_shape(
        rendered.shape.depth,
        rendered.shape.nodes,
        rendered.shape.bytes,
        root.field().to_string(),
    )?;
    Ok(rendered.value)
}

struct BuildBudget<'a> {
    limits: &'a CompilationLimits,
    nodes: usize,
    source_bytes: usize,
}

impl<'a> BuildBudget<'a> {
    fn new(limits: &'a CompilationLimits) -> Self {
        Self {
            limits,
            nodes: 0,
            source_bytes: 0,
        }
    }

    fn enter(&mut self, field: &str, depth: usize) -> Result<(), CompiledTemplateError> {
        if depth > self.limits.max_ast_depth {
            return Err(CompiledTemplateError::structure(
                field,
                format!(
                    "JSON template nesting exceeds limit {}",
                    self.limits.max_ast_depth
                ),
            ));
        }
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > self.limits.max_literal_elements {
            return Err(CompiledTemplateError::structure(
                field,
                format!(
                    "JSON template contains more than {} values",
                    self.limits.max_literal_elements
                ),
            ));
        }
        Ok(())
    }

    fn source(&mut self, field: &str, bytes: usize) -> Result<(), CompiledTemplateError> {
        self.source_bytes = self.source_bytes.saturating_add(bytes);
        if self.source_bytes > self.limits.max_template_bytes {
            return Err(CompiledTemplateError::structure(
                field,
                format!(
                    "JSON template source exceeds byte limit {}",
                    self.limits.max_template_bytes
                ),
            ));
        }
        Ok(())
    }
}

fn child_field(parent: &str, key: &str) -> String {
    if key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        format!("{parent}.{key}")
    } else {
        format!("{parent}[{key:?}]")
    }
}

fn compile_node(
    value: &Value,
    field: String,
    mode: StringMode,
    depth: usize,
    budget: &mut BuildBudget<'_>,
    references: &mut ReferenceSet,
) -> Result<CompiledJsonNode, CompiledTemplateError> {
    budget.enter(&field, depth)?;
    if let Value::String(source) = value {
        budget.source(&field, source.len())?;
    }
    match value {
        Value::String(source) if matches!(mode, StringMode::Template) => {
            let template = compile_template_for(source, field, budget.limits)?;
            references.extend(template.references());
            Ok(CompiledJsonNode::StringTemplate(template))
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            Ok(CompiledJsonNode::Literal {
                value: value.clone(),
                field,
            })
        }
        Value::Array(values) => {
            let mut compiled = Vec::new();
            for (index, value) in values.iter().enumerate() {
                compiled.push(compile_node(
                    value,
                    format!("{field}[{index}]"),
                    mode,
                    depth + 1,
                    budget,
                    references,
                )?);
            }
            Ok(CompiledJsonNode::Array {
                values: compiled,
                field,
            })
        }
        Value::Object(entries) => {
            let mut compiled = Vec::new();
            for (key, value) in entries {
                let value_field = child_field(&field, key);
                budget.source(&value_field, key.len())?;
                compiled.push(CompiledObjectEntry {
                    key: Value::String(key.clone()),
                    key_field: format!("{value_field} map key"),
                    value: compile_node(value, value_field, mode, depth + 1, budget, references)?,
                });
            }
            Ok(CompiledJsonNode::Object {
                entries: compiled,
                field,
            })
        }
    }
}

/// A recursively compiled JSON value. String values are templates; array and
/// object shape and all object keys remain literal.
#[derive(Debug, Clone)]
pub struct CompiledJsonTemplate {
    root: CompiledJsonNode,
    references: ReferenceSet,
}

impl CompiledJsonTemplate {
    pub fn compile(
        value: &Value,
        field: impl Into<String>,
        limits: &CompilationLimits,
    ) -> Result<Self, CompiledTemplateError> {
        let mut references = ReferenceSet::default();
        let mut budget = BuildBudget::new(limits);
        let root = compile_node(
            value,
            field.into(),
            StringMode::Template,
            1,
            &mut budget,
            &mut references,
        )?;
        Ok(Self { root, references })
    }

    pub fn references(&self) -> &ReferenceSet {
        &self.references
    }

    pub fn render(&self, session: &mut EvaluationSession<'_>) -> Result<Value, ExpressionError> {
        render_root(&self.root, session)
    }
}

/// Callback action template that compiles only the callback-owned top-level
/// template-bearing fields. All other fields, their descendants, and every map
/// key remain literal.
#[derive(Debug, Clone)]
pub struct CompiledActionTemplate {
    root: CompiledJsonNode,
    references: ReferenceSet,
}

impl CompiledActionTemplate {
    pub fn compile(
        action: &Value,
        field: impl Into<String>,
        limits: &CompilationLimits,
    ) -> Result<Self, CompiledTemplateError> {
        let field = field.into();
        let mut references = ReferenceSet::default();
        let mut budget = BuildBudget::new(limits);

        let root = match action {
            Value::Object(entries) => {
                budget.enter(&field, 1)?;
                let mut compiled = Vec::new();
                for (key, value) in entries {
                    let value_field = child_field(&field, key);
                    budget.source(&value_field, key.len())?;
                    let mode = if action_keys::INTERPOLATED.contains(&key.as_str()) {
                        StringMode::Template
                    } else {
                        StringMode::Literal
                    };
                    compiled.push(CompiledObjectEntry {
                        key: Value::String(key.clone()),
                        key_field: format!("{value_field} map key"),
                        value: compile_node(
                            value,
                            value_field,
                            mode,
                            2,
                            &mut budget,
                            &mut references,
                        )?,
                    });
                }
                CompiledJsonNode::Object {
                    entries: compiled,
                    field,
                }
            }
            other => compile_node(
                other,
                field,
                StringMode::Literal,
                1,
                &mut budget,
                &mut references,
            )?,
        };

        Ok(Self { root, references })
    }

    pub fn references(&self) -> &ReferenceSet {
        &self.references
    }

    pub fn render(&self, session: &mut EvaluationSession<'_>) -> Result<Value, ExpressionError> {
        render_root(&self.root, session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expression::{EvaluationLimits, ReferenceSegment};
    use serde_json::json;

    #[test]
    fn recursively_renders_values_but_never_keys() {
        let source = json!({
            "${inputs.dynamic_key}": "${inputs.value}",
            "nested": ["Hello ${inputs.name}", {"count": "${inputs.count}"}],
        });
        let compiled =
            CompiledJsonTemplate::compile(&source, "test.template", &CompilationLimits::default())
                .unwrap();
        let context = json!({
            "inputs": {
                "dynamic_key": "changed",
                "value": true,
                "name": "Rye",
                "count": 3,
            }
        });
        let limits = EvaluationLimits::default();
        let mut session = EvaluationSession::new(&context, &limits);
        let rendered = compiled.render(&mut session).unwrap();

        assert_eq!(rendered["${inputs.dynamic_key}"], json!(true));
        assert_eq!(rendered["nested"][0], "Hello Rye");
        assert_eq!(rendered["nested"][1]["count"], 3);
    }

    #[test]
    fn action_template_obeys_callback_field_whitelist() {
        let source = json!({
            "item_id": "tool:${inputs.tool}",
            "params": {"value": "${inputs.value}"},
            "call": {"method": "${inputs.method}", "args": {"q": "${inputs.query}"}},
            "facets": {"lane": "${inputs.lane}"},
            "thread": "${inputs.thread}",
            "launch_window": {"key": "${inputs.window}", "width": 2},
        });
        let compiled =
            CompiledActionTemplate::compile(&source, "hook.action", &CompilationLimits::default())
                .unwrap();
        let context = json!({
            "inputs": {
                "tool": "echo",
                "value": 7,
                "method": "query",
                "query": "hello",
                "lane": "a",
                "thread": "inline",
                "window": "w",
            }
        });
        let limits = EvaluationLimits::default();
        let mut session = EvaluationSession::new(&context, &limits);
        let rendered = compiled.render(&mut session).unwrap();

        assert_eq!(rendered["item_id"], "tool:echo");
        assert_eq!(rendered["params"]["value"], 7);
        assert_eq!(rendered["call"]["method"], "query");
        assert_eq!(rendered["facets"]["lane"], "a");
        assert_eq!(rendered["thread"], "${inputs.thread}");
        assert_eq!(rendered["launch_window"]["key"], "${inputs.window}");
    }

    #[test]
    fn action_template_preserves_explicit_null_values() {
        let source = json!({
            "item_id": "tool:test/echo",
            "params": {
                "authored": null,
                "computed": "${inputs.value}",
                "nested": [null, {"value": null}],
            },
        });
        let compiled = CompiledActionTemplate::compile(
            &source,
            "graph.nodes.echo.action",
            &CompilationLimits::default(),
        )
        .unwrap();
        let context = json!({"inputs": {"value": null}});
        let limits = EvaluationLimits::default();
        let mut session = EvaluationSession::new(&context, &limits);

        let rendered = compiled.render(&mut session).unwrap();

        assert_eq!(
            rendered,
            json!({
                "item_id": "tool:test/echo",
                "params": {
                    "authored": null,
                    "computed": null,
                    "nested": [null, {"value": null}],
                },
            })
        );
    }

    #[test]
    fn action_references_exclude_literal_fields_and_keys() {
        let source = json!({
            "${inputs.key}": "${inputs.not_a_reference}",
            "params": {"value": "${event.value}"},
            "thread": "${state.literal}",
        });
        let compiled =
            CompiledActionTemplate::compile(&source, "hook.action", &CompilationLimits::default())
                .unwrap();
        let references: Vec<_> = compiled.references().iter().collect();

        assert_eq!(references.len(), 1);
        assert_eq!(references[0].root(), "event");
        assert_eq!(
            references[0].segments(),
            &[ReferenceSegment::Key("value".to_string())]
        );
    }

    #[test]
    fn all_leaves_share_one_cumulative_evaluation_budget() {
        let source = json!({
            "first": "${inputs.first}",
            "second": "${inputs.second}",
        });
        let compiled =
            CompiledJsonTemplate::compile(&source, "test.template", &CompilationLimits::default())
                .unwrap();
        let context = json!({"inputs": {"first": "12345678", "second": "abcdefgh"}});
        let container_and_keys = 2 * (std::mem::size_of::<String>() + std::mem::size_of::<Value>())
            + "first".len()
            + "second".len();
        let limits = EvaluationLimits {
            max_allocation_bytes: container_and_keys + 12,
            ..EvaluationLimits::default()
        };
        let mut session = EvaluationSession::new(&context, &limits);

        assert!(compiled.render(&mut session).is_err());
    }

    #[test]
    fn aggregate_result_limits_cover_the_complete_json_tree() {
        let source = json!({
            "first": "${inputs.first}",
            "second": "${inputs.second}",
        });
        let compiled =
            CompiledJsonTemplate::compile(&source, "test.template", &CompilationLimits::default())
                .unwrap();
        let context = json!({"inputs": {"first": "12345678", "second": "abcdefgh"}});
        let limits = EvaluationLimits {
            max_result_bytes: 24,
            ..EvaluationLimits::default()
        };
        let mut session = EvaluationSession::new(&context, &limits);

        assert!(compiled.render(&mut session).is_err());
    }

    #[test]
    fn array_byte_limit_is_enforced_before_rendering_later_elements() {
        let source = json!([
            "${inputs.value}",
            "${inputs.missing_that_must_not_be_evaluated}",
        ]);
        let compiled =
            CompiledJsonTemplate::compile(&source, "test.template", &CompilationLimits::default())
                .unwrap();
        let context = json!({"inputs": {"value": "12345678"}});
        // The child is 10 JSON bytes and therefore valid by itself. Adding
        // the surrounding array delimiters makes the aggregate 12 bytes.
        let limits = EvaluationLimits {
            max_result_bytes: 11,
            ..EvaluationLimits::default()
        };
        let mut session = EvaluationSession::new(&context, &limits);

        let error = compiled.render(&mut session).unwrap_err();

        assert_eq!(error.phase(), crate::expression::ErrorPhase::Limit);
        assert_eq!(error.field(), Some("test.template"));
        assert!(error.message().contains("JSON byte limit"));
    }

    #[test]
    fn object_node_limit_is_enforced_before_rendering_later_entries() {
        let source = json!({
            "a": "${inputs.value}",
            "z": "${inputs.missing_that_must_not_be_evaluated}",
        });
        let compiled =
            CompiledJsonTemplate::compile(&source, "test.template", &CompilationLimits::default())
                .unwrap();
        let context = json!({"inputs": {"value": [0]}});
        // The child array has two nodes and is valid by itself. The object
        // root is the third aggregate node.
        let limits = EvaluationLimits {
            max_result_nodes: 2,
            ..EvaluationLimits::default()
        };
        let mut session = EvaluationSession::new(&context, &limits);

        let error = compiled.render(&mut session).unwrap_err();

        assert_eq!(error.phase(), crate::expression::ErrorPhase::Limit);
        assert_eq!(error.field(), Some("test.template"));
        assert!(error.message().contains("JSON node limit"));
    }

    #[test]
    fn aggregate_depth_is_enforced_before_rendering_later_entries() {
        let source = json!({
            "a": "${inputs.value}",
            "z": "${inputs.missing_that_must_not_be_evaluated}",
        });
        let compiled =
            CompiledJsonTemplate::compile(&source, "test.template", &CompilationLimits::default())
                .unwrap();
        let context = json!({"inputs": {"value": [0]}});
        // The child array has depth two and is valid by itself. Nesting it
        // below the object root would produce depth three.
        let limits = EvaluationLimits {
            max_result_depth: 2,
            ..EvaluationLimits::default()
        };
        let mut session = EvaluationSession::new(&context, &limits);

        let error = compiled.render(&mut session).unwrap_err();

        assert_eq!(error.phase(), crate::expression::ErrorPhase::Limit);
        assert_eq!(error.field(), Some("test.template"));
        assert!(error.message().contains("JSON depth limit"));
    }

    #[test]
    fn aggregate_result_bytes_include_json_structure_and_escaping() {
        let source = json!({"value": "${inputs.value}"});
        let compiled =
            CompiledJsonTemplate::compile(&source, "test.template", &CompilationLimits::default())
                .unwrap();
        let context = json!({"inputs": {"value": "quote: \" slash: \\ newline:\n"}});
        let expected = json!({"value": context["inputs"]["value"].clone()});
        let exact_bytes = serde_json::to_string(&expected).unwrap().len();
        let mut limits = EvaluationLimits {
            max_result_bytes: exact_bytes - 1,
            ..EvaluationLimits::default()
        };
        let mut session = EvaluationSession::new(&context, &limits);

        assert!(compiled.render(&mut session).is_err());

        limits.max_result_bytes = exact_bytes;
        let mut exact_session = EvaluationSession::new(&context, &limits);
        assert_eq!(compiled.render(&mut exact_session).unwrap(), expected);
    }

    #[test]
    fn literal_action_fields_are_bounded_when_cloned() {
        let source = json!({
            "item_id": "x",
            "thread": "a literal dispatch mode that is too long",
        });
        let compiled =
            CompiledActionTemplate::compile(&source, "hook.action", &CompilationLimits::default())
                .unwrap();
        let context = json!({});
        let limits = EvaluationLimits {
            max_scalar_bytes: 8,
            ..EvaluationLimits::default()
        };
        let mut session = EvaluationSession::new(&context, &limits);

        assert!(compiled.render(&mut session).is_err());
    }
}
