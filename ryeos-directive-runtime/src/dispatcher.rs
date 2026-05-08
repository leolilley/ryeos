use serde_json::Value;

use crate::adapter::parse_tool_arguments;
use crate::directive::ToolSchema;

#[derive(Debug, Clone, PartialEq)]
pub enum DispatchKind {
    Tool,
    DirectiveChild,
    GraphChild,
}

#[derive(Debug, Clone)]
pub struct ToolDispatchResult {
    pub call_id: Option<String>,
    pub arguments: Value,
    pub canonical_ref: String,
    pub dispatch_kind: DispatchKind,
}

fn classify_dispatch(canonical_ref: &str) -> DispatchKind {
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
    effective_caps: Vec<String>,
}

impl Dispatcher {
    pub fn new(
        tools: Vec<ToolSchema>,
        effective_caps: Vec<String>,
    ) -> Self {
        Self {
            tools,
            effective_caps,
        }
    }

    #[tracing::instrument(name = "tool:resolve", skip(self, raw_args), fields(tool_name = %tool_name))]
    pub fn resolve(&self, tool_name: &str, raw_args: &str, call_id: Option<String>) -> Result<ToolDispatchResult, String> {
        // directive_return is a lifecycle signal, not a tool — the
        // runner intercepts it by name before reaching this path.
        // If we see it here, the runner bypass was missed.
        if tool_name == "directive_return" {
            return Err("directive_return is a lifecycle signal, not a dispatchable tool".to_string());
        }

        // Typed-fail-loud per remediation: malformed tool-call args
        // bail at the dispatcher rather than dispatching with `{}`.
        let arguments = parse_tool_arguments(raw_args)?;

        let canonical_ref = self
            .find_tool(tool_name)
            .ok_or_else(|| format!("unknown tool: {}", tool_name))?;

        let required_cap = format!("ryeos.execute.tool.{}", canonical_ref);
        if !self.check_permission(&required_cap) {
            return Err(format!(
                "permission denied: {} (no matching capability)",
                required_cap
            ));
        }

        let dispatch_kind = classify_dispatch(&canonical_ref);

        Ok(ToolDispatchResult {
            call_id,
            arguments,
            canonical_ref,
            dispatch_kind,
        })
    }

    fn find_tool(&self, name: &str) -> Option<String> {
        for tool in &self.tools {
            if tool.name == name {
                return Some(tool.item_id.clone());
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_tracing::test as trace_test;

    fn make_dispatcher(caps: Vec<String>) -> Dispatcher {
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
        Dispatcher::new(tools, caps)
    }

    #[test]
    fn resolve_known_tool() {
        let d = make_dispatcher(vec!["ryeos.execute.tool.*".to_string()]);
        let result = d.resolve("read_file", r#"{"path": "/tmp"}"#, None).unwrap();
        assert_eq!(result.arguments["path"], "/tmp");
        assert_eq!(result.canonical_ref, "tool:read_file");
        assert_eq!(result.dispatch_kind, DispatchKind::Tool);
    }

    #[test]
    fn resolve_unknown_tool_fails() {
        let d = make_dispatcher(vec!["ryeos.execute.tool.*".to_string()]);
        assert!(d.resolve("nonexistent", "{}", None).is_err());
    }

    #[test]
    fn resolve_permission_denied() {
        let d = make_dispatcher(vec!["ryeos.execute.tool.read_file".to_string()]);
        assert!(d.resolve("write_file", "{}", None).is_err());
    }

    #[test]
    fn resolve_permission_wildcard() {
        let d = make_dispatcher(vec!["ryeos.execute.tool.*".to_string()]);
        assert!(d.resolve("write_file", "{}", None).is_ok());
    }

    #[test]
    fn resolve_passes_call_id() {
        let d = make_dispatcher(vec!["ryeos.execute.tool.*".to_string()]);
        let result = d.resolve("read_file", r#"{"path": "/tmp"}"#, Some("call_42".to_string())).unwrap();
        assert_eq!(result.call_id.as_deref(), Some("call_42"));
    }

    #[test]
    fn directive_return_rejected_by_dispatcher() {
        // directive_return is a lifecycle signal, not a tool — the
        // dispatcher must refuse it so the runner is forced to
        // intercept by name.
        let d = make_dispatcher(vec![]);
        let err = d.resolve("directive_return", r#"{"answer": "42"}"#, None)
            .unwrap_err();
        assert!(
            err.contains("lifecycle signal"),
            "expected lifecycle-signal rejection, got: {err}"
        );
    }

    #[test]
    fn arg_repair_on_invalid_json_bails_typed() {
        let d = make_dispatcher(vec!["ryeos.execute.tool.*".to_string()]);
        let err = d
            .resolve("read_file", r#"{path: "/tmp"}"#, None)
            .unwrap_err();
        assert!(
            err.contains("malformed tool-call arguments JSON"),
            "expected typed dispatcher bail, got: {err}"
        );
    }

    #[test]
    fn classify_dispatch_correct() {
        assert_eq!(classify_dispatch("tool:read_file"), DispatchKind::Tool);
        assert_eq!(classify_dispatch("directive:my/work"), DispatchKind::DirectiveChild);
        assert_eq!(classify_dispatch("graph:my/graph"), DispatchKind::GraphChild);
    }

    // ── Trace-capture tests ──────────────────────────────────────

    #[test]
    fn resolve_emits_span() {
        let d = make_dispatcher(vec!["ryeos.execute.tool.*".to_string()]);
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
