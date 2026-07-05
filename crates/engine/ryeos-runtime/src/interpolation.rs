use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::condition::resolve_path;

static INTERP_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$\{([^}]+)\}").unwrap());

/// If `template` is a SINGLE whole `${ … }` expression, return its inner text.
/// Unlike a `[^}]`-based regex this tolerates balanced braces inside — object /
/// empty-object literals in a `||` fallback (`${x || {}}`) — while still
/// rejecting multi-expression strings (`${a}${b}`) whose stripped body has an
/// unbalanced `}`.
fn whole_expr_inner(template: &str) -> Option<&str> {
    let inner = template.strip_prefix("${")?.strip_suffix('}')?;
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth < 0 {
                    return None; // a `}` closed the expression early
                }
            }
            _ => {}
        }
    }
    (depth == 0).then_some(inner)
}

static INPUT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{input:(\w+)(\?|[:|][^}]*)?\}").unwrap());

pub fn interpolate(template: &Value, context: &Value) -> anyhow::Result<Value> {
    match template {
        Value::String(s) => interpolate_string(s, context),
        Value::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                result.insert(k.clone(), interpolate(v, context)?);
            }
            Ok(Value::Object(result))
        }
        Value::Array(arr) => {
            let result: Vec<Value> = arr
                .iter()
                .map(|v| interpolate(v, context))
                .collect::<anyhow::Result<_>>()?;
            Ok(Value::Array(result))
        }
        other => Ok(other.clone()),
    }
}

/// Input/context keys a template references via `{input:KEY}` or `${KEY...}`.
///
/// Callers that also surface raw inputs (e.g. the directive runtime appending
/// an `Inputs:` block) use this to avoid re-rendering inputs the template
/// already placed — a body of `{input:input}` consumes `input`, so it is not
/// dumped a second time.
pub fn referenced_input_keys(template: &str) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    for cap in INPUT_RE.captures_iter(template) {
        keys.insert(cap[1].to_string());
    }
    for cap in INTERP_RE.captures_iter(template) {
        // Inputs referenced via expression are `${inputs.KEY ...}` — inputs
        // live under the `inputs` context key; other paths (state, etc.) are
        // not inputs and must not suppress the inputs dump.
        if let Some(rest) = cap[1].trim_start().strip_prefix("inputs.") {
            let key: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !key.is_empty() {
                keys.insert(key);
            }
        }
    }
    keys
}

fn interpolate_string(template: &str, context: &Value) -> anyhow::Result<Value> {
    tracing::trace!(template = %template, "interpolating template");

    // Whole-expression fast path: if the entire template is a single ${...},
    // resolve it and preserve the native type (number, bool, etc.) instead
    // of stringifying.
    if let Some(expr) = whole_expr_inner(template) {
        if let Some(val) = resolve_expression(expr, context) {
            return Ok(val);
        }
        // Whole expression didn't resolve — fall through to error collection
        // in the scanning loop below (will report "unresolved expression").
    }

    // If nothing looks like an interpolation, return the template as-is.
    if !INTERP_RE.is_match(template) && !INPUT_RE.is_match(template) {
        return Ok(Value::String(template.to_string()));
    }

    // Scan the template, resolving ${...} expressions and {input:...}
    // references. Both must resolve — missing inputs and unresolved paths
    // are errors unless the author explicitly opts out:
    //   - ${a || b}  → explicit fallback (try a, use b if null)
    //   - {input:x?} → optional input (empty if missing)
    //   - {input:x:default} → default value if missing
    //   - {input:x|default} → default value if missing
    let mut result = String::new();
    let mut remaining = template;

    while !remaining.is_empty() {
        // Try ${...} expression
        if let Some(caps) = INTERP_RE.captures(remaining) {
            let full_match = caps.get(0).unwrap();
            let match_start = full_match.start();

            // Try {input:...} reference (takes precedence if it appears
            // before or at the same position as the next ${...})
            if let Some(input_caps) = INPUT_RE.captures(remaining) {
                let input_start = input_caps.get(0).unwrap().start();
                if input_start <= match_start {
                    let input_match = input_caps.get(0).unwrap();
                    let name = &input_caps[1];
                    let modifier = input_caps.get(2).map(|m| m.as_str()).unwrap_or("");

                    result.push_str(&remaining[..input_start]);
                    result.push_str(&resolve_input(name, modifier, context)?);
                    remaining = &remaining[input_match.end()..];
                    continue;
                }
            }

            let expr = &caps[1];
            result.push_str(&remaining[..match_start]);
            match resolve_expression(expr, context) {
                Some(val) => match stringify_value(&val) {
                    Some(s) => result.push_str(&s),
                    None => {
                        // Structured values embed in text only via the
                        // explicit `|json` pipe — name the fix so a template
                        // author (or a directive body referencing an
                        // object-valued input) gets an actionable error
                        // instead of an opaque bootstrap failure.
                        anyhow::bail!(
                            "interpolation: ${{{expr}}} resolved to a structured value ({}); \
                             embedding an object/array in text requires the explicit json \
                             pipe: ${{{expr}|json}}",
                            val
                        );
                    }
                },
                None => {
                    anyhow::bail!(
                        "interpolation: ${{{}}} could not be resolved — \
                         no matching path in context and no || fallback provided",
                        expr
                    );
                }
            }
            remaining = &remaining[full_match.end()..];
            continue;
        }

        // Try {input:...} reference (only remaining pattern)
        if let Some(caps) = INPUT_RE.captures(remaining) {
            let full_match = caps.get(0).unwrap();
            let name = &caps[1];
            let modifier = caps.get(2).map(|m| m.as_str()).unwrap_or("");

            result.push_str(&remaining[..full_match.start()]);
            result.push_str(&resolve_input(name, modifier, context)?);
            remaining = &remaining[full_match.end()..];
            continue;
        }

        // No more interpolation patterns — append the rest verbatim.
        result.push_str(remaining);
        break;
    }

    Ok(Value::String(result))
}

