use serde_json::{json, Value};

use crate::directive::{ToolSchema, ToolSchemaConfig};

pub fn serialize_tools(
    tools: &[ToolSchema],
    tool_schema: &Option<ToolSchemaConfig>,
) -> Value {
    match tool_schema {
        Some(ts) => serialize_with_template(tools, ts),
        None => serialize_openai_default(tools),
    }
}

/// Template-driven tool serialization.
///
/// For each tool, render `template` with context:
///   {"name": <name>, "description": <description>, "schema": <input_schema>}
///
/// If `list_wrap` is set, wrap all rendered tools into a single
/// element under that key. Gemini: `[{functionDeclarations: [...]}]`.
/// Otherwise, return the rendered tools as a flat array.
fn serialize_with_template(tools: &[ToolSchema], cfg: &ToolSchemaConfig) -> Value {
    use ryeos_runtime::template::apply_template;
    use std::collections::HashMap;

    // Empty tools is unconditionally serialized as `[]` regardless of
    // list_wrap. Sending `[{functionDeclarations: []}]` to Gemini would
    // be a wrong shape; `[]` is the universally-accepted "no tools" form.
    if tools.is_empty() {
        return json!([]);
    }

    let rendered: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut ctx: HashMap<String, Value> = HashMap::new();
            ctx.insert("name".to_string(), json!(t.name));
            ctx.insert(
                "description".to_string(),
                json!(t.description.clone().unwrap_or_default()),
            );
            ctx.insert(
                "schema".to_string(),
                t.input_schema.clone().unwrap_or_else(empty_object_schema),
            );
            apply_template(&cfg.template, &ctx)
        })
        .collect();

    match &cfg.list_wrap {
        Some(wrap_key) => json!([{ wrap_key.as_str(): rendered }]),
        None => Value::Array(rendered),
    }
}

/// Filter `tools` down to those the directive's effective_caps actually
/// permit calling. Anything the dispatcher would reject at call time
/// is removed here so the LLM never sees it. Saves context, removes
/// confusion, prevents the "model tries to call something it can't"
/// error path from being entered at all.
pub fn filter_tools_by_caps<'a>(
    tools: &'a [ToolSchema],
    effective_caps: &[String],
) -> Vec<&'a ToolSchema> {
    tools
        .iter()
        .filter(|t| {
            let required = format!("ryeos.execute.tool.{}", t.item_id);
            effective_caps
                .iter()
                .any(|cap| ryeos_runtime::cap_matches(cap, &required))
        })
        .collect()
}

fn empty_object_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

