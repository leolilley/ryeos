use ryeos_handler_protocol::{ParseErrKind, SourceScalarEdit};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug)]
pub struct ParseError {
    pub kind: ParseErrKind,
    pub message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct YamlDocumentConfig {
    #[serde(default)]
    require_mapping: bool,
}

pub fn validate_config(config: &Value) -> Result<(), String> {
    let _: YamlDocumentConfig =
        serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn parse(config: &Value, content: &str) -> Result<Value, ParseError> {
    let cfg: YamlDocumentConfig =
        serde_json::from_value(config.clone()).map_err(|e| ParseError {
            kind: ParseErrKind::Internal,
            message: format!("yaml_document config: {e}"),
        })?;

    if content.trim().is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }

    let yaml: serde_yaml::Value = serde_yaml::from_str(content).map_err(|e| ParseError {
        kind: ParseErrKind::Syntax,
        message: format!("yaml_document parse: {e}"),
    })?;

    if cfg.require_mapping && !matches!(yaml, serde_yaml::Value::Mapping(_)) {
        return Err(ParseError {
            kind: ParseErrKind::Schema,
            message: "yaml_document: require_mapping=true rejects non-mapping root".into(),
        });
    }

    serde_json::to_value(yaml).map_err(|e| ParseError {
        kind: ParseErrKind::Internal,
        message: format!("yaml→json: {e}"),
    })
}

/// Apply conditional scalar edits to a block-style YAML document. Syntax
/// inspection and source preservation belong to this parser handler; callers
/// name only semantic JSON pointers and expected/replacement values.
pub fn edit_source(
    config: &Value,
    content: &str,
    edits: &[SourceScalarEdit],
) -> Result<(String, Value), ParseError> {
    let before = parse(config, content)?;
    let mut expected_after = before.clone();
    for edit in edits {
        match (&edit.expected, before.pointer(&edit.pointer)) {
            (Some(expected), Some(current)) if expected == current => {}
            (None, None) => {}
            _ => {
                return Err(ParseError {
                    kind: ParseErrKind::Schema,
                    message: format!("yaml source edit precondition failed at {}", edit.pointer),
                });
            }
        }
        set_pointer_value(&mut expected_after, &edit.pointer, edit.value.clone())?;
    }
    let edited = edit_block_sequence_scalars(content, edits)?;
    let after = parse(config, &edited)?;
    if after != expected_after {
        return Err(ParseError {
            kind: ParseErrKind::Internal,
            message: "yaml source editor changed semantic values outside the requested pointers"
                .into(),
        });
    }
    Ok((edited, after))
}

fn set_pointer_value(root: &mut Value, pointer: &str, value: Value) -> Result<(), ParseError> {
    if let Some(current) = root.pointer_mut(pointer) {
        *current = value;
        return Ok(());
    }
    let (parent_pointer, key) = pointer.rsplit_once('/').ok_or_else(|| ParseError {
        kind: ParseErrKind::Schema,
        message: "yaml source edit pointer is not canonical".into(),
    })?;
    let parent = root
        .pointer_mut(parent_pointer)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ParseError {
            kind: ParseErrKind::Schema,
            message: format!("yaml source edit parent is absent: {parent_pointer}"),
        })?;
    if parent.insert(key.to_owned(), value).is_some() {
        return source_edit_refusal("yaml source edit insertion contradicted an existing value");
    }
    Ok(())
}