/// Resolve an `{input:NAME}` or `{input:NAMEMODIFIER}` reference.
///
/// Modifier semantics:
///   - `?`        → optional: return empty string if missing
///   - `:default` → return `default` if missing
///   - `|default` → return `default` if missing
///   - (none)     → **required**: return error if missing
fn resolve_input(name: &str, modifier: &str, context: &Value) -> anyhow::Result<String> {
    let inputs = context.get("inputs");

    match inputs.and_then(|i| i.get(name)) {
        Some(val) => {
            // Input values must be strings — numeric/boolean inputs would
            // produce confusing results in template expansion. Operators
            // who need non-string inputs should use ${...} expressions with
            // the inputs path instead.
            val.as_str().map(|s| Ok(s.to_string())).unwrap_or_else(|| {
                anyhow::bail!(
                    "interpolation: {{input:{name}}} resolved to {val} which is not a string — \
                         use ${{inputs.{name}}} for non-string input access"
                )
            })
        }
        None => match modifier {
            "?" => Ok(String::new()),
            m if m.starts_with(':') => Ok(m[1..].to_string()),
            m if m.starts_with('|') => Ok(m[1..].to_string()),
            _ => anyhow::bail!(
                "interpolation: {{input:{name}}} is missing from inputs and \
                 has no fallback modifier. Use {{input:{name}?}} for optional \
                 or {{input:{name}:default}} for a default value."
            ),
        },
    }
}

fn resolve_expression(expr: &str, context: &Value) -> Option<Value> {
    let (path_part, pipes) = parse_pipes(expr);

    let fallbacks: Vec<&str> = path_part.split("||").map(|s| s.trim()).collect();

    let mut resolved: Option<Value> = None;
    for fallback in &fallbacks {
        // A `||` token is either a bare PATH (identifier chain: `state.x`,
        // `result.outputs.y`) or a JSON LITERAL (`0`, `-1.5`, `"text"`, `[]`,
        // `{}`, `true`, `false`, `null`). A literal always parses as JSON; a
        // path never does (bare identifiers / dotted chains are not JSON), so
        // try-JSON-first disambiguates. A literal is an explicit author-written
        // default: it is used as-is when reached (including `null`) — no state
        // sentinel needed, and the fallback lives at the use-site.
        if let Ok(literal) = serde_json::from_str::<Value>(fallback) {
            resolved = Some(literal);
            break;
        }
        // Otherwise resolve as a path. Resolve-or-else, NOT truthiness: a path
        // that is unresolved (no match) or resolves to `null` falls through to
        // the next token; a resolved value the author produced on purpose —
        // including `""`, `[]`, `0`, `false` — passes through untouched.
        if let Some(v) = resolve_path(context, fallback) {
            if !v.is_null() {
                resolved = Some(v.clone());
                break;
            }
        }
    }

    let resolved = resolved?;
    apply_pipes(resolved, &pipes)
}

