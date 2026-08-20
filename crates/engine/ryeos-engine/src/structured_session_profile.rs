//! Admission compiler for the closed structured-session protocol family.
//!
//! A profile is authority-bearing executable policy.  This compiler runs
//! while the worker source closure is being admitted, before any process is
//! launched.  It accepts only the fixed v1 vocabulary and exact local schema
//! blobs captured in the same signed source closure.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use ryeos_state::objects::AdmittedStructuredSessionProfile;

const MAX_PROFILE_BYTES: usize = 64 * 1024;
const MAX_SCHEMA_BYTES: usize = 8 * 1024 * 1024;
const MAX_SCHEMA_TOTAL_BYTES: usize = 16 * 1024 * 1024;

pub fn compile(
    profile_bytes: &[u8],
    source_files: &BTreeMap<String, Vec<u8>>,
) -> Result<AdmittedStructuredSessionProfile> {
    if profile_bytes.is_empty() || profile_bytes.len() > MAX_PROFILE_BYTES {
        bail!("structured-session profile is empty or exceeds its byte ceiling");
    }
    let profile: Value = serde_json::from_slice(profile_bytes)
        .context("decode structured-session profile during admission")?;
    let object = profile
        .as_object()
        .ok_or_else(|| anyhow!("structured-session profile must be an object"))?;
    let required = [
        "schema_version",
        "workload_realization_id",
        "workload_executable",
        "workload_args",
        "workload_home_env",
        "baseline_config",
        "baseline_destination",
        "initialization",
        "route_sets",
        "routes",
        "notifications",
        "ignored_notifications",
        "server_requests",
    ];
    if object.len() != required.len() || required.iter().any(|key| !object.contains_key(*key)) {
        bail!("structured-session profile has an unknown or missing top-level field");
    }
    if object.get("schema_version").and_then(Value::as_u64) != Some(1) {
        bail!("structured-session profile schema is not admitted");
    }
    validate_identifier(value_string(object, "workload_realization_id")?)?;
    validate_file_name(value_string(object, "workload_executable")?)?;
    validate_file_name(value_string(object, "baseline_config")?)?;
    validate_file_name(value_string(object, "baseline_destination")?)?;
    crate::protocol_vocabulary::validate_env_name(value_string(object, "workload_home_env")?)
        .map_err(|error| anyhow!(error))?;
    let workload_args = bounded_array(object, "workload_args", 0, 64)?;
    for argument in workload_args {
        let argument = argument
            .as_str()
            .ok_or_else(|| anyhow!("structured-session workload argument must be a string"))?;
        if argument.len() > 4096 || argument.chars().any(char::is_control) {
            bail!("structured-session workload argument is not bounded portable text");
        }
    }

    let routes = bounded_array(object, "routes", 1, 128)?;
    let mut route_ids = BTreeSet::new();
    let mut upstream_methods = BTreeSet::new();
    let allowed_effects = [
        "pure_read",
        "session_mutation",
        "external_effect",
        "credential_read",
        "credential_write",
        "credential_delete",
    ];
    let mut schema_ids = BTreeSet::new();
    for route in routes {
        let route = route
            .as_object()
            .ok_or_else(|| anyhow!("structured-session route must be an object"))?;
        require_keys(
            route,
            &[
                "id",
                "method",
                "effect_class",
                "request_schema",
                "response_schema",
                "fixed_params",
                "workspace_fields",
                "forbidden_non_null_fields",
                "response_predicates",
                "observations",
                "result_retention",
                "ceremony",
            ],
            &["audience", "session_binding"],
        )?;
        let id = value_string(route, "id")?;
        let method = value_string(route, "method")?;
        validate_identifier(id)?;
        validate_identifier(method)?;
        if !route_ids.insert(id.to_owned()) || !upstream_methods.insert(method.to_owned()) {
            bail!("structured-session route id or method is duplicated");
        }
        if !allowed_effects.contains(&value_string(route, "effect_class")?) {
            bail!("structured-session route has an unknown effect class");
        }
        if route
            .get("audience")
            .and_then(Value::as_str)
            .is_some_and(|audience| !matches!(audience, "public" | "runtime"))
        {
            bail!("structured-session route has an unknown command audience");
        }
        if let Some(binding) = route.get("session_binding") {
            let binding = binding
                .as_object()
                .ok_or_else(|| anyhow!("structured-session session binding must be an object"))?;
            require_keys(
                binding,
                &["action", "request_field", "response_pointer"],
                &[],
            )?;
            let action = value_string(binding, "action")?;
            if !matches!(action, "bind_new" | "bind_expected" | "require") {
                bail!("structured-session route has an unknown session-binding action");
            }
            for field in ["request_field", "response_pointer"] {
                if let Some(value) = binding.get(field).filter(|value| !value.is_null()) {
                    let value = value.as_str().ok_or_else(|| {
                        anyhow!("structured-session binding {field} must be a string or null")
                    })?;
                    if value.is_empty() || value.len() > 256 {
                        bail!("structured-session binding {field} is invalid");
                    }
                }
            }
        }
        let fixed_params = route
            .get("fixed_params")
            .and_then(Value::as_object)
            .filter(|values| values.len() <= 32)
            .ok_or_else(|| anyhow!("structured-session fixed parameters are invalid"))?;
        for (field, value) in fixed_params {
            validate_field_name(field)?;
            validate_bounded_value(value, 0, &mut 0)?;
        }
        validate_string_array(route, "workspace_fields", 8, false)?;
        validate_string_array(route, "forbidden_non_null_fields", 32, false)?;
        validate_predicates(route.get("response_predicates"), 32)?;
        validate_observations(route.get("observations"), 16)?;
        if !matches!(
            value_string(route, "result_retention")?,
            "ephemeral" | "durable"
        ) {
            bail!("structured-session route has an unknown result-retention policy");
        }
        if let Some(ceremony) = route.get("ceremony").filter(|value| !value.is_null())
            && !matches!(ceremony.as_str(), Some("start" | "clear"))
        {
            bail!("structured-session route has an unknown ceremony action");
        }
        schema_ids.insert(value_string(route, "request_schema")?.to_owned());
        schema_ids.insert(value_string(route, "response_schema")?.to_owned());
    }

    let route_sets = object
        .get("route_sets")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("structured-session route sets must be an object"))?;
    if route_sets.is_empty() || route_sets.len() > 16 {
        bail!("structured-session route-set count is outside its bound");
    }
    for (name, routes) in route_sets {
        validate_identifier(name)?;
        let routes = routes
            .as_array()
            .filter(|routes| !routes.is_empty() && routes.len() <= 128)
            .ok_or_else(|| anyhow!("structured-session route set is empty or too large"))?;
        let mut previous: Option<&str> = None;
        for route in routes {
            let route = route
                .as_str()
                .ok_or_else(|| anyhow!("structured-session route-set entry must be a string"))?;
            if !route_ids.contains(route) || previous.is_some_and(|prior| prior >= route) {
                bail!("structured-session route set is not a sorted admitted subset");
            }
            previous = Some(route);
        }
    }

    for step in bounded_array(object, "initialization", 1, 8)? {
        let step = step
            .as_object()
            .ok_or_else(|| anyhow!("structured-session initialization step must be an object"))?;
        require_keys(
            step,
            &[
                "method",
                "effect_class",
                "params",
                "response_schema",
                "notification",
            ],
            &[],
        )?;
        validate_identifier(value_string(step, "method")?)?;
        if value_string(step, "effect_class")? != "pure_read" {
            bail!("structured-session initialization exceeds its fixed pure-read budget");
        }
        if let Some(schema) = step.get("response_schema").and_then(Value::as_str) {
            schema_ids.insert(schema.to_owned());
        } else if !step.get("response_schema").is_some_and(Value::is_null) {
            bail!("structured-session initialization response schema is invalid");
        }
        validate_bounded_value(
            step.get("params").ok_or_else(|| {
                anyhow!("structured-session initialization parameters are absent")
            })?,
            0,
            &mut 0,
        )?;
        if let Some(notification) = step.get("notification").filter(|value| !value.is_null()) {
            validate_identifier(notification.as_str().ok_or_else(|| {
                anyhow!("structured-session initialization notification is invalid")
            })?)?;
        }
    }
    let mut notification_methods = BTreeSet::new();
    for item in bounded_array(object, "notifications", 0, 256)? {
        let item = item
            .as_object()
            .ok_or_else(|| anyhow!("structured-session notification must be an object"))?;
        require_keys(
            item,
            &[
                "method",
                "schema",
                "event_type",
                "durable",
                "payload",
                "observations",
                "ceremony_clear",
            ],
            &[],
        )?;
        let method = value_string(item, "method")?;
        validate_identifier(method)?;
        if !notification_methods.insert(method.to_owned()) {
            bail!("structured-session notification method is duplicated");
        }
        validate_identifier(value_string(item, "event_type")?)?;
        if item.get("durable").and_then(Value::as_bool).is_none()
            || item
                .get("ceremony_clear")
                .and_then(Value::as_bool)
                .is_none()
        {
            bail!("structured-session notification flags are invalid");
        }
        validate_template(item.get("payload"), 0, &mut 0)?;
        validate_observations(item.get("observations"), 16)?;
        schema_ids.insert(value_string(item, "schema")?.to_owned());
    }
    let ignored = object
        .get("ignored_notifications")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("structured-session ignored notifications must be an object"))?;
    if ignored.len() > 256 {
        bail!("structured-session ignored-notification count exceeds its bound");
    }
    for (method, schema) in ignored {
        validate_identifier(method)?;
        if notification_methods.contains(method) {
            bail!("structured-session ignored notification duplicates a mapped notification");
        }
        schema_ids.insert(
            schema
                .as_str()
                .ok_or_else(|| anyhow!("ignored-notification schema must be a string"))?
                .to_owned(),
        );
    }
    let mut server_request_methods = BTreeSet::new();
    for item in bounded_array(object, "server_requests", 0, 32)? {
        let item = item
            .as_object()
            .ok_or_else(|| anyhow!("structured-session server request must be an object"))?;
        require_keys(
            item,
            &[
                "method",
                "schema",
                "operation_class",
                "response_style",
                "deny_only",
                "permission_delta_fields",
                "display",
            ],
            &["required_review_fields"],
        )?;
        let method = value_string(item, "method")?;
        validate_identifier(method)?;
        if !server_request_methods.insert(method.to_owned())
            || notification_methods.contains(method)
            || ignored.contains_key(method)
        {
            bail!("structured-session server-request method is duplicated");
        }
        validate_identifier(value_string(item, "operation_class")?)?;
        if !matches!(
            value_string(item, "response_style")?,
            "decision" | "permissions_denial"
        ) {
            bail!("structured-session server request has an unknown response style");
        }
        if item.get("deny_only").and_then(Value::as_bool).is_none() {
            bail!("structured-session server-request deny-only flag is invalid");
        }
        validate_string_array(item, "permission_delta_fields", 32, true)?;
        if item.contains_key("required_review_fields") {
            validate_string_array(item, "required_review_fields", 32, true)?;
        }
        validate_template(item.get("display"), 0, &mut 0)?;
        schema_ids.insert(value_string(item, "schema")?.to_owned());
    }
    if schema_ids.is_empty() || schema_ids.len() > 512 {
        bail!("structured-session schema set is empty or too large");
    }

    let mut schema_hashes = BTreeMap::new();
    let mut total = 0usize;
    for identity in schema_ids {
        validate_relative_path(&identity)?;
        let bytes = source_files.get(&identity).ok_or_else(|| {
            anyhow!("structured-session schema `{identity}` is absent from the captured source")
        })?;
        if bytes.is_empty() || bytes.len() > MAX_SCHEMA_BYTES {
            bail!("structured-session schema `{identity}` exceeds its byte bound");
        }
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| anyhow!("structured-session schema byte count overflow"))?;
        if total > MAX_SCHEMA_TOTAL_BYTES {
            bail!("structured-session schemas exceed their aggregate byte ceiling");
        }
        let schema: Value = serde_json::from_slice(bytes)
            .with_context(|| format!("decode structured-session schema `{identity}`"))?;
        reject_nonlocal_refs(&schema, 0)?;
        jsonschema::validator_for(&schema)
            .map_err(|error| anyhow!("compile structured-session schema `{identity}`: {error}"))?;
        schema_hashes.insert(identity, lillux::sha256_hex(bytes));
    }
    let baseline_source = value_string(object, "baseline_config")?.to_owned();
    let baseline_destination = value_string(object, "baseline_destination")?.to_owned();
    let admitted = AdmittedStructuredSessionProfile {
        profile_hash: ryeos_state::objects::canonical_value_digest(&profile)?,
        contract: profile,
        schema_hashes,
        baseline_source,
        baseline_destination,
    };
    admitted.validate()?;
    Ok(admitted)
}

