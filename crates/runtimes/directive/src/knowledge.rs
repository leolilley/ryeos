use std::path::Path;

use anyhow::Result;

use crate::directive::{ProviderMessage, ToolSchema};

pub fn write_thread_transcript(
    project_path: &Path,
    thread_id: &str,
    directive_ref: &str,
    messages: &[ProviderMessage],
) -> Result<()> {
    let dir = project_path
        .join(".ai/knowledge/state/threads")
        .join(thread_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{thread_id}.md"));

    let mut md = format!(
        "# Thread: {thread_id}\n\n\
         **Directive**: `{directive_ref}`\n\
         **Completed**: {}\n\n",
        lillux::time::iso8601_now(),
    );

    for msg in messages {
        match msg.role.as_str() {
            "system" => {
                if let Some(ref content) = msg.content {
                    md.push_str("### System\n\n");
                    md.push_str(&format_content(content));
                    md.push_str("\n\n");
                }
            }
            "user" => {
                if let Some(ref content) = msg.content {
                    md.push_str("### User\n\n");
                    md.push_str(&format_content(content));
                    md.push_str("\n\n");
                }
            }
            "assistant" => {
                if let Some(ref tool_calls) = msg.tool_calls {
                    if !tool_calls.is_empty() {
                        md.push_str("### Assistant\n\n");
                        for tc in tool_calls {
                            md.push_str(&format!(
                                "**Tool call**: `{}`\n```json\n{}\n```\n\n",
                                tc.name,
                                serde_json::to_string_pretty(&tc.arguments)
                                    .unwrap_or_else(|_| tc.arguments.to_string()),
                            ));
                        }
                    }
                }
                if let Some(ref content) = msg.content {
                    if !content.is_null() {
                        md.push_str("### Assistant\n\n");
                        md.push_str(&format_content(content));
                        md.push_str("\n\n");
                    }
                }
            }
            "tool" => {
                md.push_str("### Tool Result\n\n");
                if let Some(ref content) = msg.content {
                    md.push_str(&format_content(content));
                }
                md.push_str("\n\n");
            }
            _ => {}
        }
    }

    std::fs::write(&path, md)?;
    Ok(())
}

pub fn write_capabilities(
    project_path: &Path,
    thread_id: &str,
    tools: &[ToolSchema],
    project_tree: Option<&str>,
) -> Result<()> {
    let dir = project_path
        .join(".ai/knowledge/state/threads")
        .join(thread_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("capabilities.md");

    let mut md = format!(
        "# Capabilities: {thread_id}\n\n\
         **Generated**: {}\n\n",
        lillux::time::iso8601_now(),
    );

    md.push_str("## Available Tools\n\n");
    for tool in tools {
        md.push_str(&format!("- **{}** (`{}`)", tool.name, tool.item_id));
        if let Some(ref desc) = tool.description {
            md.push_str(&format!(": {}", desc));
        }
        md.push('\n');
    }

    if let Some(tree) = project_tree {
        md.push_str("\n## Project Tree\n\n```\n");
        md.push_str(tree);
        md.push_str("\n```\n");
    }

    std::fs::write(&path, md)?;
    Ok(())
}

fn format_content(value: &serde_json::Value) -> String {
    match value.as_str() {
        Some(s) => s.to_string(),
        None => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn write_thread_transcript_creates_file() {
        let dir = TempDir::new().unwrap();
        let messages = vec![
            ProviderMessage {
                role: "user".to_string(),
                content: Some(json!("hello")),
                tool_calls: None,
                tool_call_id: None,
            },
            ProviderMessage {
                role: "assistant".to_string(),
                content: Some(json!("hi there")),
                tool_calls: Some(vec![]),
                tool_call_id: None,
            },
        ];
        write_thread_transcript(dir.path(), "T-abc", "my/directive", &messages).unwrap();
        let content = std::fs::read_to_string(
            dir.path().join(".ai/knowledge/state/threads/T-abc/T-abc.md"),
        )
        .unwrap();
        assert!(content.contains("T-abc"));
        assert!(content.contains("my/directive"));
        assert!(content.contains("hello"));
        assert!(content.contains("hi there"));
    }

    #[test]
    fn write_capabilities_creates_file() {
        let dir = TempDir::new().unwrap();
        let tools = vec![
            ToolSchema {
                name: "read_file".to_string(),
                item_id: "tool:read_file".to_string(),
                description: Some("Read a file".to_string()),
                input_schema: None,
            },
        ];
        write_capabilities(dir.path(), "T-abc", &tools, None).unwrap();
        let content = std::fs::read_to_string(
            dir.path().join(".ai/knowledge/state/threads/T-abc/capabilities.md"),
        )
        .unwrap();
        assert!(content.contains("read_file"));
        assert!(content.contains("Read a file"));
    }

    #[test]
    fn write_capabilities_with_tree() {
        let dir = TempDir::new().unwrap();
        let tools = vec![];
        write_capabilities(dir.path(), "T-abc", &tools, Some("src/\n  main.rs")).unwrap();
        let content = std::fs::read_to_string(
            dir.path().join(".ai/knowledge/state/threads/T-abc/capabilities.md"),
        )
        .unwrap();
        assert!(content.contains("Project Tree"));
        assert!(content.contains("src/"));
    }
}