fn parse_pipes(expr: &str) -> (String, Vec<String>) {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = expr.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '|' {
            if i + 1 < chars.len() && chars[i + 1] == '|' {
                current.push_str("||");
                i += 2;
            } else {
                parts.push(std::mem::take(&mut current));
                i += 1;
            }
        } else {
            current.push(chars[i]);
            i += 1;
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }

    if parts.len() <= 1 {
        return (expr.to_string(), Vec::new());
    }
    let path = parts[0].clone();
    let pipes = parts[1..].iter().map(|s| s.trim().to_string()).collect();
    (path, pipes)
}

fn apply_pipes(val: Value, pipes: &[String]) -> Option<Value> {
    let mut result = val;
    for pipe in pipes {
        result = match pipe.as_str() {
            "json" => {
                // Serialize ANY JSON value, not just scalars. This is the
                // explicit escape hatch for embedding objects/arrays in
                // prompt text: `"Stats: ${state.stats|json}"`. Bare embedded
                // `${path}` without `|json` still errors on objects/arrays
                // (see `stringify_value`), forcing authors to opt in.
                let s = serde_json::to_string(&result).ok()?;
                Value::String(s)
            }
            "from_json" => {
                let s = result.as_str()?;
                serde_json::from_str(s).ok()?
            }
            "length" => match &result {
                Value::Array(a) => Value::Number(a.len().into()),
                Value::Object(o) => Value::Number(o.len().into()),
                Value::String(s) => Value::Number(s.len().into()),
                _ => return None,
            },
            "keys" => match &result {
                Value::Object(o) => {
                    Value::Array(o.keys().map(|k| Value::String(k.clone())).collect())
                }
                _ => return None,
            },
            "upper" => {
                let s = result.as_str()?;
                Value::String(s.to_uppercase())
            }
            "lower" => {
                let s = result.as_str()?;
                Value::String(s.to_lowercase())
            }
            "type" => Value::String(
                match &result {
                    Value::Null => "null",
                    Value::Bool(_) => "bool",
                    Value::Number(_) => "number",
                    Value::String(_) => "string",
                    Value::Array(_) => "array",
                    Value::Object(_) => "object",
                }
                .to_string(),
            ),
            _ => return None, // Unknown pipe → expression unresolvable
        };
    }
    Some(result)
}

fn stringify_value(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some(String::new()),
        _ => None,
    }
}

