use anyhow::{bail, Result};

use crate::directive::ParsedDirective;

pub fn parse_yaml_frontmatter(content: &str) -> Result<ParsedDirective> {
    if !content.starts_with("---") {
        bail!("expected YAML frontmatter starting with '---'");
    }

    let after_first = &content[3..];
    let rest = after_first.trim_start_matches(['\r', '\n']);

    let close_pos = if rest.starts_with("---") {
        Some(0)
    } else {
        rest.find("\n---")
            .or_else(|| rest.find("\r\n---"))
    };

    let close_pos = close_pos.ok_or_else(|| anyhow::anyhow!("no closing '---' found in frontmatter"))?;

    let header_yaml = &rest[..close_pos];
    let after_close = &rest[close_pos..];

    let body_start = if after_close.starts_with("---") {
        3
    } else if after_close.starts_with("\r\n---") {
        5
    } else if after_close.starts_with("\n---") {
        4
    } else {
        3
    };
    let body = after_close[body_start..].trim_start_matches(['\r', '\n']).to_string();

    let header: crate::directive::DirectiveHeader = serde_yaml::from_str(header_yaml)?;

    Ok(ParsedDirective { header, body })
}

pub fn parse_legacy_xml(content: &str) -> Result<ParsedDirective> {
    let mut header_lines = Vec::new();
    let mut body_lines = Vec::new();
    let mut found_close = false;

    for line in content.lines() {
        if !found_close {
            if line.trim() == "```" {
                found_close = true;
                continue;
            }
            let trimmed = line.trim();
            if trimmed.starts_with("<") && !trimmed.starts_with("<!--") {
                header_lines.push(line.to_string());
            } else if trimmed.is_empty() {
            } else {
                header_lines.push(line.to_string());
            }
        } else {
            body_lines.push(line.to_string());
        }
    }

    let body = body_lines.join("\n");

    let mut name = None;
    let mut extends = None;
    let mut model_tier = None;
    let mut permissions_execute = Vec::new();

    for line in &header_lines {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("<name>") {
            if let Some(val) = rest.strip_suffix("</name>") {
                name = Some(val.trim().to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("<extends>") {
            if let Some(val) = rest.strip_suffix("</extends>") {
                extends = Some(val.trim().to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("<model_tier>") {
            if let Some(val) = rest.strip_suffix("</model_tier>") {
                model_tier = Some(val.trim().to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("<permission>") {
            if let Some(val) = rest.strip_suffix("</permission>") {
                permissions_execute.push(val.trim().to_string());
            }
        }
    }

    let header = crate::directive::DirectiveHeader {
        name,
        extends,
        model: model_tier.map(|t| crate::directive::ModelSpec {
            tier: Some(t),
            provider: None,
            name: None,
        }),
        permissions: if permissions_execute.is_empty() {
            None
        } else {
            Some(crate::directive::PermissionsSpec {
                execute: permissions_execute,
                fetch: vec![],
                sign: vec![],
            })
        },
        limits: None,
        outputs: None,
        context: None,
        hooks: None,
        extra: std::collections::HashMap::new(),
    };

    Ok(ParsedDirective { header, body })
}

pub fn parse_directive(content: &str, path: &str) -> Result<ParsedDirective> {
    if path.ends_with(".directive.md") || content.starts_with("---") {
        parse_yaml_frontmatter(content)
    } else {
        parse_legacy_xml(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_frontmatter_basic() {
        let content = "---\nname: my/agent\nmodel:\n  tier: general\n---\nBody text here.";
        let parsed = parse_yaml_frontmatter(content).unwrap();
        assert_eq!(parsed.header.name.as_deref(), Some("my/agent"));
        assert_eq!(parsed.header.model.as_ref().and_then(|m| m.tier.as_deref()), Some("general"));
        assert_eq!(parsed.body, "Body text here.");
    }

    #[test]
    fn yaml_frontmatter_with_permissions() {
        let content = "---\nname: test\npermissions:\n  execute:\n    - rye.execute.tool.*\n---\nDo the thing.";
        let parsed = parse_yaml_frontmatter(content).unwrap();
        let perms = parsed.header.permissions.unwrap();
        assert_eq!(perms.execute, vec!["rye.execute.tool.*"]);
    }

    #[test]
    fn yaml_missing_close_delimiter() {
        let content = "---\nname: test\n\nNo close.";
        assert!(parse_yaml_frontmatter(content).is_err());
    }

    #[test]
    fn body_never_parsed_as_metadata() {
        let content = "---\nname: test\n---\nThis has --- separators in body.\n---\nAnd more.";
        let parsed = parse_yaml_frontmatter(content).unwrap();
        assert_eq!(parsed.header.name.as_deref(), Some("test"));
        assert!(parsed.body.contains("---"));
        assert!(parsed.body.contains("separators in body"));
    }

    #[test]
    fn legacy_xml_basic() {
        let content = "<name>my-agent</name>\n<extends>base</extends>\n```\nBody text.";
        let parsed = parse_legacy_xml(content).unwrap();
        assert_eq!(parsed.header.name.as_deref(), Some("my-agent"));
        assert_eq!(parsed.header.extends.as_deref(), Some("base"));
        assert_eq!(parsed.body, "Body text.");
    }

    #[test]
    fn parse_dispatches_by_content() {
        let yaml = "---\nname: yaml-style\n---\nBody.";
        assert_eq!(parse_directive(yaml, "test.directive.md").unwrap().header.name.unwrap(), "yaml-style");

        let xml = "<name>xml-style</name>\n```\nBody.";
        assert_eq!(parse_directive(xml, "test.md").unwrap().header.name.unwrap(), "xml-style");
    }

    #[test]
    fn yaml_empty_frontmatter() {
        let content = "---\n---\nBody text.";
        let parsed = parse_yaml_frontmatter(content).unwrap();
        assert_eq!(parsed.body, "Body text.");
    }

    #[test]
    fn yaml_frontmatter_with_code_fence_body() {
        let content = "---\nname: test\n---\n```python\nprint(\"hello\")\n```\n\nSome text with --- in it.";
        let parsed = parse_yaml_frontmatter(content).unwrap();
        assert_eq!(parsed.header.name.as_deref(), Some("test"));
        assert!(parsed.body.contains("```python"));
        assert!(parsed.body.contains("--- in it"));
    }

    #[test]
    fn yaml_body_with_triple_dashes() {
        let content = "---\nname: test\n---\nFirst part.\n---\nSecond part.\n---\nThird part.";
        let parsed = parse_yaml_frontmatter(content).unwrap();
        assert_eq!(parsed.header.name.as_deref(), Some("test"));
        assert!(parsed.body.contains("First part."));
        assert!(parsed.body.contains("---\nSecond part."));
    }
}
