//! Signed per-item route parameters for structured-session workers.
//!
//! A worker execution item may pin request fields on routes of the worker's
//! protocol profile: `route_fixed_params` maps a route id to the fields the
//! item fixes for that route. The item is signed data and sets ceilings; a
//! goal payload may only narrow them, so a caller field that names a pinned
//! field is refused by the worker exactly as a profile `fixed_params` field is.
//!
//! The shape is closed and bounded here, once, for the launch preparer, the
//! daemon's session start and the worker itself. Judging against a profile
//! never overrides the profile: a field the route already fixes, fills from the
//! workspace or forbids is refused.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};
use serde_json::Value;

/// The most routes one item may pin fields on.
pub const MAX_ROUTE_FIXED_PARAM_ROUTES: usize = 8;
/// The most fields one route may have pinned.
pub const MAX_ROUTE_FIXED_PARAM_FIELDS: usize = 32;
/// The longest pinned field name, in bytes.
pub const MAX_ROUTE_FIXED_PARAM_FIELD_NAME_BYTES: usize = 64;
/// The most bytes the whole serialized map may take.
pub const MAX_ROUTE_FIXED_PARAMS_BYTES: usize = 16 * 1024;
/// The worker environment variable carrying the admitted pins, as a JSON
/// object of [`RouteFixedParams`]. Absent means the launch pins nothing.
pub const ROUTE_FIXED_PARAMS_ENV: &str = "RYEOS_STRUCTURED_SESSION_ROUTE_FIXED_PARAMS";
/// The longest route id, in bytes.
const MAX_ROUTE_ID_BYTES: usize = 128;

/// Route id to the fields the item fixes on that route.
pub type RouteFixedParams = BTreeMap<String, BTreeMap<String, Value>>;

/// Decode and bound an item's `route_fixed_params` value. This judges only the
/// closed shape; [`check_route_fixed_params_against_profile`] judges it
/// against the worker's admitted profile.
pub fn parse_route_fixed_params(value: &Value) -> Result<RouteFixedParams> {
    let routes = value
        .as_object()
        .ok_or_else(|| anyhow!("route_fixed_params must be an object"))?;
    if routes.len() > MAX_ROUTE_FIXED_PARAM_ROUTES {
        bail!("route_fixed_params names more than {MAX_ROUTE_FIXED_PARAM_ROUTES} routes");
    }
    if serde_json::to_vec(value)?.len() > MAX_ROUTE_FIXED_PARAMS_BYTES {
        bail!("route_fixed_params exceeds {MAX_ROUTE_FIXED_PARAMS_BYTES} serialized bytes");
    }
    let mut parsed = RouteFixedParams::new();
    for (route_id, fields) in routes {
        if !canonical_route_id(route_id) {
            bail!("route_fixed_params route id `{route_id}` is not a canonical route identifier");
        }
        let fields = fields
            .as_object()
            .filter(|fields| !fields.is_empty())
            .ok_or_else(|| {
                anyhow!("route_fixed_params for `{route_id}` must be a non-empty object")
            })?;
        if fields.len() > MAX_ROUTE_FIXED_PARAM_FIELDS {
            bail!(
                "route_fixed_params for `{route_id}` pins more than \
                 {MAX_ROUTE_FIXED_PARAM_FIELDS} fields"
            );
        }
        let mut pinned = BTreeMap::new();
        for (field, field_value) in fields {
            if field.is_empty() || field.len() > MAX_ROUTE_FIXED_PARAM_FIELD_NAME_BYTES {
                bail!(
                    "route_fixed_params field name on `{route_id}` is empty or longer than \
                     {MAX_ROUTE_FIXED_PARAM_FIELD_NAME_BYTES} bytes"
                );
            }
            pinned.insert(field.clone(), field_value.clone());
        }
        parsed.insert(route_id.clone(), pinned);
    }
    Ok(parsed)
}

fn canonical_route_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ROUTE_ID_BYTES
        && id.split('.').all(|segment| {
            !segment.is_empty()
                && segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_')
                })
        })
}