pub fn interpolate_action(action: &Value, context: &Value) -> anyhow::Result<Value> {
    match action {
        Value::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                // The template-bearing key set is owned by
                // `callback::action_keys` next to the `ActionPayload` wire
                // shape. `facets` values are templates by design
                // (`fleet: "${inputs.fleet}"`) and MUST resolve here, with
                // the foreach item variable in scope, or the daemon stamps
                // the raw template strings on the child.
                if crate::callback::action_keys::INTERPOLATED.contains(&k.as_str()) {
                    result.insert(k.clone(), interpolate(v, context)?);
                } else {
                    result.insert(k.clone(), v.clone());
                }
            }
            Ok(Value::Object(result))
        }
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn interpolate_action_resolves_facets_with_item_var() {
        // Cohort facets on a detach action are templates that must resolve
        // per iteration — a raw `${item}` reaching the daemon gets stamped
        // verbatim on the child and breaks every facet query for the cohort.
        let action = json!({
            "item_id": "graph:t/leaf",
            "params": {"x": "${item}"},
            "thread": "detached",
            "facets": {"fleet": "${inputs.fleet}", "member": "${item}"},
        });
        let ctx = json!({"inputs": {"fleet": "f-1"}, "item": "alpha"});
        let out = interpolate_action(&action, &ctx).unwrap();
        assert_eq!(out["facets"], json!({"fleet": "f-1", "member": "alpha"}));
        assert_eq!(out["params"], json!({"x": "alpha"}));
        assert_eq!(out["thread"], json!("detached"));
    }

    #[test]
    fn whole_expression_preserves_type() {
        let ctx = json!({"state": {"count": 3}});
        assert_eq!(
            interpolate(&json!("${state.count}"), &ctx).unwrap(),
            json!(3)
        );
    }

    #[test]
    fn partial_string_stringifies() {
        let ctx = json!({"user": {"name": "Alice"}, "count": 3});
        assert_eq!(
            interpolate(&json!("Hello ${user.name} (${count})"), &ctx).unwrap(),
            json!("Hello Alice (3)")
        );
    }

    #[test]
    fn fallback_and_pipe() {
        let ctx = json!({"backup": {"name": "alice"}});
        assert_eq!(
            interpolate(&json!("${state.missing || backup.name | upper}"), &ctx).unwrap(),
            json!("ALICE")
        );
    }

    #[test]
    fn literal_fallbacks_keep_their_json_type_as_a_whole_expr() {
        let ctx = json!({"result": {"outputs": {}}});
        // Missing path → the literal default, with its JSON type preserved.
        assert_eq!(
            interpolate(&json!("${result.outputs.confidence || 0}"), &ctx).unwrap(),
            json!(0)
        );
        assert_eq!(
            interpolate(&json!("${result.outputs.body || \"\"}"), &ctx).unwrap(),
            json!("")
        );
        assert_eq!(
            interpolate(&json!("${result.outputs.cases || []}"), &ctx).unwrap(),
            json!([])
        );
        assert_eq!(
            interpolate(&json!("${result.outputs.flag || false}"), &ctx).unwrap(),
            json!(false)
        );
        // Object literal — needs the brace-aware whole-expr extraction.
        assert_eq!(
            interpolate(&json!("${result.outputs.obj || {}}"), &ctx).unwrap(),
            json!({})
        );
    }

    #[test]
    fn resolve_or_else_passes_resolved_falsy_through() {
        // A resolved-but-falsy value the author produced (empty array, 0, "")
        // is NOT overridden by the fallback — only unresolved/null falls through.
        let ctx = json!({"inputs": {"game_ids": []}, "state": {"defaults": [1, 2]}});
        assert_eq!(
            interpolate(&json!("${inputs.game_ids || state.defaults}"), &ctx).unwrap(),
            json!([]),
            "an explicit empty array is a value, not a trigger for the fallback"
        );
        // Unresolved → the fallback (a literal here).
        assert_eq!(
            interpolate(&json!("${inputs.missing || [1, 2]}"), &ctx).unwrap(),
            json!([1, 2])
        );
    }

    #[test]
    fn path_fallbacks_still_resolve_and_chain() {
        // A bare identifier chain is still a path (not JSON), so existing
        // `|| state.x` graphs are unchanged; chaining picks the first resolved.
        let ctx = json!({"state": {"primary": null, "backup": "b"}});
        assert_eq!(
            interpolate(&json!("${state.primary || state.backup || \"z\"}"), &ctx).unwrap(),
            json!("b")
        );
    }

    #[test]
    fn input_refs() {
        let ctx = json!({"inputs": {"name": "Rye"}});
        assert_eq!(
            interpolate(
                &json!("A={input:name};B={input:missing?};C={input:missing:default}"),
                &ctx
            )
            .unwrap(),
            json!("A=Rye;B=;C=default")
        );
    }

    #[test]
    fn missing_required_input_errors() {
        let ctx = json!({"inputs": {"name": "Rye"}});
        let result = interpolate(&json!("{input:nonexistent}"), &ctx);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("nonexistent") && msg.contains("missing from inputs"),
            "error message should name the missing input: {msg}"
        );
    }

    #[test]
    fn unresolved_expression_errors() {
        let ctx = json!({"inputs": {}});
        let result = interpolate(&json!("Hello ${state.nonexistent}"), &ctx);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("state.nonexistent") && msg.contains("no || fallback"),
            "error message should name the path and suggest fallback: {msg}"
        );
    }

    #[test]
    fn unresolved_expression_with_fallback_is_ok() {
        let ctx = json!({"backup": {"name": "alice"}});
        assert_eq!(
            interpolate(&json!("${state.missing || backup.name}"), &ctx).unwrap(),
            json!("alice")
        );
    }

    #[test]
    fn optional_input_with_question_mark_is_ok() {
        let ctx = json!({"inputs": {}});
        assert_eq!(
            interpolate(&json!("Hello {input:name?}!"), &ctx).unwrap(),
            json!("Hello !")
        );
    }

    #[test]
    fn optional_input_with_default_is_ok() {
        let ctx = json!({"inputs": {}});
        assert_eq!(
            interpolate(&json!("Hello {input:name:World}!"), &ctx).unwrap(),
            json!("Hello World!")
        );
    }

    #[test]
    fn interpolate_action_touches_item_id_params_and_call() {
        let action = json!({
            "item_id": "${tool.id}",
            "params": {"n": "${x}"},
            "call": {"method": "query", "args": {"query": "${q}"}},
            "other": "${y}"
        });
        let ctx = json!({"tool": {"id": "ryeos/tool"}, "x": 1, "y": 2, "q": "hint"});
        let result = interpolate_action(&action, &ctx).unwrap();
        assert_eq!(result["item_id"], "ryeos/tool");
        assert_eq!(result["params"]["n"], 1);
        // `call.args` templates resolve; literal `call.method` passes through.
        assert_eq!(result["call"]["method"], "query");
        assert_eq!(result["call"]["args"]["query"], "hint");
        // Non-dispatch keys are still left untouched.
        assert_eq!(result["other"], "${y}");
    }

    #[test]
    fn no_interpolation_passthrough() {
        let ctx = json!({});
        assert_eq!(
            interpolate(&json!("plain text"), &ctx).unwrap(),
            json!("plain text")
        );
    }

    #[test]
    fn nested_object_interpolation() {
        let ctx = json!({"a": 1});
        let template = json!({"x": "${a}", "nested": {"y": "${a}"}});
        let result = interpolate(&template, &ctx).unwrap();
        assert_eq!(result["x"], 1);
        assert_eq!(result["nested"]["y"], 1);
    }

    #[test]
    fn pipe_length() {
        let ctx = json!({"items": [1, 2, 3]});
        assert_eq!(
            interpolate(&json!("${items | length}"), &ctx).unwrap(),
            json!(3)
        );
    }

    #[test]
    fn whole_expression_preserves_object() {
        // Whole-expression `${path}` resolving to an object keeps the native
        // JSON type — the structured-parameter transport path.
        let ctx = json!({"state": {"stats": {"hp": 7, "items": ["a", "b"]}}});
        assert_eq!(
            interpolate(&json!("${state.stats}"), &ctx).unwrap(),
            json!({"hp": 7, "items": ["a", "b"]})
        );
    }

    #[test]
    fn interpolate_action_params_accept_object_from_inputs() {
        // A graph action passing an object into a directive param via
        // whole-expression interpolation must arrive as structured JSON.
        let action = json!({
            "item_id": "directive:arc/reason",
            "params": {"stats": "${inputs.stats}"}
        });
        let ctx = json!({"inputs": {"stats": {"hp": 7}}});
        let result = interpolate_action(&action, &ctx).unwrap();
        assert_eq!(result["params"]["stats"], json!({"hp": 7}));
    }

    #[test]
    fn embedded_json_pipe_serializes_object() {
        let ctx = json!({"state": {"stats": {"hp": 7}}});
        assert_eq!(
            interpolate(&json!("Stats: ${state.stats|json}"), &ctx).unwrap(),
            json!("Stats: {\"hp\":7}")
        );
    }

    #[test]
    fn embedded_json_pipe_serializes_array() {
        let ctx = json!({"state": {"items": ["a", "b"]}});
        assert_eq!(
            interpolate(&json!("Items: ${state.items|json}"), &ctx).unwrap(),
            json!("Items: [\"a\",\"b\"]")
        );
    }

    #[test]
    fn from_json_round_trips_json_pipe() {
        // `|json` serializes, `|from_json` parses back to the original value.
        let ctx = json!({"state": {"stats": {"hp": 7, "items": ["a"]}}});
        let serialized = interpolate(&json!("${state.stats|json}"), &ctx).unwrap();
        let reparsed =
            interpolate(&json!("${blob|from_json}"), &json!({"blob": serialized})).unwrap();
        assert_eq!(reparsed, json!({"hp": 7, "items": ["a"]}));
    }

    #[test]
    fn embedded_object_without_json_pipe_errors() {
        // Embedding an object in text WITHOUT `|json` must still error —
        // authors must opt into serialization explicitly. The error must
        // name the expression AND the `|json` fix, so a directive body
        // referencing an object-valued input fails legibly at bootstrap
        // instead of surfacing as an opaque child-runtime failure.
        let ctx = json!({"state": {"stats": {"hp": 7}}});
        let result = interpolate(&json!("Stats: ${state.stats}"), &ctx);
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("${state.stats|json}"),
            "error should name the expression and the |json fix: {msg}"
        );
    }
}