fn edit_block_sequence_scalars(
    content: &str,
    edits: &[SourceScalarEdit],
) -> Result<String, ParseError> {
    if content.contains('\t') || content.starts_with("---\n") || content.starts_with("---\r\n") {
        return source_edit_refusal(
            "source editing requires one unmarked block-style YAML document without tabs",
        );
    }
    let mut root_key = None::<String>;
    let mut requested = std::collections::BTreeMap::<(usize, String), String>::new();
    for edit in edits {
        let segments = edit.pointer.split('/').collect::<Vec<_>>();
        if segments.len() != 4
            || !segments[0].is_empty()
            || !is_plain_mapping_key(segments[1])
            || !is_plain_mapping_key(segments[3])
        {
            return source_edit_refusal(
                "source editor accepts only top-level block-sequence scalar pointers",
            );
        }
        match &root_key {
            Some(root) if root != segments[1] => {
                return source_edit_refusal(
                    "one source edit request must target one top-level block sequence",
                );
            }
            None => root_key = Some(segments[1].to_owned()),
            _ => {}
        }
        let index = segments[2].parse::<usize>().map_err(|_| ParseError {
            kind: ParseErrKind::Schema,
            message: "source-edit sequence index is not canonical".into(),
        })?;
        let value = edit.value.as_str().ok_or_else(|| ParseError {
            kind: ParseErrKind::Schema,
            message: "block YAML source-edit replacement must be a string".into(),
        })?;
        if !is_safe_plain_scalar(value) {
            return source_edit_refusal(
                "block YAML source-edit replacement is not a safe plain scalar",
            );
        }
        if requested
            .insert((index, segments[3].to_owned()), value.to_owned())
            .is_some()
        {
            return source_edit_refusal("source-edit pointer is duplicated");
        }
    }
    let root_key = root_key.ok_or_else(|| ParseError {
        kind: ParseErrKind::Schema,
        message: "source edit request is empty".into(),
    })?;

    let mut lines = split_lines_preserving_endings(content);
    let root_marker = format!("{root_key}:");
    let sequence_start = lines
        .iter()
        .position(|line| {
            line_content(line).starts_with(&root_marker) && leading_spaces(line_content(line)) == 0
        })
        .ok_or_else(|| ParseError {
            kind: ParseErrKind::Schema,
            message: format!("item has no root-authored block `{root_key}` sequence"),
        })?;
    if lines.iter().skip(sequence_start + 1).any(|line| {
        line_content(line).starts_with(&root_marker) && leading_spaces(line_content(line)) == 0
    }) {
        return source_edit_refusal("edited top-level sequence key is duplicated");
    }
    let block_end = lines
        .iter()
        .enumerate()
        .skip(sequence_start + 1)
        .find(|(_, line)| {
            let content = line_content(line);
            !content.trim().is_empty()
                && !content.trim_start().starts_with('#')
                && leading_spaces(content) == 0
        })
        .map(|(index, _)| index)
        .unwrap_or(lines.len());
    if lines[sequence_start..block_end].iter().any(|line| {
        let source = line_content(line);
        let trimmed = source.trim_start();
        !trimmed.starts_with('#')
            && (trimmed.starts_with('&')
                || trimmed.starts_with('*')
                || trimmed.starts_with('!')
                || trimmed.starts_with("<<:")
                || trimmed.contains(": &")
                || trimmed.contains(": *")
                || trimmed.contains(": !"))
    }) {
        return source_edit_refusal(
            "block YAML source editing refuses anchors, aliases, tags, and merge keys",
        );
    }
    let sequence_indent = lines
        .iter()
        .take(block_end)
        .skip(sequence_start + 1)
        .find_map(|line| {
            let source = line_content(line);
            source
                .trim_start()
                .starts_with("- ")
                .then(|| leading_spaces(source))
        })
        .ok_or_else(|| ParseError {
            kind: ParseErrKind::Schema,
            message: format!("`{root_key}` must be a non-empty block sequence"),
        })?;
    let starts = lines
        .iter()
        .enumerate()
        .take(block_end)
        .skip(sequence_start + 1)
        .filter(|(_, line)| {
            let source = line_content(line);
            leading_spaces(source) == sequence_indent && source.trim_start().starts_with("- ")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if starts.iter().any(|index| {
        let item = line_content(&lines[*index])
            .trim_start()
            .strip_prefix("- ")
            .unwrap_or("")
            .trim_start();
        item.starts_with('{')
            || item.starts_with('[')
            || item.starts_with('&')
            || item.starts_with('*')
            || item.starts_with('!')
    }) {
        return source_edit_refusal(
            "edited YAML sequence items must use block-style mappings without anchors, aliases, or tags",
        );
    }

    let mut indices = requested
        .keys()
        .map(|(index, _)| *index)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    indices.sort_unstable_by(|left, right| right.cmp(left));
    for declaration_index in indices {
        let start = *starts.get(declaration_index).ok_or_else(|| ParseError {
            kind: ParseErrKind::Schema,
            message: "source-edit index is outside the source sequence".into(),
        })?;
        let end = starts
            .get(declaration_index + 1)
            .copied()
            .unwrap_or(block_end);
        let key_indent = sequence_indent + 2;
        let mut key_lines = std::collections::BTreeMap::<String, usize>::new();
        for (line_index, line) in lines.iter().enumerate().take(end).skip(start) {
            let source = line_content(line);
            let candidate = if line_index == start {
                source.trim_start().strip_prefix("- ").unwrap_or("")
            } else if leading_spaces(source) == key_indent {
                &source[key_indent..]
            } else {
                continue;
            };
            if let Some(key) = scalar_key(candidate)
                && key_lines.insert(key.to_owned(), line_index).is_some()
            {
                return source_edit_refusal("edited block-sequence mapping key is duplicated");
            }
        }
        let fields = requested
            .iter()
            .filter(|((index, _), _)| *index == declaration_index)
            .map(|((_, field), value)| (field.clone(), value.clone()))
            .collect::<Vec<_>>();
        for (field, value) in fields {
            match key_lines.get(&field).copied() {
                Some(index) => lines[index] = replace_scalar_value(&lines[index], &value)?,
                None => {
                    let insertion = key_lines.values().copied().max().unwrap_or(start);
                    let newline = newline_of(&lines[insertion]);
                    lines.insert(
                        insertion + 1,
                        format!("{}{field}: {value}{newline}", " ".repeat(key_indent)),
                    );
                    key_lines.insert(field, insertion + 1);
                }
            }
        }
    }
    Ok(lines.concat())
}

fn is_plain_mapping_key(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn is_safe_plain_scalar(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/'))
}

fn source_edit_refusal<T>(message: &str) -> Result<T, ParseError> {
    Err(ParseError {
        kind: ParseErrKind::Schema,
        message: message.to_owned(),
    })
}

fn split_lines_preserving_endings(content: &str) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }
    content.split_inclusive('\n').map(str::to_owned).collect()
}

fn line_content(line: &str) -> &str {
    line.strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(line)
}

fn newline_of(line: &str) -> &'static str {
    if line.ends_with("\r\n") { "\r\n" } else { "\n" }
}

