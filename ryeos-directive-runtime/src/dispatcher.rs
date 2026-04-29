use serde_json::Value;

use crate::adapter::parse_tool_arguments;
use crate::directive::{OutputSpec, ToolSchema};

#[derive(Debug, Clone, PartialEq)]
pub enum DispatchKind {
    Tool,
    DirectiveChild,
    GraphChild,
    Internal,
}

#[derive(Debug, Clone)]
pub struct ToolDispatchResult {
    pub tool_name: String,
    pub call_id: Option<String>,
    pub arguments: Value,
    pub canonical_ref: String,
    pub dispatch_kind: DispatchKind,
}

fn classify_dispatch(canonical_ref: &str, is_internal: bool) -> DispatchKind {
    if is_internal {
        return DispatchKind::Internal;
    }
    if canonical_ref.starts_with("directive:") {
        DispatchKind::DirectiveChild
    } else if canonical_ref.starts_with("graph:") {
        DispatchKind::GraphChild
    } else {
        DispatchKind::Tool
    }
}

pub struct Dispatcher {
    tools: Vec<ToolSchema>,
    outputs: Option<Vec<OutputSpec>>,
    effective_caps: Vec<String>,
}

impl Dispatcher {
    pub fn new(
        tools: Vec<ToolSchema>,
        outputs: Option<Vec<OutputSpec>>,
        effective_caps: Vec<String>,
    ) -> Self {
        Self {
            tools,
            outputs,
            effective_caps,
        }
    }

    #[tracing::instrument(name = "tool:resolve", skip(self, raw_args), fields(tool_name = %tool_name))]
    pub fn resolve(&self, tool_name: &str, raw_args: &str, call_id: Option<String>) -> Result<ToolDispatchResult, String> {
        // Typed-fail-loud per remediation: malformed tool-call args
        // bail at the dispatcher rather than dispatching with `{}`.
        let arguments = parse_tool_arguments(raw_args)?;

        if tool_name == "directive_return" {
            return self.handle_directive_return(&arguments, call_id);
        }

        let (canonical_ref, is_internal) = self
            .find_tool(tool_name)
            .ok_or_else(|| format!("unknown tool: {}", tool_name))?;

        let required_cap = format!("rye.execute.tool.{}", canonical_ref);
        if !is_internal && !self.check_permission(&required_cap) {
            return Err(format!(
                "permission denied: {} (no matching capability)",
                required_cap
            ));
        }

        let dispatch_kind = classify_dispatch(&canonical_ref, is_internal);

        Ok(ToolDispatchResult {
            tool_name: tool_name.to_string(),
            call_id,
            arguments,
            canonical_ref,
            dispatch_kind,
        })
    }

    fn find_tool(&self, name: &str) -> Option<(String, bool)> {
        let internal_tools = ["directive_return"];

        if internal_tools.contains(&name) {
            return Some((name.to_string(), true));
        }

        for tool in &self.tools {
            if tool.name == name {
                return Some((tool.item_id.clone(), false));
            }
        }

        None
    }

    fn check_permission(&self, required: &str) -> bool {
        ryeos_runtime::cap_matches(&self.effective_caps.join(","), required)
            || self
                .effective_caps
                .iter()
                .any(|cap| ryeos_runtime::cap_matches(cap, required))
    }

    fn handle_directive_return(&self, args: &Value, call_id: Option<String>) -> Result<ToolDispatchResult, String> {
        if let Some(ref outputs) = self.outputs {
            for output in outputs {
                if !args.get(&output.name).map_or(false, |v| !v.is_null()) {
                    return Err(format!(
                        "directive_return: missing required output '{}'",
                        output.name
                    ));
                }
            }
        }

        Ok(ToolDispatchResult {
            tool_name: "directive_return".to_string(),
            call_id,
            arguments: args.clone(),
            canonical_ref: "directive_return".to_string(),
            dispatch_kind: DispatchKind::Internal,
        })
    }

