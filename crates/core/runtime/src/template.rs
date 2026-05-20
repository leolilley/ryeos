//! Generic template + path utilities for data-driven provider configs.
//!
//! Two primitives ported from the python adapter:
//!
//! 1. `apply_template(template, data)` — recursively walks a JSON
//!    `Value`, substituting any string of the exact form `"{key}"`
//!    with `data[key]`. Strings that *contain* `{key}` (rather than
//!    being exactly `"{key}"`) are left as-is.
//!
//! 2. `resolve_path(value, "a.0.b")` — dot-path lookup with array
//!    indices. Returns `None` on any miss. Used to extract fields
//!    from heterogeneous provider response payloads using paths
//!    declared in YAML (e.g. `candidates.0.content.parts`).
//!
//! Neither helper allocates more than necessary. Both are pure
//! functions; no I/O, no panics.

use serde_json::Value;
use std::collections::HashMap;

/// Recursively apply `{key}` placeholders to a JSON template.
///
/// - Strings of the exact form `"{key}"` (after trimming) are
///   replaced wholesale with `data[key]` (preserving the value's
///   JSON type — array, object, number, etc.).
/// - Strings without exact-form placeholders are returned verbatim.
/// - Objects and arrays recurse.
/// - Other primitives (numbers, bools, null) are cloned.
///
/// Missing placeholders resolve to `Value::Null` (not an error).
pub fn apply_template(template: &Value, data: &HashMap<String, Value>) -> Value {
    match template {
        Value::String(s) => {
            let trimmed = s.trim();
            if let Some(key) = trimmed
                .strip_prefix('{')
                .and_then(|rest| rest.strip_suffix('}'))
            {
            // Whole-string placeholder: substitute the value.
            match data.get(key) {
                Some(v) => v.clone(),
                None => {
                    tracing::warn!(
                        missing_placeholder = key,
                        available_keys = ?data.keys().collect::<Vec<_>>(),
                        "template placeholder not found in context — emitting null; \
                         this is almost certainly a YAML typo"
                    );
                    Value::Null
                }
            }
            } else {
                // Plain string: return as-is.
                Value::String(s.clone())
            }
        }
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|v| apply_template(v, data)).collect())
        }
        Value::Object(obj) => {
            let mut out = serde_json::Map::with_capacity(obj.len());
            for (k, v) in obj {
                out.insert(k.clone(), apply_template(v, data));
            }
            Value::Object(out)
        }
        // Numbers, bools, null: clone unchanged.
        other => other.clone(),
    }
}

/// Resolve a dot-separated path against a JSON `Value`.
///
/// Path syntax:
/// - `"a.b.c"` walks object keys
/// - `"a.0.b"` indexes array `a` at index 0
/// - Empty path returns the input
///
/// Returns `None` if any segment is missing or the type doesn't
/// support indexing (e.g. trying to walk into a string).
pub fn resolve_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(value);
    }
    let mut current = value;
    for segment in path.split('.') {
        current = match current {
            Value::Object(map) => map.get(segment)?,
            Value::Array(arr) => {
                let idx: usize = segment.parse().ok()?;
                arr.get(idx)?
            }
            _ => return None,
        };
    }
    Some(current)
}

/// Deep-merge `overlay` into `base`. Object keys from `overlay`
/// replace those in `base`. Arrays are replaced wholesale, not
/// concatenated. Used to apply `body_extra` over a templated body.
pub fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, other) => {
            *slot = other.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data(pairs: &[(&str, Value)]) -> HashMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn apply_template_substitutes_whole_string_placeholder() {
        let t = json!({"model": "{model}"});
        let d = data(&[("model", json!("claude-haiku"))]);
        assert_eq!(apply_template(&t, &d), json!({"model": "claude-haiku"}));
    }

    #[test]
    fn apply_template_substitutes_with_complex_value() {
        let t = json!({"messages": "{messages}"});
        let d = data(&[("messages", json!([{"role": "user", "content": "hi"}]))]);
        assert_eq!(
            apply_template(&t, &d),
            json!({"messages": [{"role": "user", "content": "hi"}]})
        );
    }

    #[test]
    fn apply_template_recurses_into_nested_objects() {
        let t = json!({"a": {"b": {"c": "{val}"}}});
        let d = data(&[("val", json!(42))]);
        assert_eq!(apply_template(&t, &d), json!({"a": {"b": {"c": 42}}}));
    }

    #[test]
    fn apply_template_recurses_into_arrays() {
        let t = json!({"contents": [{"role": "user", "parts": [{"text": "{text}"}]}]});
        let d = data(&[("text", json!("hello"))]);
        assert_eq!(
            apply_template(&t, &d),
            json!({"contents": [{"role": "user", "parts": [{"text": "hello"}]}]})
        );
    }

    #[test]
    fn apply_template_leaves_partial_placeholder_strings_unchanged() {
        let t = json!({"greeting": "Hello {name}"});
        let d = data(&[("name", json!("Leo"))]);
        assert_eq!(apply_template(&t, &d), json!({"greeting": "Hello {name}"}));
    }

    #[test]
    fn apply_template_missing_placeholder_becomes_null() {
        let t = json!({"x": "{missing}"});
        let d = data(&[]);
        assert_eq!(apply_template(&t, &d), json!({"x": null}));
    }

    #[test]
    fn apply_template_passes_through_non_string_primitives() {
        let t = json!({"max_tokens": 1024, "stream": true, "temp": 0.7, "n": null});
        let d = data(&[]);
        assert_eq!(
            apply_template(&t, &d),
            json!({"max_tokens": 1024, "stream": true, "temp": 0.7, "n": null})
        );
    }

    #[test]
    fn resolve_path_walks_object_keys() {
        let v = json!({"a": {"b": {"c": 99}}});
        assert_eq!(resolve_path(&v, "a.b.c"), Some(&json!(99)));
    }

    #[test]
    fn resolve_path_indexes_arrays() {
        let v = json!({"items": [{"name": "first"}, {"name": "second"}]});
        assert_eq!(resolve_path(&v, "items.1.name"), Some(&json!("second")));
    }

    #[test]
    fn resolve_path_empty_returns_root() {
        let v = json!({"a": 1});
        assert_eq!(resolve_path(&v, ""), Some(&json!({"a": 1})));
    }

    #[test]
    fn resolve_path_missing_segment_returns_none() {
        let v = json!({"a": {"b": 1}});
        assert_eq!(resolve_path(&v, "a.c"), None);
    }

    #[test]
    fn resolve_path_index_out_of_bounds_returns_none() {
        let v = json!({"items": [1, 2]});
        assert_eq!(resolve_path(&v, "items.5"), None);
    }

    #[test]
    fn resolve_path_into_primitive_returns_none() {
        let v = json!({"a": "not an object"});
        assert_eq!(resolve_path(&v, "a.b"), None);
    }

    #[test]
    fn deep_merge_replaces_keys_recursively() {
        let mut base = json!({"a": 1, "b": {"c": 2, "d": 3}});
        let overlay = json!({"b": {"c": 99}, "e": 5});
        deep_merge(&mut base, &overlay);
        assert_eq!(base, json!({"a": 1, "b": {"c": 99, "d": 3}, "e": 5}));
    }

    #[test]
    fn deep_merge_arrays_replace_not_concatenate() {
        let mut base = json!({"x": [1, 2, 3]});
        let overlay = json!({"x": [9]});
        deep_merge(&mut base, &overlay);
        assert_eq!(base, json!({"x": [9]}));
    }
}
