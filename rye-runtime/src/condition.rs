use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

static BRACKET_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[(\d+)\]").unwrap());

pub fn resolve_path<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let normalized = BRACKET_RE.replace_all(path, ".$1");
    let mut current = doc;
    for segment in normalized.split('.') {
        if segment.is_empty() {
            continue;
        }
        match current {
            Value::Object(map) => {
                current = map.get(segment)?;
            }
            Value::Array(arr) => {
                let idx: usize = segment.parse().ok()?;
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

pub fn apply_operator(actual: Option<&Value>, op: &str, expected: &Value) -> anyhow::Result<bool> {
    let actual_val = actual.unwrap_or(&Value::Null);
    match op {
        "eq" => Ok(actual_val == expected),
        "ne" => Ok(actual_val != expected),
        "gt" | "gte" | "lt" | "lte" => {
            let ord = match (actual_val, expected) {
                (Value::Number(a), Value::Number(b)) => {
                    if let (Some(af), Some(bf)) = (a.as_f64(), b.as_f64()) {
                        Some(af.partial_cmp(&bf))
                    } else {
                        None
                    }
                }
                (Value::String(a), Value::String(b)) => Some(a.cmp(b).into()),
                _ => None,
            };
            let cmp = ord.and_then(|o| o);
            let result = match op {
                "gt" => cmp.map(|o| o.is_gt()),
                "gte" => cmp.map(|o| o.is_ge()),
                "lt" => cmp.map(|o| o.is_lt()),
                "lte" => cmp.map(|o| o.is_le()),
                _ => unreachable!(),
            };
            Ok(result.unwrap_or(false))
        }
        "in" => match expected {
            Value::Array(arr) => {
                let actual_or_null = actual.unwrap_or(&Value::Null);
                Ok(arr.iter().any(|item| item == actual_or_null))
            }
            _ => Ok(false),
        },
        "contains" => {
            let s: Cow<'_, str> = match actual_val {
                Value::String(st) => Cow::Borrowed(st.as_str()),
                _ => Cow::Owned(actual_val.to_string()),
            };
            let needle = expected.as_str().unwrap_or("");
            Ok(s.contains(needle))
        }
        "regex" => {
            let s: Cow<'_, str> = match actual_val {
                Value::String(st) => Cow::Borrowed(st.as_str()),
                _ => Cow::Owned(actual_val.to_string()),
            };
            let pattern = expected.as_str().unwrap_or("");
            let re = Regex::new(pattern)?;
            Ok(re.is_match(&s))
        }
        "exists" => Ok(actual.is_some()),
        _ => anyhow::bail!("unknown operator: {op}"),
    }
}

pub fn matches(doc: &Value, condition: &Value) -> anyhow::Result<bool> {
    if condition.is_null() || condition.as_object().map_or(false, |o| o.is_empty()) {
        return Ok(true);
    }

    if let Some(any) = condition.get("any") {
        let arr = any
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("'any' must be an array"))?;
        return Ok(arr.iter().any(|c| matches(doc, c).unwrap_or(false)));
    }

    if let Some(all) = condition.get("all") {
        let arr = all
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("'all' must be an array"))?;
        return Ok(arr.iter().all(|c| matches(doc, c).unwrap_or(false)));
    }

    if let Some(neg) = condition.get("not") {
        return Ok(!matches(doc, neg)?);
    }

    let path_val = condition
        .get("path")
        .and_then(|p| p.as_str())
        .ok_or_else(|| anyhow::anyhow!("condition must have 'path'"))?;
    let op = condition
        .get("op")
        .and_then(|o| o.as_str())
        .ok_or_else(|| anyhow::anyhow!("condition must have 'op'"))?;
    let expected = condition
        .get("value")
        .ok_or_else(|| anyhow::anyhow!("condition must have 'value'"))?;

    let actual = resolve_path(doc, path_val);
    apply_operator(actual, op, expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_path_supports_dot_and_bracket_indices() {
        let doc = json!({"state": {"items": [{"name": "alpha"}, {"name": "beta"}]}});
        assert_eq!(
            resolve_path(&doc, "state.items.0.name").cloned(),
            Some(json!("alpha"))
        );
        assert_eq!(
            resolve_path(&doc, "state.items[1].name").cloned(),
            Some(json!("beta"))
        );
    }

    #[test]
    fn resolve_path_returns_none_for_missing() {
        let doc = json!({"state": {"items": []}});
        assert_eq!(resolve_path(&doc, "state.items[3].name"), None);
    }

    #[test]
    fn matches_supports_any_all_not() {
        let doc = json!({"status": "running", "cost": {"turns": 4}, "flags": {"blocked": false}});
        let cond = json!({"all": [
            {"path": "status", "op": "eq", "value": "running"},
            {"any": [{"path": "cost.turns", "op": "gte", "value": 3}]},
            {"not": {"path": "flags.blocked", "op": "eq", "value": true}}
        ]});
        assert!(matches(&doc, &cond).unwrap());
    }

    #[test]
    fn missing_actual_is_null_for_eq() {
        let doc = json!({"a": 1});
        assert!(matches(&doc, &json!({"path": "missing", "op": "eq", "value": null})).unwrap());
    }

    #[test]
    fn exists_operator() {
        let doc = json!({"a": 1});
        assert!(matches(&doc, &json!({"path": "a", "op": "exists", "value": true})).unwrap());
        assert!(!matches(&doc, &json!({"path": "b", "op": "exists", "value": true})).unwrap());
    }

    #[test]
    fn contains_operator() {
        let doc = json!({"msg": "hello world"});
        assert!(matches(
            &doc,
            &json!({"path": "msg", "op": "contains", "value": "world"})
        )
        .unwrap());
    }

    #[test]
    fn in_operator() {
        let doc = json!({"status": "running"});
        assert!(matches(
            &doc,
            &json!({"path": "status", "op": "in", "value": ["running", "pending"]})
        )
        .unwrap());
    }

    #[test]
    fn empty_condition_is_true() {
        let doc = json!({"a": 1});
        assert!(matches(&doc, &json!({})).unwrap());
        assert!(matches(&doc, &Value::Null).unwrap());
    }
}
