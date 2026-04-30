use ryeos_handler_protocol::ParseErrKind;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Debug)]
pub struct ParseError {
    pub kind: ParseErrKind,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct YamlHeaderDocumentConfig {
    #[serde(default)]
    require_header: bool,
    #[serde(default)]
    body_field: Option<String>,
    forms: Vec<HeaderForm>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum HeaderForm {
    Frontmatter { delimiter: String },
    FencedBlock { language: String },
}

struct ExtractedHeader {
    header: String,
    body: String,
}

pub fn validate_config(config: &Value) -> Result<(), String> {
    let cfg: YamlHeaderDocumentConfig =
        serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
    if cfg.forms.is_empty() {
        return Err("yaml_header_document: forms list is empty".into());
    }
    Ok(())
}

pub fn parse(config: &Value, content: &str) -> Result<Value, ParseError> {
    let cfg: YamlHeaderDocumentConfig = serde_json::from_value(config.clone()).map_err(|e| {
        ParseError {
            kind: ParseErrKind::Internal,
            message: format!("yaml_header_document config: {e}"),
        }
    })?;

    let mut matches: Vec<(usize, ExtractedHeader)> = Vec::new();
    for (i, form) in cfg.forms.iter().enumerate() {
        if let Some(extracted) = try_form(form, content)? {
            matches.push((i, extracted));
        }
    }

    if matches.len() > 1 {
        return Err(ParseError {
            kind: ParseErrKind::Internal,
            message: format!(
                "yaml_header_document: {} forms matched simultaneously; \
                 disambiguate by removing the unintended marker",
                matches.len()
            ),
        });
    }

    let (header_yaml, body) = match matches.pop() {
        Some((_, e)) => (Some(e.header), e.body),
        None => {
            if cfg.require_header {
                return Err(ParseError {
                    kind: ParseErrKind::Schema,
                    message: "yaml_header_document: no header form matched and require_header=true"
                        .into(),
                });
            }
            (None, content.to_string())
        }
    };

    let mut map = match header_yaml {
        Some(text) if !text.trim().is_empty() => {
            let yaml: serde_yaml::Value = serde_yaml::from_str(&text).map_err(|e| {
                ParseError {
                    kind: ParseErrKind::Syntax,
                    message: format!("yaml_header_document yaml: {e}"),
                }
            })?;
            let v: Value = serde_json::to_value(yaml).map_err(|e| ParseError {
                kind: ParseErrKind::Internal,
                message: format!("yaml_header_document yaml→json: {e}"),
            })?;
            match v {
                Value::Object(m) => m,
                Value::Null => Map::new(),
                _ => {
                    return Err(ParseError {
                        kind: ParseErrKind::Schema,
                        message: "yaml_header_document: header must be a YAML mapping".into(),
                    });
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

fn try_form(form: &HeaderForm, content: &str) -> Result<Option<ExtractedHeader>, ParseError> {
    match form {
        HeaderForm::Frontmatter { delimiter } => extract_frontmatter(content, delimiter),
        HeaderForm::FencedBlock { language } => extract_fenced_block(content, language),
    }
}

fn extract_frontmatter(content: &str, delimiter: &str) -> Result<Option<ExtractedHeader>, ParseError> {
    let trimmed = content.trim_start_matches(['\u{feff}']);
    if !trimmed.starts_with(delimiter) {
        return Ok(None);
    }
    let after = &trimmed[delimiter.len()..];
    let after = match after.strip_prefix('\n').or_else(|| after.strip_prefix("\r\n")) {
        Some(s) => s,
        None => return Ok(None),
    };

    let needle_a = format!("\n{delimiter}\n");
    let needle_b = format!("\n{delimiter}\r\n");
    let needle_c = format!("\n{delimiter}");
    let close = after
        .find(&needle_a)
        .or_else(|| after.find(&needle_b))
        .or_else(|| {
            if after.ends_with(&format!("\n{delimiter}")) {
                Some(after.len() - needle_c.len())
            } else {
                None
            }
        });

    let close_idx = close.ok_or_else(|| ParseError {
        kind: ParseErrKind::Syntax,
        message: format!(
            "yaml_header_document: frontmatter opened with `{delimiter}` but never closed"
        ),
    })?;

    let header = after[..close_idx].to_string();
    let after_close = &after[close_idx..];
    let after_close = after_close
        .strip_prefix('\n')
        .unwrap_or(after_close)
        .strip_prefix(delimiter)
        .unwrap_or(after_close);
    let body = after_close.trim_start_matches(['\r', '\n']).to_string();

    Ok(Some(ExtractedHeader { header, body }))
}

fn extract_fenced_block(content: &str, language: &str) -> Result<Option<ExtractedHeader>, ParseError> {
    let opener = format!("```{language}");

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
        return Err(ParseError {
            kind: ParseErrKind::Syntax,
            message: format!(
                "yaml_header_document: fenced ```{language} block opened but never closed"
            ),
        });
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
        let out = parse(
            &cfg(),
            "---\nname: child\nversion: 1\n---\nbody-text\n",
        )
        .unwrap();
        assert_eq!(out["name"], "child");
        assert_eq!(out["body"], "body-text\n");
    }

    #[test]
    fn fenced_form() {
        let out = parse(
            &cfg(),
            "```yaml\nname: child\n```\nthe body\n",
        )
        .unwrap();
        assert_eq!(out["name"], "child");
        assert_eq!(out["body"], "the body");
    }

    #[test]
    fn require_header_no_match_errors() {
        let err = parse(&cfg(), "no header at all\n").unwrap_err();
        assert!(err.message.contains("no header form matched"));
    }

    #[test]
    fn body_field_null_does_not_emit_body() {
        let cfg_no_body = json!({
            "require_header": true,
            "forms": [
                { "kind": "frontmatter", "delimiter": "---" }
            ]
        });
        let out = parse(&cfg_no_body, "---\nname: x\n---\nbody-text\n").unwrap();
        assert_eq!(out["name"], "x");
        assert!(
            out.as_object().unwrap().get("body").is_none(),
            "body must not be emitted when body_field is absent: {out}"
        );
    }

    #[test]
    fn validate_config_rejects_unknown_form_kind() {
        let err = validate_config(&json!({
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

    #[test]
    fn frontmatter_form_a_with_yaml_codeblock_in_body_parses_form_a() {
        let content = "---\nname: child\n---\nbody\n```yaml\nx: 1\n```\n";
        let out = parse(&cfg(), content).unwrap();
        assert_eq!(out["name"], "child");
        let body = out["body"].as_str().unwrap();
        assert!(body.contains("```yaml"), "body lost code fence: {body:?}");
        assert!(body.contains("x: 1"));
    }

    #[test]
    fn body_only_yaml_fence_does_not_satisfy_require_header() {
        let content = "preamble text\n\nsome more\n```yaml\nname: x\n```\n";
        let err = parse(&cfg(), content).unwrap_err();
        assert!(err.message.contains("no header form matched"));
    }

    #[test]
    fn prologue_yaml_fence_still_parses_form_b() {
        let content = "```yaml\nname: child\n```\nthe body\n";
        let out = parse(&cfg(), content).unwrap();
        assert_eq!(out["name"], "child");
        assert_eq!(out["body"], "the body");
    }

    #[test]
    fn validate_config_rejects_unknown_top_field() {
        let err = validate_config(&json!({
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

    #[test]
    fn validate_config_rejects_empty_forms() {
        let err = validate_config(&json!({
            "forms": []
        }))
        .unwrap_err();
        assert!(err.contains("forms list is empty"));
    }
}