fn value_string<'a>(object: &'a serde_json::Map<String, Value>, key: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("structured-session `{key}` must be a string"))
}

fn require_keys(
    object: &serde_json::Map<String, Value>,
    required: &[&str],
    optional: &[&str],
) -> Result<()> {
    if required.iter().any(|key| !object.contains_key(*key))
        || object
            .keys()
            .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        bail!("structured-session mapping has an unknown or missing field");
    }
    Ok(())
}

fn bounded_array<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
    minimum: usize,
    maximum: usize,
) -> Result<&'a Vec<Value>> {
    object
        .get(key)
        .and_then(Value::as_array)
        .filter(|values| values.len() >= minimum && values.len() <= maximum)
        .ok_or_else(|| anyhow!("structured-session `{key}` count is outside its bound"))
}

fn validate_field_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || value.chars().any(char::is_control)
        || value.contains('/')
    {
        bail!("structured-session field name is not bounded portable text");
    }
    Ok(())
}

fn validate_pointer(value: &str) -> Result<()> {
    if value.len() > 1024
        || (!value.is_empty() && !value.starts_with('/'))
        || value.chars().any(char::is_control)
    {
        bail!("structured-session JSON pointer is invalid");
    }
    Ok(())
}

fn validate_string_array(
    object: &serde_json::Map<String, Value>,
    key: &str,
    maximum: usize,
    pointers: bool,
) -> Result<()> {
    let values = object
        .get(key)
        .and_then(Value::as_array)
        .filter(|values| values.len() <= maximum)
        .ok_or_else(|| anyhow!("structured-session `{key}` is not a bounded array"))?;
    let mut seen = BTreeSet::new();
    for value in values {
        let value = value
            .as_str()
            .ok_or_else(|| anyhow!("structured-session `{key}` entry must be a string"))?;
        if pointers {
            validate_pointer(value)?;
        } else {
            validate_identifier(value)?;
        }
        if !seen.insert(value) {
            bail!("structured-session `{key}` contains a duplicate");
        }
    }
    Ok(())
}

