use serde_json::Value;

use crate::adapter::parse_tool_arguments;
use crate::directive::{OutputSpec, ToolSchema};

#[derive(Debug, Clone)]
pub struct ToolDispatchResult {
    pub tool_name: String,
    pub call_id: Option<String>,
    pub arguments: Value,
    pub canonical_ref: String,
    pub is_internal: bool,
}

pub struct Dispatcher {
    tools: Vec<ToolSchema>,
    outputs: Option<Vec<OutputSpec>>,
    effective_caps: Vec<String>,
    allowed_primaries: Vec<String>,
}

impl Dispatcher {
    pub fn new(
        tools: Vec<ToolSchema>,
        outputs: Option<Vec<OutputSpec>>,
        effective_caps: Vec<String>,
        allowed_primaries: Vec<String>,
    ) -> Self {
        Self {
            tools,
            outputs,
            effective_caps,
            allowed_primaries,
        }
    }

    pub fn resolve(&self, tool_name: &str, raw_args: &str) -> Result<ToolDispatchResult, String> {
        let arguments = parse_tool_arguments(raw_args);

        if tool_name == "directive_return" {
            return self.handle_directive_return(&arguments);
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

        Ok(ToolDispatchResult {
            tool_name: tool_name.to_string(),
            call_id: None,
            arguments,
            canonical_ref,
            is_internal,
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
        rye_runtime::cap_matches(&self.effective_caps.join(","), required)
            || self
                .effective_caps
                .iter()
                .any(|cap| rye_runtime::cap_matches(cap, required))
    }

    fn handle_directive_return(&self, args: &Value) -> Result<ToolDispatchResult, String> {
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
            call_id: None,
            arguments: args.clone(),
            canonical_ref: "directive_return".to_string(),
            is_internal: true,
        })
    }

    pub fn is_directive_return(&self, tool_name: &str) -> bool {
        tool_name == "directive_return"
    }

    pub fn validate_allowed_primary(&self, primary: &str) -> bool {
        self.allowed_primaries
            .iter()
            .any(|p| p == primary || p == "*")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            vec!["execute".to_string(), "fetch".to_string()],
        )
    }

    #[test]
    fn resolve_known_tool() {
        let d = make_dispatcher(vec!["rye.execute.tool.*".to_string()], None);
        let result = d.resolve("read_file", r#"{"path": "/tmp"}"#).unwrap();
        assert_eq!(result.tool_name, "read_file");
        assert_eq!(result.arguments["path"], "/tmp");
        assert!(!result.is_internal);
    }

    #[test]
    fn resolve_unknown_tool_fails() {
        let d = make_dispatcher(vec!["rye.execute.tool.*".to_string()], None);
        assert!(d.resolve("nonexistent", "{}").is_err());
    }

    #[test]
    fn resolve_permission_denied() {
        let d = make_dispatcher(
            vec!["rye.execute.tool.read_file".to_string()],
            None,
        );
        assert!(d.resolve("write_file", "{}").is_err());
    }

    #[test]
    fn resolve_permission_wildcard() {
        let d = make_dispatcher(vec!["rye.execute.tool.*".to_string()], None);
        assert!(d.resolve("write_file", "{}").is_ok());
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
            .resolve("directive_return", r#"{"answer": "42"}"#)
            .unwrap();
        assert!(result.is_internal);
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
            .resolve("directive_return", r#"{"wrong": "value"}"#)
            .is_err());
    }

    #[test]
    fn directive_return_no_outputs_declared() {
        let d = make_dispatcher(vec![], None);
        let result = d
            .resolve("directive_return", r#"{"anything": "goes"}"#)
            .unwrap();
        assert!(result.is_internal);
    }

    #[test]
    fn arg_repair_on_invalid_json() {
        let d = make_dispatcher(vec!["rye.execute.tool.*".to_string()], None);
        let result = d
            .resolve("read_file", r#"{path: "/tmp"}"#)
            .unwrap();
        assert!(result.arguments.is_object());
    }

    #[test]
    fn is_directive_return_check() {
        let d = make_dispatcher(vec![], None);
        assert!(d.is_directive_return("directive_return"));
        assert!(!d.is_directive_return("read_file"));
    }

    #[test]
    fn validate_allowed_primary() {
        let d = make_dispatcher(vec![], None);
        assert!(d.validate_allowed_primary("execute"));
        assert!(d.validate_allowed_primary("fetch"));
        assert!(!d.validate_allowed_primary("admin"));
    }
}
