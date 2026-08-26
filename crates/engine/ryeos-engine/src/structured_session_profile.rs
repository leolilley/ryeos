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
        "configuration_authority",
        "workload_realization_id",
        "workload_executable",
        "workload_args",
        "workload_home_env",
        "baseline_config",
        "baseline_destination",
        "initialization",
        "recovery",
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
    if object
        .get("configuration_authority")
        .and_then(Value::as_str)
        != Some("immutable_argv")
    {
        bail!("structured-session configuration authority is not immutable argv");
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
            &[
                "audience",
                "session_binding",
                "forbidden_fields",
                "post_success_routes",
            ],
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
        if let Some(audience) = route.get("audience") {
            let audience = audience.as_str().ok_or_else(|| {
                anyhow!("structured-session route audience must be a string when present")
            })?;
            if !matches!(audience, "public" | "runtime") {
                bail!("structured-session route has an unknown command audience");
            }
        }
        let mut controlled_fields = BTreeSet::new();
        let mut binding_request_field: Option<&str> = None;
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
            let request_field = binding
                .get("request_field")
                .filter(|value| !value.is_null())
                .map(|value| {
                    value.as_str().ok_or_else(|| {
                        anyhow!("structured-session binding request_field must be a string or null")
                    })
                })
                .transpose()?;
            let response_pointer = binding
                .get("response_pointer")
                .filter(|value| !value.is_null())
                .map(|value| {
                    value.as_str().ok_or_else(|| {
                        anyhow!(
                            "structured-session binding response_pointer must be a string or null"
                        )
                    })
                })
                .transpose()?;
            match action {
                "bind_new" if request_field.is_none() && response_pointer.is_some() => {}
                "bind_expected" if request_field.is_some() && response_pointer.is_some() => {}
                "require" if request_field.is_some() && response_pointer.is_none() => {}
                _ => bail!("structured-session binding fields contradict its action"),
            }
            if let Some(field) = request_field {
                validate_field_name(field)?;
                binding_request_field = Some(field);
            }
            if let Some(pointer) = response_pointer {
                validate_pointer(pointer)?;
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
            controlled_fields.insert(field.as_str());
        }
        validate_string_array(route, "workspace_fields", 8, false)?;
        validate_string_array(route, "forbidden_non_null_fields", 32, false)?;
        if route.contains_key("forbidden_fields") {
            validate_string_array(route, "forbidden_fields", 32, false)?;
        }
        if route.contains_key("post_success_routes") {
            validate_string_array(route, "post_success_routes", 8, false)?;
        }
        for field in [
            "workspace_fields",
            "forbidden_non_null_fields",
            "forbidden_fields",
        ] {
            for value in route
                .get(field)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !controlled_fields.insert(value) {
                    bail!("structured-session route field policies overlap");
                }
            }
        }
        if binding_request_field.is_some_and(|field| controlled_fields.contains(field)) {
            bail!("structured-session binding field overlaps another route field policy");
        }
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
    for route in routes {
        let route = route
            .as_object()
            .expect("route objects were validated above");
        let route_id = value_string(route, "id")?;
        let post_routes = route
            .get("post_success_routes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|value| {
                value.as_str().ok_or_else(|| {
                    anyhow!("structured-session post-success route must be a string")
                })
            })
            .collect::<Result<Vec<_>>>()?;
        for post_route_id in &post_routes {
            if *post_route_id == route_id {
                bail!("structured-session post-success route graph contains a self-cycle");
            }
            let post_route = routes
                .iter()
                .filter_map(Value::as_object)
                .find(|candidate| {
                    candidate.get("id").and_then(Value::as_str) == Some(*post_route_id)
                })
                .ok_or_else(|| anyhow!("structured-session post-success route is absent"))?;
            let post_binding = post_route.get("session_binding").and_then(Value::as_object);
            if post_route.get("audience").and_then(Value::as_str) != Some("runtime")
                || post_binding
                    .and_then(|binding| binding.get("action"))
                    .and_then(Value::as_str)
                    != Some("require")
                || post_route
                    .get("post_success_routes")
                    .and_then(Value::as_array)
                    .is_some_and(|routes| !routes.is_empty())
                || post_route
                    .get("observations")
                    .and_then(Value::as_array)
                    .is_none_or(|observations| !observations.is_empty())
                || post_route
                    .get("ceremony")
                    .is_some_and(|value| !value.is_null())
                || post_route.get("result_retention").and_then(Value::as_str) != Some("ephemeral")
            {
                bail!(
                    "structured-session post-success route is not an inert runtime-only binding operation"
                );
            }
        }
        for selected in route_sets.values().filter_map(Value::as_array) {
            if selected
                .iter()
                .any(|candidate| candidate.as_str() == Some(route_id))
                && post_routes.iter().any(|post_route| {
                    !selected
                        .iter()
                        .any(|candidate| candidate.as_str() == Some(*post_route))
                })
            {
                bail!("structured-session post-success route escapes its source route set");
            }
        }
    }

    if let Some(recovery) = object.get("recovery").filter(|value| !value.is_null()) {
        let recovery = recovery.as_object().ok_or_else(|| {
            anyhow!("structured-session recovery contract must be an object or null")
        })?;
        require_keys(
            recovery,
            &["resume_route", "inspect_route", "route_sets"],
            &[],
        )?;
        let resume_route = value_string(recovery, "resume_route")?;
        let inspect_route = value_string(recovery, "inspect_route")?;
        validate_identifier(resume_route)?;
        validate_identifier(inspect_route)?;
        if resume_route == inspect_route {
            bail!("structured-session recovery routes must be distinct");
        }
        let recovery_route_sets = recovery
            .get("route_sets")
            .and_then(Value::as_array)
            .filter(|sets| !sets.is_empty() && sets.len() <= 16)
            .ok_or_else(|| anyhow!("structured-session recovery route sets are invalid"))?;
        let mut previous: Option<&str> = None;
        for route_set in recovery_route_sets {
            let route_set = route_set.as_str().ok_or_else(|| {
                anyhow!("structured-session recovery route-set entry must be a string")
            })?;
            let selected = route_sets
                .get(route_set)
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("structured-session recovery names an unknown route set"))?;
            if previous.is_some_and(|prior| prior >= route_set)
                || !selected
                    .iter()
                    .any(|route| route.as_str() == Some(resume_route))
                || !selected
                    .iter()
                    .any(|route| route.as_str() == Some(inspect_route))
            {
                bail!("structured-session recovery route sets are not a sorted admitted subset");
            }
            previous = Some(route_set);
        }
        for (route_id, binding_action) in
            [(resume_route, "bind_expected"), (inspect_route, "require")]
        {
            let route = routes
                .iter()
                .filter_map(Value::as_object)
                .find(|route| route.get("id").and_then(Value::as_str) == Some(route_id))
                .ok_or_else(|| anyhow!("structured-session recovery route is absent"))?;
            if route.get("audience").and_then(Value::as_str) != Some("runtime")
                || route
                    .get("session_binding")
                    .and_then(Value::as_object)
                    .and_then(|binding| binding.get("action"))
                    .and_then(Value::as_str)
                    != Some(binding_action)
            {
                bail!(
                    "structured-session recovery route `{route_id}` has the wrong audience or binding"
                );
            }
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
        let response_schema = step.get("response_schema").filter(|value| !value.is_null());
        let notification = step.get("notification").filter(|value| !value.is_null());
        if response_schema.is_some() == notification.is_some() {
            bail!(
                "structured-session initialization must select exactly one response or notification"
            );
        }
        if let Some(schema) = response_schema.and_then(Value::as_str) {
            schema_ids.insert(schema.to_owned());
        } else if response_schema.is_some() {
            bail!("structured-session initialization response schema is invalid");
        }
        validate_bounded_value(
            step.get("params").ok_or_else(|| {
                anyhow!("structured-session initialization parameters are absent")
            })?,
            0,
            &mut 0,
        )?;
        if let Some(notification) = notification {
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
    if ignored.len() > 256
        || object
            .get("notifications")
            .and_then(Value::as_array)
            .is_some_and(|notifications| notifications.len() + ignored.len() > 256)
    {
        bail!("structured-session notification count exceeds its aggregate bound");
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
                "correlation",
                "responses",
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
        let correlation = item
            .get("correlation")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("structured-session server-request correlation is invalid"))?;
        require_keys(
            correlation,
            &["upstream_session_pointer", "operation_pointer"],
            &[],
        )?;
        validate_pointer(value_string(correlation, "upstream_session_pointer")?)?;
        validate_pointer(value_string(correlation, "operation_pointer")?)?;
        if item.get("deny_only").and_then(Value::as_bool).is_none() {
            bail!("structured-session server-request deny-only flag is invalid");
        }
        validate_string_array(item, "permission_delta_fields", 32, true)?;
        if item.contains_key("required_review_fields") {
            validate_string_array(item, "required_review_fields", 32, true)?;
        }
        validate_template(item.get("display"), 0, &mut 0)?;
        let responses = item
            .get("responses")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("structured-session server-request responses are invalid"))?;
        const RESPONSE_KEYS: [&str; 4] = ["accept", "cancel", "decline", "expire"];
        if responses.len() != RESPONSE_KEYS.len()
            || RESPONSE_KEYS
                .iter()
                .any(|key| !responses.contains_key(*key))
        {
            bail!("structured-session server-request responses are incomplete");
        }
        for response in responses.values() {
            validate_template(Some(response), 0, &mut 0)?;
        }
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
    let baseline = source_files
        .get(&baseline_source)
        .ok_or_else(|| anyhow!("structured-session baseline is absent from the captured source"))?;
    if baseline.is_empty() || baseline.len() > MAX_SCHEMA_BYTES {
        bail!("structured-session baseline exceeds its byte bound");
    }
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
            "configuration_authority":"immutable_argv",
            "initialization":[{
                "method":"initialize",
                "effect_class":"pure_read",
                "params":{},
                "response_schema":"schema/response.json",
                "notification":null
            }],
            "recovery":null,
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
            ("baseline.conf".to_owned(), b"fixture=true\n".to_vec()),
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

        let mut profile: Value =
            serde_json::from_slice(&fixture_profile("document.inspect", "document/read")).unwrap();
        profile["routes"][0]["session_binding"] = json!({
            "action":"require",
            "request_field":null,
            "response_pointer":null
        });
        assert!(compile(&serde_json::to_vec(&profile).unwrap(), &schemas()).is_err());

        let mut profile: Value =
            serde_json::from_slice(&fixture_profile("document.inspect", "document/read")).unwrap();
        profile["routes"][0]["fixed_params"] = json!({"workspace":"fixed"});
        profile["routes"][0]["workspace_fields"] = json!(["workspace"]);
        assert!(compile(&serde_json::to_vec(&profile).unwrap(), &schemas()).is_err());

        let mut profile: Value =
            serde_json::from_slice(&fixture_profile("document.inspect", "document/read")).unwrap();
        profile["initialization"][0]["notification"] = json!("initialized");
        assert!(compile(&serde_json::to_vec(&profile).unwrap(), &schemas()).is_err());
    }

    #[test]
    fn post_success_route_is_frozen_as_inert_runtime_policy() {
        let mut profile: Value =
            serde_json::from_slice(&fixture_profile("session.start", "thread/start")).unwrap();
        let mut persist = profile["routes"][0].clone();
        persist["id"] = json!("session.persist");
        persist["method"] = json!("thread/name/set");
        persist["audience"] = json!("runtime");
        persist["session_binding"] = json!({
            "action":"require",
            "request_field":"threadId",
            "response_pointer":null
        });
        profile["routes"][0]["post_success_routes"] = json!(["session.persist"]);
        profile["routes"].as_array_mut().unwrap().push(persist);
        profile["route_sets"]["default"] = json!(["session.persist", "session.start"]);

        compile(&serde_json::to_vec(&profile).unwrap(), &schemas())
            .expect("an inert runtime-only post-success route must be admitted");

        profile["routes"][1]["audience"] = json!("public");
        let error = compile(&serde_json::to_vec(&profile).unwrap(), &schemas()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("post-success route is not an inert runtime-only binding operation")
        );
    }

    #[test]
    fn aggregate_notification_bound_fails_during_admission() {
        let mut profile: Value =
            serde_json::from_slice(&fixture_profile("job.status", "job/status")).unwrap();
        profile["notifications"] = Value::Array(
            (0..129)
                .map(|index| {
                    json!({
                        "method":format!("event/{index}"),
                        "schema":"schema/response.json",
                        "event_type":format!("event.{index}"),
                        "durable":false,
                        "payload":{"op":"literal","value":null},
                        "observations":[],
                        "ceremony_clear":false
                    })
                })
                .collect(),
        );
        profile["ignored_notifications"] = Value::Object(
            (0..128)
                .map(|index| {
                    (
                        format!("ignored/{index}"),
                        Value::String("schema/response.json".to_owned()),
                    )
                })
                .collect(),
        );

        let error = compile(&serde_json::to_vec(&profile).unwrap(), &schemas()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("notification count exceeds its aggregate bound")
        );
    }
}
