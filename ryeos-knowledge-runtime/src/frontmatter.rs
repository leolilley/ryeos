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

/// Strip a top-of-document ```yaml fenced code block.
/// Only matches at the start of the document (after optional signature comments).
/// Does NOT touch fenced blocks in the body — those are content.
pub fn strip_fenced_yaml_block(content: &str) -> Result<String, KnowledgeError> {
    let trimmed = skip_signature_comment(content);
    if !trimmed.starts_with("```yaml") {
        return Ok(content.to_string());
    }

    // Find closing ``` on its own line
    let after_fence = &trimmed[7..]; // skip "```yaml\n"
    let rest = after_fence.trim_start_matches(['\r', '\n']);

    for line in rest.lines() {
        if line.trim() == "```" {
            let body_start = rest.find(line).unwrap() + line.len();
            let body = rest[body_start..].trim_start_matches(['\r', '\n']);
            return Ok(body.to_string());
        }
    }

    Err(KnowledgeError::FrontmatterParse {
        item_id: "<unknown>".to_string(),
        reason: "unclosed ```yaml fenced block at document start".to_string(),
    })
}

/// Skip leading HTML signature comment (<!-- ryeos:signed:... -->).
fn skip_signature_comment(content: &str) -> &str {
    let trimmed = content.trim_start();
    if trimmed.starts_with("<!--") {
        if let Some(end) = trimmed.find("-->") {
            return trimmed[end + 3..].trim_start();
        }
    }
    trimmed
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
/// Tries formats in the same order as the parser:
///   1. `---` frontmatter (canonical)
///   2. ` ```yaml` fenced block at doc start (backward compat)
///   3. Signed YAML `# ryeos:signed:...`
pub fn strip_frontmatter(content: &str, item_id: &str) -> Result<String, KnowledgeError> {
    let trimmed = content.trim_start();

    // Markdown --- frontmatter
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

    // Fenced ```yaml block at doc start (after optional signature)
    let after_sig = skip_signature_comment(content);
    if after_sig.trim_start().starts_with("```yaml") {
        return strip_fenced_yaml_block(content)
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

    #[test]
    fn strip_fenced_yaml_block_basic() {
        let input = "```yaml\ntitle: Foo\n```\nBody text";
        let result = strip_fenced_yaml_block(input).unwrap();
        assert_eq!(result, "Body text");
    }

    #[test]
    fn strip_fenced_yaml_block_no_block() {
        let input = "Just body text";
        let result = strip_fenced_yaml_block(input).unwrap();
        assert_eq!(result, "Just body text");
    }

    #[test]
    fn strip_fenced_yaml_block_unclosed() {
        let input = "```yaml\ntitle: Foo\nNo closing fence";
        let result = strip_fenced_yaml_block(input);
        assert!(result.is_err());
    }

    #[test]
    fn strip_fenced_yaml_block_ignores_body_fences() {
        let input = "Body with ```yaml\nnot metadata\n```\nstill body";
        let result = strip_fenced_yaml_block(input).unwrap();
        assert_eq!(result, input); // unchanged — not at doc start
    }

    #[test]
    fn strip_frontmatter_fenced_yaml() {
        let input = "```yaml\ntitle: Test\n```\nContent here";
        let result = strip_frontmatter(input, "test:item").unwrap();
        assert_eq!(result, "Content here");
    }
}
