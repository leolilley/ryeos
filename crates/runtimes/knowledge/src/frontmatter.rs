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

/// Skip a leading HTML signature comment (`<!-- ryeos:signed:… -->`) and/or
/// a `# ryeos:signed:`/`# ryeos: cas:` line, returning the trimmed
/// remainder. Both envelope styles are handled so the SAME helper serves
/// markdown items (HTML envelope) and signed-YAML items (`#` envelope) —
/// the bug this fixes is that strip and parse used to disagree on whether
/// to skip the HTML signature before looking for `---` frontmatter.
fn after_leading_signatures(content: &str) -> &str {
    let mut s = content.trim_start();
    if s.starts_with("<!--") {
        if let Some(end) = s.find("-->") {
            s = s[end + 3..].trim_start();
        }
    }
    if s.starts_with("# ryeos:signed:") || s.starts_with("# ryeos: cas:") {
        s = match s.find('\n') {
            Some(nl) => s[nl + 1..].trim_start(),
            None => "",
        };
    }
    s
}

/// Locate the raw YAML frontmatter block text (between delimiters), if any,
/// after stripping leading signatures. Returns `None` when no `---` or
/// ` ```yaml` block is present. Used by [`parse_frontmatter_result`].
fn frontmatter_block(content: &str) -> Option<&str> {
    let trimmed = after_leading_signatures(content);
    if trimmed.starts_with("---") {
        let rest = trimmed[3..].trim_start_matches(['\r', '\n']);
        rest.find("\n---")
            .map(|idx| &rest[..idx])
            .or_else(|| rest.strip_suffix("---").map(str::trim_end))
    } else if trimmed.starts_with("```yaml") {
        let rest = trimmed[7..].trim_start_matches(['\r', '\n']);
        rest.find("\n```").map(|idx| &rest[..idx])
    } else {
        None
    }
}

/// Strict frontmatter parse for integrity validation:
///   - `Ok(None)`        — no frontmatter block present (not an error),
///   - `Ok(Some(obj))`   — a block parsed to a mapping (or was empty),
///   - `Err(reason)`     — a block IS present but is not valid YAML.
///
/// A valid-but-non-mapping block (e.g. a YAML list) is treated as an empty
/// mapping rather than an error — only genuine syntax errors are integrity
/// failures.
pub fn parse_frontmatter_result(content: &str) -> Result<Option<serde_json::Value>, String> {
    let Some(block) = frontmatter_block(content) else {
        return Ok(None);
    };
    match serde_yaml::from_str::<serde_json::Value>(block) {
        Ok(v) if v.is_object() => Ok(Some(v)),
        Ok(_) => Ok(Some(serde_json::json!({}))),
        Err(e) => Err(e.to_string()),
    }
}

/// Whether a knowledge item's source is a whole-document YAML file
/// (`.yaml`/`.yml`) rather than a markdown file with a frontmatter block.
/// The kind schema advertises both formats, so the read ops must validate
/// both — not just markdown.
fn is_yaml_source(source_path: &str) -> bool {
    let p = source_path.to_ascii_lowercase();
    p.ends_with(".yaml") || p.ends_with(".yml")
}

/// Format-aware strict metadata parse, keyed off the item's source path:
///   - `.md`           → the markdown frontmatter block (see
///     [`parse_frontmatter_result`]);
///   - `.yaml`/`.yml`  → the WHOLE (signature-stripped) document is the
///     metadata and must be syntactically valid YAML.
///
/// Returns `Err` only on genuine YAML syntax errors (an integrity failure
/// the corpus `validate` must surface), `Ok(None)` when there is no
/// metadata, and `Ok(Some(obj))` for a parsed mapping (or empty for a
/// valid-but-non-mapping document).
pub fn parse_metadata_result(
    raw_content: &str,
    source_path: &str,
) -> Result<Option<serde_json::Value>, String> {
    if is_yaml_source(source_path) {
        match serde_yaml::from_str::<serde_json::Value>(raw_content) {
            Ok(v) if v.is_object() => Ok(Some(v)),
            Ok(serde_json::Value::Null) => Ok(None),
            Ok(_) => Ok(Some(serde_json::json!({}))),
            Err(e) => Err(e.to_string()),
        }
    } else {
        parse_frontmatter_result(raw_content)
    }
}

/// Tolerant format-aware metadata parse (empty object on any failure) —
/// for query filters/display where a malformed item is simply unmatchable,
/// not a hard error.
pub fn parse_metadata(raw_content: &str, source_path: &str) -> serde_json::Value {
    match parse_metadata_result(raw_content, source_path) {
        Ok(Some(v)) => v,
        _ => serde_json::json!({}),
    }
}

/// Strip frontmatter from content, returning the body.
///
/// Leading signatures (HTML comment and/or `# ryeos:signed:` line) are
/// stripped FIRST, then the frontmatter style is detected:
///   1. `---` frontmatter (canonical markdown)
///   2. ` ```yaml` fenced block
///   3. neither → the post-signature content is the body verbatim (a
///      signed-YAML item whose whole document is its content)
pub fn strip_frontmatter(content: &str, item_id: &str) -> Result<String, KnowledgeError> {
    let after = after_leading_signatures(content);

    let relabel = |e: KnowledgeError| KnowledgeError::FrontmatterParse {
        item_id: item_id.to_string(),
        reason: match e {
            KnowledgeError::FrontmatterParse { reason, .. } => reason,
            other => other.to_string(),
        },
    };

    if after.starts_with("---") {
        return strip_markdown_frontmatter(after).map_err(relabel);
    }
    if after.starts_with("```yaml") {
        return strip_fenced_yaml_block(after).map_err(relabel);
    }
    Ok(after.to_string())
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
    fn strip_frontmatter_signed_yaml_body() {
        // A signed-YAML item (`# ryeos:signed:` line, no `---`/```yaml
        // block) returns the post-signature body verbatim.
        let input = "# ryeos:signed:v1:abc:def:123\nactual body\nmore body";
        let result = strip_frontmatter(input, "test:item").unwrap();
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
