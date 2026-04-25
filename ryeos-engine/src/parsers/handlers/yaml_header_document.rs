//! `parser_yaml_header_document` — extract a YAML header + body from
//! one of several configured forms (frontmatter, fenced block, …).
//!
//! Config:
//! ```yaml
//! require_header: true
//! body_field: body          # optional; when present the parsed Value
//!                           # gains this string field with the body text
//! forms:
//!   - kind: frontmatter
//!     delimiter: "---"
//!   - kind: fenced_block
//!     language: yaml
//! ```

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::EngineError;

use super::{NativeParserHandler, ParseInput};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct YamlHeaderDocumentConfig {
    #[serde(default)]
    pub require_header: bool,
    #[serde(default)]
    pub body_field: Option<String>,
    pub forms: Vec<HeaderForm>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HeaderForm {
    Frontmatter { delimiter: String },
    FencedBlock { language: String },
}

pub struct YamlHeaderDocumentHandler;

impl NativeParserHandler for YamlHeaderDocumentHandler {
    fn validate_config(&self, config: &Value) -> Result<(), String> {
        let cfg: YamlHeaderDocumentConfig =
            serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        if cfg.forms.is_empty() {
            return Err("yaml_header_document: forms list is empty".into());
        }
        Ok(())
    }

    fn parse(&self, config: &Value, input: ParseInput<'_>) -> Result<Value, EngineError> {
        let cfg: YamlHeaderDocumentConfig = serde_json::from_value(config.clone())
            .map_err(|e| EngineError::Internal(format!("yaml_header_document config: {e}")))?;

        // Try each form independently; collect every match. If more
        // than one matches we have an ambiguity → hard fail. This way a
        // file accidentally containing both `---` and a fenced block is
        // never silently chosen for one over the other.
        let mut matches: Vec<(usize, ExtractedHeader)> = Vec::new();
        for (i, form) in cfg.forms.iter().enumerate() {
            if let Some(extracted) = try_form(form, input.content)? {
                matches.push((i, extracted));
            }
        }

        if matches.len() > 1 {
            return Err(EngineError::Internal(format!(
                "yaml_header_document: {} forms matched simultaneously; \
                 disambiguate by removing the unintended marker",
                matches.len()
            )));
        }

        let (header_yaml, body) = match matches.pop() {
            Some((_, e)) => (Some(e.header), e.body),
            None => {
                if cfg.require_header {
                    return Err(EngineError::Internal(
                        "yaml_header_document: no header form matched and \
                         require_header=true"
                            .into(),
                    ));
                }
                (None, input.content.to_string())
            }
        };

        let mut map = match header_yaml {
            Some(text) if !text.trim().is_empty() => {
                let yaml: serde_yaml::Value = serde_yaml::from_str(&text).map_err(|e| {
                    EngineError::Internal(format!("yaml_header_document yaml: {e}"))
                })?;
                let v: Value = serde_json::to_value(yaml).map_err(|e| {
                    EngineError::Internal(format!("yaml_header_document yaml→json: {e}"))
                })?;
                match v {
                    Value::Object(m) => m,
                    Value::Null => Map::new(),
                    _ => {
                        return Err(EngineError::Internal(
                            "yaml_header_document: header must be a YAML mapping".into(),
                        ));
                    }
                }
            }
            _ => Map::new(),
        };

        if let Some(field) = cfg.body_field {
            map.insert(field, Value::String(body));
        }

        Ok(Value::Object(map))
    }
}

struct ExtractedHeader {
    header: String,
    body: String,
}

fn try_form(form: &HeaderForm, content: &str) -> Result<Option<ExtractedHeader>, EngineError> {
    match form {
        HeaderForm::Frontmatter { delimiter } => Ok(extract_frontmatter(content, delimiter)?),
        HeaderForm::FencedBlock { language } => Ok(extract_fenced_block(content, language)?),
    }
}

fn extract_frontmatter(
    content: &str,
    delimiter: &str,
) -> Result<Option<ExtractedHeader>, EngineError> {
    let trimmed = content.trim_start_matches(['\u{feff}']);
    if !trimmed.starts_with(delimiter) {
        return Ok(None);
    }
    let after = &trimmed[delimiter.len()..];
    // The delimiter must be on its own line.
    let after = match after.strip_prefix('\n').or_else(|| after.strip_prefix("\r\n")) {
        Some(s) => s,
        None => return Ok(None),
    };

    // Find closing delimiter at line start.
    let needle_a = format!("\n{delimiter}\n");
    let needle_b = format!("\n{delimiter}\r\n");
    let needle_c = format!("\n{delimiter}");
    let close = after
        .find(&needle_a)
        .or_else(|| after.find(&needle_b))
        .or_else(|| {
            // Allow a closing delimiter at EOF with no trailing newline.
            if after.ends_with(&format!("\n{delimiter}")) {
                Some(after.len() - needle_c.len())
            } else {
                None
            }
        });

    let close_idx = close.ok_or_else(|| {
        EngineError::Internal(format!(
            "yaml_header_document: frontmatter opened with `{delimiter}` but never closed"
        ))
    })?;

    let header = after[..close_idx].to_string();
    let after_close = &after[close_idx..];
    // Skip "\n<delim>" + optional "\r" + optional "\n"
    let after_close = after_close
        .strip_prefix('\n')
        .unwrap_or(after_close)
        .strip_prefix(delimiter)
        .unwrap_or(after_close);
    let body = after_close
        .trim_start_matches(['\r', '\n'])
        .to_string();

    Ok(Some(ExtractedHeader { header, body }))
}

fn extract_fenced_block(
    content: &str,
    language: &str,
) -> Result<Option<ExtractedHeader>, EngineError> {
    let opener = format!("```{language}");

    // Form B is anchored at the document **prologue**: the first
    // non-blank line (after BOM/whitespace) MUST be the opening fence
    // with the configured language. A fenced ```yaml block that
    // appears later in the file is body content, not a header — those
    // are silently ignored here so a Form A frontmatter doc with an
    // illustrative ```yaml example in its body never trips ambiguity
    // detection.
    let trimmed = content.trim_start_matches(['\u{feff}']);
    let mut iter = trimmed.lines();
    let first_non_blank = loop {
        match iter.next() {
            Some(l) if l.trim().is_empty() => continue,
            other => break other,
        }
    };
    let Some(first) = first_non_blank else {
        return Ok(None);
    };
    if first.trim_end() != opener {
        return Ok(None);
    }

    let mut header_lines: Vec<&str> = Vec::new();
    let mut found_close = false;
    while let Some(line) = iter.next() {
        if line.trim() == "```" {
            found_close = true;
            break;
        }
        header_lines.push(line);
    }
    if !found_close {
        return Err(EngineError::Internal(format!(
            "yaml_header_document: fenced ```{language} block opened but never closed"
        )));
    }

    let body_lines: Vec<&str> = iter.collect();

    Ok(Some(ExtractedHeader {
        header: header_lines.join("\n"),
        body: body_lines.join("\n"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> Value {
        json!({
            "require_header": true,
            "body_field": "body",
            "forms": [
                { "kind": "frontmatter", "delimiter": "---" },
                { "kind": "fenced_block", "language": "yaml" }
            ]
        })
    }

    #[test]
    fn frontmatter_form() {
        let h = YamlHeaderDocumentHandler;
        let out = h
            .parse(
                &cfg(),
                ParseInput {
                    content: "---\nname: child\nversion: 1\n---\nbody-text\n",
                    path: None,
                },
            )
            .unwrap();
        assert_eq!(out["name"], "child");
        assert_eq!(out["body"], "body-text\n");
    }

    #[test]
    fn fenced_form() {
        let h = YamlHeaderDocumentHandler;
        let out = h
            .parse(
                &cfg(),
                ParseInput {
                    content: "```yaml\nname: child\n```\nthe body\n",
                    path: None,
                },
            )
            .unwrap();
        assert_eq!(out["name"], "child");
        assert_eq!(out["body"], "the body");
    }

    // NOTE: A previous test `ambiguity_is_hard_error` asserted that a
    // doc with both `---` frontmatter AND a body ```yaml fence raised
    // an ambiguity error. That was a false positive caused by the
    // greedy fenced-block extractor matching anywhere in the file.
    // With the prologue anchor (oracle P1 #5), only a fence at the
    // first non-blank line counts as Form B, so the two forms cannot
    // both match the same document and the ambiguity branch is
    // exercised only by future form kinds. The replacement coverage
    // lives in `frontmatter_form_a_with_yaml_codeblock_in_body_parses_form_a`.

    #[test]
    fn require_header_no_match_errors() {
        let h = YamlHeaderDocumentHandler;
        let err = h
            .parse(
                &cfg(),
                ParseInput {
                    content: "no header at all\n",
                    path: None,
                },
            )
            .unwrap_err();
        assert!(format!("{err}").contains("no header form matched"));
    }

    #[test]
    fn body_field_null_does_not_emit_body() {
        // body_field omitted → no `body` key on the parsed value.
        let cfg_no_body = json!({
            "require_header": true,
            "forms": [
                { "kind": "frontmatter", "delimiter": "---" }
            ]
        });
        let h = YamlHeaderDocumentHandler;
        let out = h
            .parse(
                &cfg_no_body,
                ParseInput {
                    content: "---\nname: x\n---\nbody-text\n",
                    path: None,
                },
            )
            .unwrap();
        assert_eq!(out["name"], "x");
        assert!(
            out.as_object().unwrap().get("body").is_none(),
            "body must not be emitted when body_field is absent: {out}"
        );
    }

    #[test]
    fn validate_config_rejects_unknown_form_kind() {
        let h = YamlHeaderDocumentHandler;
        let err = h
            .validate_config(&json!({
                "require_header": true,
                "forms": [
                    { "kind": "totally_made_up", "delimiter": "---" }
                ]
            }))
            .unwrap_err();
        assert!(
            err.contains("unknown variant")
                || err.contains("totally_made_up")
                || err.contains("kind"),
            "expected rejection of unknown form kind, got: {err}"
        );
    }

    /// A Form A (frontmatter) doc that happens to contain a ```yaml
    /// code block in its body must parse cleanly as Form A. The Form
    /// B fenced-block extractor is anchored to the document prologue,
    /// so a later ```yaml block is body content, not a competing
    /// header — no ambiguity error.
    #[test]
    fn frontmatter_form_a_with_yaml_codeblock_in_body_parses_form_a() {
        let h = YamlHeaderDocumentHandler;
        let content = "---\nname: child\n---\nbody\n```yaml\nx: 1\n```\n";
        let out = h
            .parse(&cfg(), ParseInput { content, path: None })
            .unwrap();
        assert_eq!(out["name"], "child");
        // body must include the literal code fence text — proof that
        // the fenced extractor did NOT swallow it as a header form.
        let body = out["body"].as_str().unwrap();
        assert!(body.contains("```yaml"), "body lost code fence: {body:?}");
        assert!(body.contains("x: 1"));
    }

    /// `require_header: true` + a body-only ```yaml fence (no prologue
    /// match for any form) → must error. Catches the regression where
    /// the previous greedy fenced extractor would have matched the
    /// body fence and "succeeded".
    #[test]
    fn body_only_yaml_fence_does_not_satisfy_require_header() {
        let h = YamlHeaderDocumentHandler;
        let content = "preamble text\n\nsome more\n```yaml\nname: x\n```\n";
        let err = h
            .parse(&cfg(), ParseInput { content, path: None })
            .unwrap_err();
        assert!(format!("{err}").contains("no header form matched"));
    }

    /// Sanity: an explicit Form B with the fence at the prologue is
    /// still extracted (ensures the prologue anchor didn't break Form
    /// B entirely).
    #[test]
    fn prologue_yaml_fence_still_parses_form_b() {
        let h = YamlHeaderDocumentHandler;
        let content = "```yaml\nname: child\n```\nthe body\n";
        let out = h
            .parse(&cfg(), ParseInput { content, path: None })
            .unwrap();
        assert_eq!(out["name"], "child");
        assert_eq!(out["body"], "the body");
    }

    #[test]
    fn validate_config_rejects_unknown_top_field() {
        let h = YamlHeaderDocumentHandler;
        let err = h
            .validate_config(&json!({
                "require_header": true,
                "forms": [{ "kind": "frontmatter", "delimiter": "---" }],
                "bogus": true
            }))
            .unwrap_err();
        assert!(
            err.contains("unknown field") || err.contains("bogus"),
            "expected unknown-field rejection, got: {err}"
        );
    }
}