/// Default OpenAI tool serialization — used when no ToolSchemaConfig
/// is provided (e.g. test code, legacy profiles). Produces the standard
/// `{type: "function", function: {name, description, parameters}}` shape.
fn serialize_openai_default(tools: &[ToolSchema]) -> Value {
    json!(tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema.clone().unwrap_or_else(empty_object_schema),
                }
            })
        })
        .collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::ToolSchema;

    fn sample_tools() -> Vec<ToolSchema> {
        vec![
            ToolSchema {
                name: "bash".to_string(),
                item_id: "ryeos/bash/bash".to_string(),
                description: Some("Run a bash command".to_string()),
                input_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"}
                    },
                    "required": ["command"]
                })),
            },
            ToolSchema {
                name: "read_file".to_string(),
                item_id: "ryeos/core/read".to_string(),
                description: Some("Read a file".to_string()),
                input_schema: None,
            },
        ]
    }

    #[test]
    fn default_openai_tool_format() {
        let tools = sample_tools();
        let result = serialize_tools(&tools, &None);
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "bash");
        assert_eq!(
            arr[0]["function"]["description"],
            "Run a bash command"
        );
        assert_eq!(
            arr[0]["function"]["parameters"]["type"],
            "object"
        );
    assert_eq!(arr[1]["function"]["name"], "read_file");
    assert_eq!(
        arr[1]["function"]["parameters"]["type"], "object",
        "tools without input_schema should get empty-object default, not null"
    );
    }

    #[test]
    fn empty_tools() {
        let result = serialize_tools(&[], &None);
        assert_eq!(result.as_array().unwrap().len(), 0);
    }

    #[test]
    fn openai_tool_with_no_schema_gets_empty_object_default() {
        let tools = vec![ToolSchema {
            name: "no_schema_tool".to_string(),
            item_id: "test/no-schema".to_string(),
            description: Some("A tool with no schema".to_string()),
            input_schema: None,
        }];
        let result = serialize_tools(&tools, &None);
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0]["function"]["parameters"]["type"], "object");
        assert!(arr[0]["function"]["parameters"]["properties"].is_object());
    }

    #[test]
    fn permission_filter_keeps_only_matching_tools() {
        let tools = vec![
            ToolSchema {
                name: "read".to_string(),
                item_id: "ryeos/file-system/read".to_string(),
                description: Some("Read a file".to_string()),
                input_schema: None,
            },
            ToolSchema {
                name: "write".to_string(),
                item_id: "ryeos/file-system/write".to_string(),
                description: Some("Write a file".to_string()),
                input_schema: None,
            },
        ];
        let caps = vec!["ryeos.execute.tool.ryeos/file-system/read".to_string()];
        let filtered = filter_tools_by_caps(&tools, &caps);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "read");
    }

    #[test]
    fn permission_filter_wildcard_allows_all() {
        let tools = vec![
            ToolSchema {
                name: "read".to_string(),
                item_id: "ryeos/file-system/read".to_string(),
                description: Some("Read".to_string()),
                input_schema: None,
            },
            ToolSchema {
                name: "write".to_string(),
                item_id: "ryeos/file-system/write".to_string(),
                description: Some("Write".to_string()),
                input_schema: None,
            },
        ];
        let caps = vec!["ryeos.execute.tool.*".to_string()];
        let filtered = filter_tools_by_caps(&tools, &caps);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn permission_filter_no_caps_removes_all() {
        let tools = vec![ToolSchema {
            name: "read".to_string(),
            item_id: "ryeos/file-system/read".to_string(),
            description: Some("Read".to_string()),
            input_schema: None,
        }];
        let caps: Vec<String> = vec![];
        let filtered = filter_tools_by_caps(&tools, &caps);
        assert!(filtered.is_empty());
    }

    #[test]
    fn serialize_tools_with_gemini_template_produces_function_declarations_wrap() {
        use crate::directive::ToolSchemaConfig;
        let tools = sample_tools();
        let cfg = ToolSchemaConfig {
            template: json!({
                "name": "{name}",
                "description": "{description}",
                "parameters": "{schema}",
            }),
            list_wrap: Some("functionDeclarations".to_string()),
        };
        let result = serialize_tools(&tools, &Some(cfg));
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1, "list_wrap collapses tools into one element");
        let decls = arr[0].get("functionDeclarations").unwrap().as_array().unwrap();
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0]["name"], "bash");
        assert_eq!(decls[0]["parameters"]["type"], "object");
        // The OpenAI {type:function, function:{...}} wrapper MUST NOT appear.
        assert!(decls[0].get("type").is_none());
        assert!(decls[0].get("function").is_none());
    }

    #[test]
    fn serialize_tools_with_anthropic_template_produces_input_schema_no_function_wrapper() {
        use crate::directive::ToolSchemaConfig;
        let tools = sample_tools();
        let cfg = ToolSchemaConfig {
            template: json!({
                "name": "{name}",
                "description": "{description}",
                "input_schema": "{schema}",
            }),
            list_wrap: None,
        };
        let result = serialize_tools(&tools, &Some(cfg));
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "bash");
        assert!(arr[0].get("input_schema").is_some());
        assert!(arr[0].get("type").is_none(), "no function wrapper for anthropic");
    }

    #[test]
    fn serialize_tools_with_openai_template_keeps_function_wrapper() {
        use crate::directive::ToolSchemaConfig;
        let tools = sample_tools();
        let cfg = ToolSchemaConfig {
            template: json!({
                "type": "function",
                "function": {
                    "name": "{name}",
                    "description": "{description}",
                    "parameters": "{schema}",
                },
            }),
            list_wrap: None,
        };
        let result = serialize_tools(&tools, &Some(cfg));
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0]["type"], "function");
        assert_eq!(arr[0]["function"]["name"], "bash");
        assert_eq!(arr[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn serialize_tools_empty_returns_flat_empty_array_even_with_list_wrap() {
        // Critical: Gemini empty-tools must NOT become [{functionDeclarations: []}].
        use crate::directive::ToolSchemaConfig;
        let cfg = ToolSchemaConfig {
            template: json!({
                "name": "{name}",
                "description": "{description}",
                "parameters": "{schema}",
            }),
            list_wrap: Some("functionDeclarations".to_string()),
        };
        let result = serialize_tools(&[], &Some(cfg));
        assert_eq!(result, json!([]),
            "empty tools must serialize as `[]` regardless of list_wrap; \
             got: {}", result);
    }
}
