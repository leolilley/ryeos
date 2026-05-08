//! Frontmatter stripping for markdown and YAML files.

use crate::types::KnowledgeError;

/// Strip YAML frontmatter from markdown content (`---\n...---\n`).
/// Returns the body verbatim. Rejects malformed (unclosed) frontmatter.
pub fn strip_markdown_frontmatter(content: &str) -> Result<String, KnowledgeError> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return Ok(content.to_string());
    }

    // After the opening ---, look for a line that is exactly "---"
    let after_opening = &trimmed[3..];
    // Skip whitespace after opening ---
    let rest = after_opening.trim_start_matches(['\r', '\n']);

    // Find the closing --- on its own line
    let mut found_close = false;
    let mut body_start = 0;
    for (_i, line) in rest.lines().enumerate() {
        if line.trim() == "---" {
            // Found closing ---
            body_start = rest.find(line).unwrap() + line.len();
            found_close = true;
            break;
        }
    }

    if !found_close {
        return Err(KnowledgeError::FrontmatterParse {
            item_id: "<unknown>".to_string(),
            reason: "unclosed markdown frontmatter (missing closing ---)".to_string(),
        });
    }

    let body = rest[body_start..].trim_start_matches(['\r', '\n']);
    Ok(body.to_string())
}

/// Strip signed YAML frontmatter (`# ryeos:signed:...` line + body).
/// Returns the body verbatim.
pub fn strip_signed_yaml_frontmatter(content: &str) -> String {
    let mut lines = content.lines();
    // Skip the signature line
    if let Some(first) = lines.next() {
        if first.trim().starts_with("# ryeos:signed:") || first.trim().starts_with("# ryos: cas:") {
            return lines.collect::<Vec<_>>().join("\n");
        }
    }
    content.to_string()
}

/// Strip frontmatter from content, detecting whether it's markdown or YAML.
pub fn strip_frontmatter(content: &str, item_id: &str) -> Result<String, KnowledgeError> {
    let trimmed = content.trim_start();

    // Markdown frontmatter
    if trimmed.starts_with("---") {
        return strip_markdown_frontmatter(content)
            .map_err(|e| KnowledgeError::FrontmatterParse {
                item_id: item_id.to_string(),
                reason: match e {
                    KnowledgeError::FrontmatterParse { reason, .. } => reason,
                    _ => "unknown frontmatter parse error".to_string(),
                },
            });
    }

    // Signed YAML frontmatter
    if trimmed.starts_with("# ryeos:signed:") || trimmed.starts_with("# ryos: cas:") {
        return Ok(strip_signed_yaml_frontmatter(content));
    }

    Ok(content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_markdown_frontmatter_basic() {
        let input = "---\ntitle: Foo\n---\nBody text";
        let result = strip_markdown_frontmatter(input).unwrap();
        assert_eq!(result, "Body text");
    }

    #[test]
    fn strip_markdown_frontmatter_no_frontmatter() {
        let input = "Just body text";
        let result = strip_markdown_frontmatter(input).unwrap();
        assert_eq!(result, "Just body text");
    }

    #[test]
    fn strip_markdown_frontmatter_unclosed() {
        let input = "---\ntitle: Foo\nBody without closing";
        let result = strip_markdown_frontmatter(input);
        assert!(result.is_err());
    }

    #[test]
    fn strip_signed_yaml_frontmatter_basic() {
        let input = "# ryeos:signed:v1:abc:def:123\nactual body\nmore body";
        let result = strip_signed_yaml_frontmatter(input);
        assert_eq!(result, "actual body\nmore body");
    }

    #[test]
    fn strip_frontmatter_markdown() {
        let input = "---\ntitle: Test\n---\nContent here";
        let result = strip_frontmatter(input, "test:item").unwrap();
        assert_eq!(result, "Content here");
    }

    #[test]
    fn strip_frontmatter_plain() {
        let input = "No frontmatter here";
        let result = strip_frontmatter(input, "test:item").unwrap();
        assert_eq!(result, "No frontmatter here");
    }
}