fn validate_bounded_value(value: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    if depth > 32 {
        bail!("structured-session authored value exceeds its nesting bound");
    }
    *nodes = nodes
        .checked_add(1)
        .ok_or_else(|| anyhow!("structured-session authored value node count overflow"))?;
    if *nodes > 4096 {
        bail!("structured-session authored value exceeds its node bound");
    }
    match value {
        Value::String(value) if value.len() > 64 * 1024 => {
            bail!("structured-session authored string exceeds its byte bound")
        }
        Value::Array(values) => {
            if values.len() > 1024 {
                bail!("structured-session authored array exceeds its element bound");
            }
            for value in values {
                validate_bounded_value(value, depth + 1, nodes)?;
            }
        }
        Value::Object(values) => {
            if values.len() > 1024 {
                bail!("structured-session authored object exceeds its field bound");
            }
            for (key, value) in values {
                validate_field_name(key)?;
                validate_bounded_value(value, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_predicates(value: Option<&Value>, maximum: usize) -> Result<()> {
    let predicates = value
        .and_then(Value::as_array)
        .filter(|values| values.len() <= maximum)
        .ok_or_else(|| anyhow!("structured-session predicates are not a bounded array"))?;
    for predicate in predicates {
        let predicate = predicate
            .as_object()
            .ok_or_else(|| anyhow!("structured-session predicate must be an object"))?;
        require_keys(predicate, &["pointer", "equals"], &[])?;
        validate_pointer(value_string(predicate, "pointer")?)?;
        validate_bounded_value(
            predicate
                .get("equals")
                .ok_or_else(|| anyhow!("structured-session predicate value is absent"))?,
            0,
            &mut 0,
        )?;
    }
    Ok(())
}

fn validate_observations(value: Option<&Value>, maximum: usize) -> Result<()> {
    let observations = value
        .and_then(Value::as_array)
        .filter(|values| values.len() <= maximum)
        .ok_or_else(|| anyhow!("structured-session observations are not a bounded array"))?;
    for observation in observations {
        let observation = observation
            .as_object()
            .ok_or_else(|| anyhow!("structured-session observation must be an object"))?;
        require_keys(observation, &["when", "value"], &[])?;
        validate_predicates(observation.get("when"), 16)?;
        validate_template(observation.get("value"), 0, &mut 0)?;
    }
    Ok(())
}

fn validate_template(value: Option<&Value>, depth: usize, nodes: &mut usize) -> Result<()> {
    if depth > 32 {
        bail!("structured-session value template exceeds its nesting bound");
    }
    *nodes = nodes
        .checked_add(1)
        .ok_or_else(|| anyhow!("structured-session template node count overflow"))?;
    if *nodes > 2048 {
        bail!("structured-session value template exceeds its node bound");
    }
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("structured-session value template must be an object"))?;
    match value_string(object, "op")? {
        "literal" => {
            require_keys(object, &["op", "value"], &[])?;
            validate_bounded_value(
                object
                    .get("value")
                    .ok_or_else(|| anyhow!("structured-session literal value is absent"))?,
                0,
                &mut 0,
            )?;
        }
        "pointer" => {
            require_keys(
                object,
                &["op", "pointer"],
                &["optional", "max_string_bytes"],
            )?;
            validate_pointer(value_string(object, "pointer")?)?;
            if object
                .get("optional")
                .is_some_and(|value| !value.is_boolean())
            {
                bail!("structured-session pointer optional flag is invalid");
            }
            if let Some(limit) = object.get("max_string_bytes") {
                let limit = limit
                    .as_u64()
                    .filter(|limit| *limit > 0 && *limit <= 1024 * 1024)
                    .ok_or_else(|| anyhow!("structured-session pointer byte bound is invalid"))?;
                let _ = limit;
            }
        }
        "object" => {
            require_keys(object, &["op", "fields"], &[])?;
            let fields = object
                .get("fields")
                .and_then(Value::as_object)
                .filter(|fields| fields.len() <= 256)
                .ok_or_else(|| anyhow!("structured-session template fields are invalid"))?;
            for (field, value) in fields {
                validate_field_name(field)?;
                validate_template(Some(value), depth + 1, nodes)?;
            }
        }
        "array" => {
            require_keys(object, &["op", "values"], &[])?;
            let values = object
                .get("values")
                .and_then(Value::as_array)
                .filter(|values| values.len() <= 256)
                .ok_or_else(|| anyhow!("structured-session template array is invalid"))?;
            for value in values {
                validate_template(Some(value), depth + 1, nodes)?;
            }
        }
        "digest" => {
            require_keys(object, &["op", "pointer"], &[])?;
            validate_pointer(value_string(object, "pointer")?)?;
        }
        _ => bail!("structured-session value template operation is not admitted"),
    }
    Ok(())
}

fn validate_identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        bail!("structured-session identifier is not bounded and portable");
    }
    Ok(())
}

fn validate_file_name(value: &str) -> Result<()> {
    let path = Path::new(value);
    if path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        bail!("structured-session file identity must be one relative name");
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.len() > 4096
        || path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("structured-session schema identity is not a safe local path");
    }
    Ok(())
}

fn reject_nonlocal_refs(value: &Value, depth: usize) -> Result<()> {
    if depth > 128 {
        bail!("structured-session schema exceeds its nesting bound");
    }
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                && !reference.starts_with("#/")
            {
                bail!("structured-session schema contains a non-local reference");
            }
            for nested in object.values() {
                reject_nonlocal_refs(nested, depth + 1)?;
            }
        }
        Value::Array(values) => {
            for nested in values {
                reject_nonlocal_refs(nested, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture_profile(route_id: &str, method: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version":1,
            "workload_realization_id":"fixture-runtime",
            "workload_executable":"fixture-worker",
            "workload_args":[],
            "workload_home_env":"FIXTURE_HOME",
            "baseline_config":"baseline.conf",
            "baseline_destination":"runtime.conf",
            "initialization":[{
                "method":"initialize",
                "effect_class":"pure_read",
                "params":{},
                "response_schema":"schema/response.json",
                "notification":null
            }],
            "route_sets":{"default":[route_id]},
            "routes":[{
                "id":route_id,
                "method":method,
                "effect_class":"pure_read",
                "request_schema":"schema/request.json",
                "response_schema":"schema/response.json",
                "fixed_params":{},
                "workspace_fields":[],
                "forbidden_non_null_fields":[],
                "response_predicates":[],
                "observations":[],
                "result_retention":"ephemeral",
                "ceremony":null
            }],
            "notifications":[],
            "ignored_notifications":{},
            "server_requests":[]
        }))
        .unwrap()
    }

    fn schemas() -> BTreeMap<String, Vec<u8>> {
        BTreeMap::from([
            (
                "schema/request.json".to_owned(),
                serde_json::to_vec(&json!({"type":"object","additionalProperties":false})).unwrap(),
            ),
            (
                "schema/response.json".to_owned(),
                serde_json::to_vec(&json!({})).unwrap(),
            ),
        ])
    }

    #[test]
    fn two_unrelated_profiles_compile_without_provider_code() {
        let first = compile(
            &fixture_profile("document.inspect", "document/read"),
            &schemas(),
        )
        .unwrap();
        let second = compile(&fixture_profile("job.status", "job/status"), &schemas()).unwrap();
        assert_ne!(first.profile_hash, second.profile_hash);
        assert_eq!(first.schema_hashes, second.schema_hashes);
        assert_eq!(
            first.contract.get("schema_version").and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn remote_schema_reference_fails_at_admission() {
        let mut files = schemas();
        files.insert(
            "schema/request.json".to_owned(),
            serde_json::to_vec(&json!({"$ref":"https://invalid.example/schema"})).unwrap(),
        );
        assert!(compile(&fixture_profile("job.status", "job/status"), &files).is_err());
    }

    #[test]
    fn malformed_mapping_fails_before_worker_launch() {
        let mut profile: Value =
            serde_json::from_slice(&fixture_profile("document.inspect", "document/read")).unwrap();
        profile["routes"][0]["observations"] = json!([{
            "when": [],
            "value": {"op":"execute_arbitrary_code","source":"oops"}
        }]);
        assert!(compile(&serde_json::to_vec(&profile).unwrap(), &schemas()).is_err());

        profile["routes"][0]["observations"] = json!([]);
        profile["routes"][0]["response_predicates"] =
            json!([{"pointer":"not-a-json-pointer","equals":true}]);
        assert!(compile(&serde_json::to_vec(&profile).unwrap(), &schemas()).is_err());
    }

    #[test]
    fn shipped_codex_contract_is_fully_admitted_before_launch() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles/codex/.ai/workers/codex/lib/hosted");
        let profile = std::fs::read(root.join("structured-session.profile.json")).unwrap();
        let mut files = BTreeMap::new();
        for entry in std::fs::read_dir(root.join("schema")).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                files.insert(
                    format!("schema/{}", entry.file_name().to_string_lossy()),
                    std::fs::read(entry.path()).unwrap(),
                );
            }
        }
        let admitted = compile(&profile, &files).unwrap();
        assert_eq!(
            admitted
                .contract
                .get("schema_version")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert!(admitted.schema_hashes.len() > 10);
        let args = admitted.contract["workload_args"].as_array().unwrap();
        for required in [
            "--strict-config",
            "default_permissions=\"ryeos-workspace-only\"",
            "approvals_reviewer=\"user\"",
        ] {
            assert!(args.iter().any(|value| value.as_str() == Some(required)));
        }
    }
}
