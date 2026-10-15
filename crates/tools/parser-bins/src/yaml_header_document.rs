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
    Frontmatter {
        delimiter: String,
    },
    FencedBlock {
        language: String,
    },
    CommentMarker {
        marker: String,
        comment_prefix: String,
        #[serde(default)]
        allow_after_shebang: bool,
    },
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
    for form in &cfg.forms {
        match form {
            HeaderForm::Frontmatter { delimiter } if delimiter.is_empty() => {
                return Err("yaml_header_document: frontmatter delimiter must not be empty".into());
            }
            HeaderForm::FencedBlock { language } if language.trim().is_empty() => {
                return Err("yaml_header_document: fenced_block language must not be empty".into());
            }
            HeaderForm::CommentMarker {
                marker,
                comment_prefix,
                ..
            } => {
                if marker.trim().is_empty() {
                    return Err(
                        "yaml_header_document: comment_marker marker must not be empty".into(),
                    );
                }
                if comment_prefix.is_empty() {
                    return Err(
                        "yaml_header_document: comment_marker comment_prefix must not be empty"
                            .into(),
                    );
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn parse(config: &Value, content: &str) -> Result<Value, ParseError> {
    let cfg: YamlHeaderDocumentConfig =
        serde_json::from_value(config.clone()).map_err(|e| ParseError {
            kind: ParseErrKind::Internal,
            message: format!("yaml_header_document config: {e}"),
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
            let yaml: serde_yaml::Value = serde_yaml::from_str(&text).map_err(|e| ParseError {
                kind: ParseErrKind::Syntax,
                message: format!("yaml_header_document yaml: {e}"),
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
        HeaderForm::CommentMarker {
            marker,
            comment_prefix,
            allow_after_shebang,
        } => extract_comment_marker(content, marker, comment_prefix, *allow_after_shebang),
    }
}

fn extract_frontmatter(
    content: &str,
    delimiter: &str,
) -> Result<Option<ExtractedHeader>, ParseError> {
    let trimmed = content.trim_start_matches(['\u{feff}']);
    if !trimmed.starts_with(delimiter) {
        return Ok(None);
    }
    let after = &trimmed[delimiter.len()..];
    let after = match after
        .strip_prefix('\n')
        .or_else(|| after.strip_prefix("\r\n"))
    {
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

fn extract_fenced_block(
    content: &str,
    language: &str,
) -> Result<Option<ExtractedHeader>, ParseError> {
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
    for line in iter.by_ref() {
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

fn extract_comment_marker(
    content: &str,
    marker: &str,
    comment_prefix: &str,
    allow_after_shebang: bool,
) -> Result<Option<ExtractedHeader>, ParseError> {
    if marker.trim().is_empty() {
        return Err(ParseError {
            kind: ParseErrKind::Internal,
            message: "yaml_header_document: comment_marker marker must not be empty".into(),
        });
    }
    if comment_prefix.is_empty() {
        return Err(ParseError {
            kind: ParseErrKind::Internal,
            message: "yaml_header_document: comment_marker comment_prefix must not be empty".into(),
        });
    }

    let trimmed = content.trim_start_matches(['\u{feff}']);
    let mut lines: Vec<&str> = trimmed.lines().collect();
    if trimmed.ends_with('\n') {
        lines.push("");
    }

    let mut start = 0;
    if allow_after_shebang && lines.first().is_some_and(|line| line.starts_with("#!")) {
        start = 1;
    }
    while start < lines.len() && lines[start].trim().is_empty() {
        start += 1;
    }

    let marker_line = format!("{marker}:");
    let Some(first_payload) = strip_comment_payload(
        lines.get(start).copied().unwrap_or_default(),
        comment_prefix,
    ) else {
        return Ok(None);
    };
    if first_payload.trim() != marker_line {
        return Ok(None);
    }

    let mut header_lines = Vec::new();
    let mut idx = start;
    while idx < lines.len() {
        let Some(payload) = strip_comment_payload(lines[idx], comment_prefix) else {
            break;
        };
        header_lines.push(payload.to_owned());
        idx += 1;
    }

    let marker_count = header_lines
        .iter()
        .filter(|line| line.trim() == marker_line)
        .count();
    if marker_count > 1 {
        return Err(ParseError {
            kind: ParseErrKind::Schema,
            message: format!(
                "yaml_header_document: comment_marker `{marker}` appears more than once in header"
            ),
        });
    }

    let body = lines[idx..].join("\n");
    Ok(Some(ExtractedHeader {
        header: unwrap_comment_marker_header(&header_lines.join("\n"), marker)?,
        body,
    }))
}

fn strip_comment_payload<'a>(line: &'a str, comment_prefix: &str) -> Option<&'a str> {
    let payload = line.strip_prefix(comment_prefix)?;
    Some(payload.strip_prefix(' ').unwrap_or(payload))
}

fn unwrap_comment_marker_header(header: &str, marker: &str) -> Result<String, ParseError> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(header).map_err(|e| ParseError {
        kind: ParseErrKind::Syntax,
        message: format!("yaml_header_document comment_marker yaml: {e}"),
    })?;
    let v: Value = serde_json::to_value(yaml).map_err(|e| ParseError {
        kind: ParseErrKind::Internal,
        message: format!("yaml_header_document comment_marker yaml→json: {e}"),
    })?;
    let Value::Object(mut root) = v else {
        return Err(ParseError {
            kind: ParseErrKind::Schema,
            message: "yaml_header_document: comment_marker header must be a YAML mapping".into(),
        });
    };
    let inner = root.remove(marker).ok_or_else(|| ParseError {
        kind: ParseErrKind::Schema,
        message: format!("yaml_header_document: comment_marker missing `{marker}` mapping"),
    })?;
    if !root.is_empty() {
        return Err(ParseError {
            kind: ParseErrKind::Schema,
            message: format!(
                "yaml_header_document: comment_marker header must contain only `{marker}` at root"
            ),
        });
    }
    match inner {
        Value::Object(map) => serde_yaml::to_string(&map).map_err(|e| ParseError {
            kind: ParseErrKind::Internal,
            message: format!("yaml_header_document comment_marker json→yaml: {e}"),
        }),
        _ => Err(ParseError {
            kind: ParseErrKind::Schema,
            message: format!("yaml_header_document: `{marker}` value must be a YAML mapping"),
        }),
    }
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
        let out = parse(&cfg(), "---\nname: child\nversion: 1\n---\nbody-text\n").unwrap();
        assert_eq!(out["name"], "child");
        assert_eq!(out["body"], "body-text\n");
    }

    #[test]
    fn fenced_form() {
        let out = parse(&cfg(), "```yaml\nname: child\n```\nthe body\n").unwrap();
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

    fn comment_cfg() -> Value {
        json!({
            "require_header": true,
            "body_field": "body",
            "forms": [{
                "kind": "comment_marker",
                "marker": "ryeos-tool",
                "comment_prefix": "#",
                "allow_after_shebang": true
            }]
        })
    }

    #[test]
    fn comment_marker_form_unwraps_marker_mapping_after_shebang() {
        let content = "#!/usr/bin/env python3\n# ryeos-tool:\n#   category: agent-kiwi/oauth\n#   version: \"1.0.0\"\n#   required_secrets:\n#     - GOOGLE_CLIENT_ID\n\ndef execute(params, project_path):\n    return {}\n";
        let out = parse(&comment_cfg(), content).unwrap();
        assert_eq!(out["category"], "agent-kiwi/oauth");
        assert_eq!(out["version"], "1.0.0");
        assert_eq!(out["required_secrets"][0], "GOOGLE_CLIENT_ID");
        assert!(out["body"].as_str().unwrap().contains("def execute"));
    }

    #[test]
    fn comment_marker_form_rejects_missing_header_when_required() {
        let err = parse(
            &comment_cfg(),
            "#!/usr/bin/env python3\ndef execute(params, project_path):\n    return {}\n",
        )
        .unwrap_err();
        assert!(err.message.contains("no header form matched"));
    }

    #[test]
    fn comment_marker_form_rejects_unindented_root_keys() {
        let err = parse(
            &comment_cfg(),
            "# ryeos-tool:\n# category: wrong\nprint('body')\n",
        )
        .unwrap_err();
        assert!(
            err.message.contains("must contain only `ryeos-tool`"),
            "expected extra-root-key rejection, got: {}",
            err.message
        );
    }

    #[test]
    fn comment_marker_form_rejects_duplicate_marker_in_header() {
        let err = parse(
            &comment_cfg(),
            "# ryeos-tool:\n#   category: one\n# ryeos-tool:\n#   category: two\nprint('body')\n",
        )
        .unwrap_err();
        assert!(
            err.message.contains("appears more than once"),
            "expected duplicate marker rejection, got: {}",
            err.message
        );
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

    #[test]
    fn validate_config_rejects_empty_comment_marker_fields() {
        let err = validate_config(&json!({
            "forms": [{
                "kind": "comment_marker",
                "marker": " ",
                "comment_prefix": "#"
            }]
        }))
        .unwrap_err();
        assert!(err.contains("marker must not be empty"));

        let err = validate_config(&json!({
            "forms": [{
                "kind": "comment_marker",
                "marker": "ryeos-tool",
                "comment_prefix": ""
            }]
        }))
        .unwrap_err();
        assert!(err.contains("comment_prefix must not be empty"));
    }
}