fn leading_spaces(line: &str) -> usize {
    line.bytes().take_while(|byte| *byte == b' ').count()
}

fn scalar_key(candidate: &str) -> Option<&str> {
    let (key, _) = candidate.split_once(':')?;
    (!key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    .then_some(key)
}

fn replace_scalar_value(line: &str, value: &str) -> Result<String, ParseError> {
    let ending = if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let content = line_content(line);
    let colon = content.find(':').ok_or_else(|| ParseError {
        kind: ParseErrKind::Schema,
        message: "source scalar has no colon".into(),
    })?;
    let value_start = colon
        + 1
        + content[colon + 1..]
            .bytes()
            .take_while(|byte| *byte == b' ')
            .count();
    let tail = &content[value_start..];
    let comment = find_yaml_comment(tail).unwrap_or(tail.len());
    let raw = tail[..comment].trim_end();
    if raw.is_empty() || raw.starts_with('|') || raw.starts_with('>') {
        return source_edit_refusal("source scalar uses an unsupported YAML value");
    }
    let rendered = if raw.starts_with('"') && raw.ends_with('"') {
        format!("\"{value}\"")
    } else if raw.starts_with('\'') && raw.ends_with('\'') {
        format!("'{value}'")
    } else {
        value.to_owned()
    };
    let whitespace = &tail[raw.len()..comment];
    Ok(format!(
        "{}{}{}{}{}",
        &content[..value_start],
        rendered,
        whitespace,
        &tail[comment..],
        ending
    ))
}

fn find_yaml_comment(value: &str) -> Option<usize> {
    let mut single = false;
    let mut double = false;
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        match *byte {
            b'\'' if !double => single = !single,
            b'"' if !single && (index == 0 || bytes[index - 1] != b'\\') => double = !double,
            b'#' if !single
                && !double
                && (index == 0 || bytes[index - 1].is_ascii_whitespace()) =>
            {
                return Some(index);
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_yaml_mapping() {
        let out = parse(
            &json!({ "require_mapping": true }),
            "name: foo\nversion: 1\n",
        )
        .unwrap();
        assert_eq!(out["name"], "foo");
    }

    #[test]
    fn require_mapping_rejects_scalar() {
        let err = parse(&json!({ "require_mapping": true }), "42\n").unwrap_err();
        assert!(err.message.contains("require_mapping"));
    }

    #[test]
    fn require_mapping_rejects_sequence() {
        let err = parse(&json!({ "require_mapping": true }), "- a\n- b\n").unwrap_err();
        assert!(err.message.contains("require_mapping"));
    }

    #[test]
    fn validate_config_rejects_unknown_field() {
        let err = validate_config(&json!({ "require_mapping": true, "bogus": 1 })).unwrap_err();
        assert!(
            err.contains("unknown field") || err.contains("bogus"),
            "expected unknown-field rejection, got: {err}"
        );
    }

    #[test]
    fn empty_yields_empty_object() {
        let out = parse(&json!({}), "").unwrap();
        assert!(out.as_object().unwrap().is_empty());
    }

    #[test]
    fn source_editor_preserves_comments_and_crlf() {
        let source = "external_content:\r\n  - id: fixture\r\n    mode: captured # keep\r\n    mount: fixture\r\n";
        let edits = vec![
            SourceScalarEdit {
                pointer: "/external_content/0/mode".into(),
                expected: Some(Value::String("captured".into())),
                value: Value::String("pinned".into()),
            },
            SourceScalarEdit {
                pointer: "/external_content/0/digest".into(),
                expected: None,
                value: Value::String("a".repeat(64)),
            },
        ];
        let (edited, value) =
            edit_source(&json!({ "require_mapping": true }), source, &edits).unwrap();
        assert!(edited.contains("mode: pinned # keep\r\n"));
        assert!(edited.contains(&format!("digest: {}\r\n", "a".repeat(64))));
        assert_eq!(value["external_content"][0]["mode"], "pinned");
    }

    #[test]
    fn source_editor_is_generic_over_the_requested_sequence_and_fields() {
        let source = "routes:\n  - name: primary\n    state: old # retain\n";
        let edits = vec![
            SourceScalarEdit {
                pointer: "/routes/0/state".into(),
                expected: Some(Value::String("old".into())),
                value: Value::String("current".into()),
            },
            SourceScalarEdit {
                pointer: "/routes/0/digest".into(),
                expected: None,
                value: Value::String("b".repeat(64)),
            },
        ];
        let (edited, value) =
            edit_source(&json!({ "require_mapping": true }), source, &edits).unwrap();
        assert!(edited.contains("state: current # retain\n"));
        assert!(edited.contains(&format!("digest: {}\n", "b".repeat(64))));
        assert_eq!(value["routes"][0]["state"], "current");
    }

    #[test]
    fn source_editor_preserves_unedited_flow_values_inside_a_block_item() {
        let source = "routes:\n  - name: primary\n    excludes: [one, two]\n    state: old\nconfig: {enabled: true}\n";
        let edit = SourceScalarEdit {
            pointer: "/routes/0/state".into(),
            expected: Some(Value::String("old".into())),
            value: Value::String("current".into()),
        };

        let (edited, value) =
            edit_source(&json!({ "require_mapping": true }), source, &[edit]).unwrap();

        assert!(edited.contains("excludes: [one, two]\n"));
        assert!(edited.contains("config: {enabled: true}\n"));
        assert_eq!(value["routes"][0]["state"], "current");
    }

    #[test]
    fn source_editor_refuses_ambiguous_yaml_and_failed_preconditions() {
        let flow = "routes: [{name: primary, state: old}]\n";
        let edit = SourceScalarEdit {
            pointer: "/routes/0/state".into(),
            expected: Some(Value::String("old".into())),
            value: Value::String("current".into()),
        };
        assert!(edit_source(&json!({ "require_mapping": true }), flow, &[edit.clone()]).is_err());

        let duplicate = "routes:\n  - name: primary\n    state: old\n    state: older\n";
        assert!(
            edit_source(
                &json!({ "require_mapping": true }),
                duplicate,
                &[edit.clone()]
            )
            .is_err()
        );

        let ordinary = "routes:\n  - name: primary\n    state: old\n";
        let wrong = SourceScalarEdit {
            expected: Some(Value::String("different".into())),
            ..edit
        };
        assert!(edit_source(&json!({ "require_mapping": true }), ordinary, &[wrong]).is_err());
    }
}