/// Judge an item's pinned fields against the admitted profile contract and the
/// route set the item selects. Every named route must be in that route set,
/// and no pinned field may be one the route already fixes, fills from the
/// workspace, or forbids.
pub fn check_route_fixed_params_against_profile(
    contract: &Value,
    route_set: &str,
    params: &RouteFixedParams,
) -> Result<()> {
    if params.is_empty() {
        return Ok(());
    }
    let admitted: BTreeSet<&str> = contract
        .get("route_sets")
        .and_then(Value::as_object)
        .and_then(|sets| sets.get(route_set))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("route_fixed_params selects an unknown route set"))?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let routes = contract
        .get("routes")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("admitted structured-session contract has no route table"))?;
    for (route_id, fields) in params {
        if !admitted.contains(route_id.as_str()) {
            bail!("route_fixed_params names route `{route_id}` outside the admitted route set");
        }
        let route = routes
            .iter()
            .find(|route| route.get("id").and_then(Value::as_str) == Some(route_id))
            .ok_or_else(|| anyhow!("route_fixed_params names undefined route `{route_id}`"))?;
        let mut owned = BTreeSet::new();
        if let Some(fixed) = route.get("fixed_params").and_then(Value::as_object) {
            owned.extend(fixed.keys().map(String::as_str));
        }
        for key in [
            "workspace_fields",
            "forbidden_fields",
            "forbidden_non_null_fields",
        ] {
            if let Some(list) = route.get(key).and_then(Value::as_array) {
                owned.extend(list.iter().filter_map(Value::as_str));
            }
        }
        if let Some(field) = fields.keys().find(|field| owned.contains(field.as_str())) {
            bail!(
                "route_fixed_params field `{field}` on `{route_id}` is already owned or \
                 forbidden by the profile route"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contract() -> Value {
        json!({
            "route_sets": {"session": ["session.start", "turn.start"]},
            "routes": [
                {"id": "session.start", "fixed_params": {"access": "read_write"},
                 "workspace_fields": ["cwd"], "forbidden_fields": ["resume"],
                 "forbidden_non_null_fields": ["fork"]},
                {"id": "turn.start", "fixed_params": {}},
                {"id": "other.route", "fixed_params": {}}
            ]
        })
    }

    fn judge(value: Value) -> Result<RouteFixedParams> {
        let params = parse_route_fixed_params(&value)?;
        check_route_fixed_params_against_profile(&contract(), "session", &params)?;
        Ok(params)
    }

    #[test]
    fn admits_fields_the_profile_does_not_own() {
        let params =
            judge(json!({"session.start": {"effort": "high", "tools": ["Read"]}})).unwrap();
        assert_eq!(params["session.start"]["effort"], json!("high"));
    }

    #[test]
    fn refuses_overlap_with_profile_fixed_workspace_and_forbidden_fields() {
        for field in ["access", "cwd", "resume", "fork"] {
            assert!(
                judge(json!({"session.start": {field: 1}})).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn refuses_unknown_and_out_of_route_set_routes() {
        assert!(judge(json!({"missing.route": {"a": 1}})).is_err());
        assert!(judge(json!({"other.route": {"a": 1}})).is_err());
    }

    #[test]
    fn refuses_unbounded_or_malformed_shapes() {
        assert!(parse_route_fixed_params(&json!([])).is_err());
        assert!(parse_route_fixed_params(&json!({"Bad Id": {"a": 1}})).is_err());
        assert!(parse_route_fixed_params(&json!({"a.b": {}})).is_err());
        assert!(parse_route_fixed_params(&json!({"a.b": {"": 1}})).is_err());
        let long = "x".repeat(MAX_ROUTE_FIXED_PARAM_FIELD_NAME_BYTES + 1);
        assert!(parse_route_fixed_params(&json!({"a.b": {long: 1}})).is_err());
        let many: serde_json::Map<String, Value> = (0..=MAX_ROUTE_FIXED_PARAM_FIELDS)
            .map(|index| (format!("f{index}"), json!(1)))
            .collect();
        assert!(parse_route_fixed_params(&json!({"a.b": many})).is_err());
        let routes: serde_json::Map<String, Value> = (0..=MAX_ROUTE_FIXED_PARAM_ROUTES)
            .map(|index| (format!("r{index}.go"), json!({"a": 1})))
            .collect();
        assert!(parse_route_fixed_params(&Value::Object(routes)).is_err());
        let big = "x".repeat(MAX_ROUTE_FIXED_PARAMS_BYTES);
        assert!(parse_route_fixed_params(&json!({"a.b": {"a": big}})).is_err());
    }
}
