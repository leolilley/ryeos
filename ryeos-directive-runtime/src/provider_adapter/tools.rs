use serde_json::{json, Value};

use crate::directive::{MessageSchemas, ToolSchema};

pub fn serialize_tools(tools: &[ToolSchema], schemas: &Option<MessageSchemas>) -> Value {
    match schemas {
        None => serialize_openai_tools(tools),
        Some(s) => serialize_with_schemas(tools, s),
    }
}

fn serialize_openai_tools(tools: &[ToolSchema]) -> Value {
    json!(tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect::<Vec<_>>())
}

fn serialize_with_schemas(tools: &[ToolSchema], schemas: &MessageSchemas) -> Value {
    let param_key = schemas
        .content_key
        .as_deref()
        .unwrap_or("parameters");
    let tool_list_wrap = schemas.tool_list_wrap.as_deref();

    let list: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut func_obj = json!({
                "name": t.name,
                "description": t.description,
            });
            if let Some(ref schema) = t.input_schema {
                func_obj[param_key] = schema.clone();
            }
            func_obj
        })
        .collect();

    if let Some(wrap) = tool_list_wrap {
        let mut wrapped = json!({});
        wrapped[wrap] = json!(list);
        wrapped
    } else {
        json!(list
            .iter()
            .map(|func| {
                json!({
                    "type": "function",
                    "function": func,
                })
            })
            .collect::<Vec<_>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::{MessageSchemas, ToolSchema};

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
        assert_eq!(arr[1]["function"]["parameters"], Value::Null);
    }

    #[test]
    fn tool_list_wrap_gemini_style() {
        let tools = sample_tools();
        let schemas = MessageSchemas {
            role_map: None,
            content_key: None,
            content_wrap: None,
            system_message: None,
            tool_result: None,
            tool_list_wrap: Some("function_declarations".to_string()),
        };
        let result = serialize_tools(&tools, &Some(schemas));
        let decls = result.get("function_declarations").unwrap().as_array().unwrap();
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0]["name"], "bash");
        assert_eq!(decls[0]["parameters"]["type"], "object");
    }

    #[test]
    fn custom_content_key_renames_parameters() {
        let tools = sample_tools();
        let schemas = MessageSchemas {
            role_map: None,
            content_key: Some("inputSchema".to_string()),
            content_wrap: None,
            system_message: None,
            tool_result: None,
            tool_list_wrap: None,
        };
        let result = serialize_tools(&tools, &Some(schemas));
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0]["function"]["inputSchema"]["type"], "object");
        assert!(arr[0]["function"].get("parameters").is_none());
    }

    #[test]
    fn empty_tools() {
        let result = serialize_tools(&[], &None);
        assert_eq!(result.as_array().unwrap().len(), 0);
    }

    #[test]
    fn wrap_with_custom_content_key() {
        let tools = sample_tools();
        let schemas = MessageSchemas {
            role_map: None,
            content_key: Some("inputSchema".to_string()),
            content_wrap: None,
            system_message: None,
            tool_result: None,
            tool_list_wrap: Some("tools".to_string()),
        };
        let result = serialize_tools(&tools, &Some(schemas));
        let tools_list = result.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools_list[0]["inputSchema"]["type"], "object");
        assert!(tools_list[0].get("parameters").is_none());
    }
}
