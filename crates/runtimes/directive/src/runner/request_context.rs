use serde_json::{json, Map, Value};

use crate::directive::{OutputSpec, ProviderMessage, ToolSchema};

pub(super) fn initial_messages(
    messages: Vec<ProviderMessage>,
    system_prompt: Option<&str>,
) -> Vec<ProviderMessage> {
    let mut initial_messages = Vec::new();
    if let Some(system_prompt) = system_prompt {
        initial_messages.push(ProviderMessage {
            role: "system".to_string(),
            content: Some(json!(system_prompt)),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        });
    }
    initial_messages.extend(messages);
    initial_messages
}

pub(super) fn visible_provider_tools(
    tools: &[ToolSchema],
    effective_caps: &[String],
    directive_outputs: Option<&[OutputSpec]>,
) -> Vec<ToolSchema> {
    let visible_tools = crate::provider_adapter::tools::filter_tools_by_caps(tools, effective_caps);
    let mut visible_tools_owned: Vec<_> = visible_tools.into_iter().cloned().collect();

    // The lifecycle tool follows filtered dispatchable tools. It is intercepted
    // by the runner and never reaches capability lookup or tool dispatch.
    if let Some(outputs) = directive_outputs.filter(|outputs| !outputs.is_empty()) {
        visible_tools_owned.push(build_directive_return_tool(outputs));
    }
    visible_tools_owned
}

/// Synthesize the lifecycle-only `directive_return` provider tool.
fn build_directive_return_tool(outputs: &[OutputSpec]) -> ToolSchema {
    let mut props = Map::new();
    let mut required: Vec<Value> = Vec::with_capacity(outputs.len());
    for output in outputs {
        let mut property = Map::new();
        property.insert(
            "type".to_string(),
            json!(output
                .r#type
                .clone()
                .unwrap_or_else(|| "string".to_string())),
        );
        if let Some(description) = &output.description {
            property.insert("description".to_string(), json!(description));
        }
        props.insert(output.name.clone(), Value::Object(property));
        required.push(json!(output.name));
    }
    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(props));
    schema.insert("required".to_string(), Value::Array(required));
    ToolSchema {
        name: "directive_return".to_string(),
        item_id: "lifecycle:directive_return".to_string(),
        description: Some(
            "Return final structured outputs and finish the directive. \
             Call this exactly once when you have a complete answer."
                .to_string(),
        ),
        input_schema: Some(Value::Object(schema)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_message() -> ProviderMessage {
        ProviderMessage {
            role: "user".to_string(),
            content: Some(json!("hello")),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    #[test]
    fn system_prompt_is_prepended_without_reordering_messages() {
        let messages = initial_messages(vec![user_message()], Some("You are helpful"));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, Some(json!("You are helpful")));
        assert_eq!(messages[1].role, "user");
    }

    #[test]
    fn directive_return_schema_preserves_output_order_and_defaults() {
        let tool = build_directive_return_tool(&[
            OutputSpec {
                name: "answer".to_string(),
                description: Some("Final answer".to_string()),
                r#type: None,
            },
            OutputSpec {
                name: "confidence".to_string(),
                description: None,
                r#type: Some("number".to_string()),
            },
        ]);

        assert_eq!(tool.name, "directive_return");
        assert_eq!(tool.item_id, "lifecycle:directive_return");
        let schema = tool.input_schema.expect("directive return schema");
        assert_eq!(schema["required"], json!(["answer", "confidence"]));
        assert_eq!(schema["properties"]["answer"]["type"], "string");
        assert_eq!(
            schema["properties"]["answer"]["description"],
            "Final answer"
        );
        assert_eq!(schema["properties"]["confidence"]["type"], "number");
    }

    #[test]
    fn empty_outputs_do_not_advertise_directive_return() {
        assert!(visible_provider_tools(&[], &[], Some(&[])).is_empty());
    }
}