    pub fn is_directive_return(&self, tool_name: &str) -> bool {
        tool_name == "directive_return"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_tracing::test as trace_test;

    fn make_dispatcher(
        caps: Vec<String>,
        outputs: Option<Vec<OutputSpec>>,
    ) -> Dispatcher {
        let tools = vec![
            ToolSchema {
                name: "read_file".to_string(),
                item_id: "tool:read_file".to_string(),
                description: None,
                input_schema: None,
            },
            ToolSchema {
                name: "write_file".to_string(),
                item_id: "tool:write_file".to_string(),
                description: None,
                input_schema: None,
            },
        ];
        Dispatcher::new(
            tools,
            outputs,
            caps,
        )
    }

    #[test]
    fn resolve_known_tool() {
        let d = make_dispatcher(vec!["rye.execute.tool.*".to_string()], None);
        let result = d.resolve("read_file", r#"{"path": "/tmp"}"#, None).unwrap();
        assert_eq!(result.tool_name, "read_file");
        assert_eq!(result.arguments["path"], "/tmp");
        assert_eq!(result.dispatch_kind, DispatchKind::Tool);
    }

    #[test]
    fn resolve_unknown_tool_fails() {
        let d = make_dispatcher(vec!["rye.execute.tool.*".to_string()], None);
        assert!(d.resolve("nonexistent", "{}", None).is_err());
    }

    #[test]
    fn resolve_permission_denied() {
        let d = make_dispatcher(
            vec!["rye.execute.tool.read_file".to_string()],
            None,
        );
        assert!(d.resolve("write_file", "{}", None).is_err());
    }

    #[test]
    fn resolve_permission_wildcard() {
        let d = make_dispatcher(vec!["rye.execute.tool.*".to_string()], None);
        assert!(d.resolve("write_file", "{}", None).is_ok());
    }

    #[test]
    fn resolve_passes_call_id() {
        let d = make_dispatcher(vec!["rye.execute.tool.*".to_string()], None);
        let result = d.resolve("read_file", r#"{"path": "/tmp"}"#, Some("call_42".to_string())).unwrap();
        assert_eq!(result.call_id.as_deref(), Some("call_42"));
    }

    #[test]
    fn directive_return_validates_outputs() {
        let outputs = Some(vec![OutputSpec {
            name: "answer".to_string(),
            description: None,
            r#type: None,
        }]);
        let d = make_dispatcher(vec![], outputs);
        let result = d
            .resolve("directive_return", r#"{"answer": "42"}"#, None)
            .unwrap();
        assert_eq!(result.dispatch_kind, DispatchKind::Internal);
        assert_eq!(result.arguments["answer"], "42");
    }

    #[test]
    fn directive_return_missing_output() {
        let outputs = Some(vec![OutputSpec {
            name: "answer".to_string(),
            description: None,
            r#type: None,
        }]);
        let d = make_dispatcher(vec![], outputs);
        assert!(d
            .resolve("directive_return", r#"{"wrong": "value"}"#, None)
            .is_err());
    }

    #[test]
    fn directive_return_no_outputs_declared() {
        let d = make_dispatcher(vec![], None);
        let result = d
            .resolve("directive_return", r#"{"anything": "goes"}"#, None)
            .unwrap();
        assert_eq!(result.dispatch_kind, DispatchKind::Internal);
    }

    #[test]
    fn arg_repair_on_invalid_json_bails_typed() {
        let d = make_dispatcher(vec!["rye.execute.tool.*".to_string()], None);
        let err = d
            .resolve("read_file", r#"{path: "/tmp"}"#, None)
            .unwrap_err();
        assert!(
            err.contains("malformed tool-call arguments JSON"),
            "expected typed dispatcher bail, got: {err}"
        );
    }

    #[test]
    fn is_directive_return_check() {
        let d = make_dispatcher(vec![], None);
        assert!(d.is_directive_return("directive_return"));
        assert!(!d.is_directive_return("read_file"));
    }

    #[test]
    fn classify_dispatch_correct() {
        assert_eq!(classify_dispatch("tool:read_file", false), DispatchKind::Tool);
        assert_eq!(classify_dispatch("directive:my/work", false), DispatchKind::DirectiveChild);
        assert_eq!(classify_dispatch("graph:my/graph", false), DispatchKind::GraphChild);
        assert_eq!(classify_dispatch("directive_return", true), DispatchKind::Internal);
    }

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn resolve_emits_span() {
        let d = make_dispatcher(vec!["rye.execute.tool.*".to_string()], None);
        let (_, spans) = trace_test::capture_traces(|| {
            let _ = d.resolve("read_file", r#"{"path": "/tmp"}"#, None);
        });

        let span = trace_test::find_span(&spans, "tool:resolve");
        assert!(span.is_some(), "expected tool:resolve span, got: {:?}", spans.iter().map(|s: &ryeos_tracing::test::RecordedSpan| &s.name).collect::<Vec<_>>());

        let span = span.unwrap();
        let field_val = |name: &str| -> Option<&str> {
            span.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
        };
        assert_eq!(field_val("tool_name"), Some("read_file"));
    }
}
