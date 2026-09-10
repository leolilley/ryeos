//! Admission compiler for the closed structured-session protocol family.
//!
//! A profile is authority-bearing executable policy.  This compiler runs
//! while the worker source closure is being admitted, before any process is
//! launched. It accepts only the current closed vocabulary and exact local schema
//! blobs captured in the same signed source closure.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use ryeos_state::objects::AdmittedStructuredSessionProfile;

const MAX_PROFILE_BYTES: usize = 64 * 1024;
const MAX_SCHEMA_BYTES: usize = 8 * 1024 * 1024;
const MAX_SCHEMA_TOTAL_BYTES: usize = 16 * 1024 * 1024;
pub const STRUCTURED_SESSION_PROFILE_SCHEMA_VERSION: u32 = 4;

/// The closed workload transport vocabulary. The admission compiler and the
/// bridge must accept exactly this set; adding a transport is a schema
/// version decision, never a per-profile discovery.
pub const STRUCTURED_SESSION_TRANSPORTS: [&str; 2] = ["stdio_jsonrpc", "http_sse"];

/// Provider-neutral ingress mapping compiled as part of the signed protocol
/// profile. It names wire fields, never a Tool implementation or live grant.
/// Keep this type shared with the bridge; do not add a second runtime parser
/// that accepts deeper or different authority than source admission.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredSessionInvocationMapping {
    pub registration_route: String,
    pub registration_field: String,
    pub registration: Value,
    pub method: String,
    pub request_schema: String,
    pub response_schema: String,
    pub session_pointer: String,
    pub operation_pointer: String,
    pub call_pointer: String,
    pub required_values: BTreeMap<String, Value>,
    pub request: Value,
    pub response: Value,
}

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
    let mut required = [
        "schema_version",
        "configuration_authority",
        "workload_realization_id",
        "workload_executable",
        "workload_args",
        "workload_home_env",
        "required_process_environment",
        "workload_client",
        "baseline_config",
        "baseline_destination",
        "portable_state",
        "credential_subject",
        "initialization",
        "recovery",
        "route_sets",
        "routes",
        "notifications",
        "ignored_notifications",
        "server_requests",
    ]
    .to_vec();
    if object.get("schema_version").and_then(Value::as_u64)
        != Some(u64::from(STRUCTURED_SESSION_PROFILE_SCHEMA_VERSION))
    {
        bail!("structured-session profile schema is not admitted");
    }
    required.push("transport");
    let http_transport = match object.get("transport").and_then(Value::as_str) {
        Some(transport @ "http_sse") => {
            if !STRUCTURED_SESSION_TRANSPORTS.contains(&transport) {
                bail!("structured-session transport is not admitted");
            }
            true
        }
        Some("stdio_jsonrpc") => false,
        _ => bail!("structured-session transport is not admitted"),
    };
    required.push("http_sse");
    let mut http_credentials: Vec<&str> = Vec::new();
    match (http_transport, object.get("http_sse")) {
        (true, Some(credentials)) => {
            let credentials = credentials.as_object().ok_or_else(|| {
                anyhow!("structured-session HTTP credential environment block is invalid")
            })?;
            require_keys(credentials, &["username_env", "password_env"], &[])?;
            for key in ["username_env", "password_env"] {
                let name = value_string(credentials, key)?;
                crate::protocol_vocabulary::validate_env_name(name)
                    .map_err(|error| anyhow!(error))?;
                if http_credentials.contains(&name) {
                    bail!("structured-session HTTP credential environments are duplicated");
                }
                http_credentials.push(name);
            }
        }
        (true, None) => {
            bail!("structured-session HTTP transport lacks its credential environment block")
        }
        (false, Some(Value::Null)) => {}
        (false, Some(_)) => {
            bail!("structured-session HTTP credential block requires the HTTP transport");
        }
        (false, None) => {
            bail!("structured-session HTTP credential block must be present and nullable");
        }
    }
    if object.len() != required.len() || required.iter().any(|key| !object.contains_key(*key)) {
        bail!("structured-session profile has an unknown or missing top-level field");
    }
    let home_env = value_string(object, "workload_home_env")?;
    let mut admitted_environment: Vec<&str> = vec![home_env, "LANG", "LC_ALL", "HOME", "PATH"];
    for name in bounded_array(object, "required_process_environment", 0, 64)? {
        let name = name
            .as_str()
            .ok_or_else(|| anyhow!("required process environment name is not text"))?;
        admitted_environment.push(name);
    }
    for name in &http_credentials {
        if admitted_environment.contains(name) {
            bail!("structured-session HTTP credential environment collides with admitted environment");
        }
    }
    if http_transport && !object.get("initialization").and_then(Value::as_array).is_some_and(|steps| steps.is_empty()) {
        bail!("structured-session HTTP transport admits no initialization handshake");
    }
    if object
        .get("configuration_authority")
        .and_then(Value::as_str)
        != Some("immutable_argv")
    {
        bail!("structured-session configuration authority is not immutable argv");
    }
    validate_identifier(value_string(object, "workload_realization_id")?)?;
    validate_workload_executable_member(value_string(object, "workload_executable")?)?;
    validate_file_name(value_string(object, "baseline_config")?)?;
    validate_file_name(value_string(object, "baseline_destination")?)?;
    crate::protocol_vocabulary::validate_env_name(value_string(object, "workload_home_env")?)
        .map_err(|error| anyhow!(error))?;
    let mut previous_environment = None;
    for name in bounded_array(object, "required_process_environment", 0, 64)? {
        let name = name
            .as_str()
            .ok_or_else(|| anyhow!("required process environment name is not text"))?;
        crate::protocol_vocabulary::validate_env_name(name).map_err(|error| anyhow!(error))?;
        if previous_environment.is_some_and(|previous| previous >= name) {
            bail!("required process environment names must be sorted and unique");
        }
        previous_environment = Some(name);
    }
    match object.get("workload_client") {
        Some(Value::Null) => {}
        Some(Value::Object(workload_client)) => {
            require_keys(
                workload_client,
                &["cli_endpoint_env", "structured_session"],
                &[],
            )?;
            if !workload_client["cli_endpoint_env"].is_null()
                && workload_client["cli_endpoint_env"].as_str()
                    != Some(crate::protocol_vocabulary::WORKLOAD_CLIENT_ENDPOINT_ENV)
            {
                bail!("structured-session CLI endpoint environment is not current");
            }
            if workload_client.values().all(Value::is_null) {
                bail!("structured-session workload client declares no ingress");
            }
        }
        _ => bail!("structured-session workload_client must be present and nullable"),
    }
    if let Some(portable_state) = object
        .get("portable_state")
        .filter(|value| !value.is_null())
    {
        let contract: ryeos_state::objects::PortableSessionStateContract =
            serde_json::from_value(portable_state.clone())
                .context("decode structured-session portable-state contract")?;
        contract.validate()?;
    }
    if let Some(credential_subject) = object
        .get("credential_subject")
        .filter(|value| !value.is_null())
    {
        let contract: ryeos_state::objects::CredentialSubjectProjectionContract =
            serde_json::from_value(credential_subject.clone())
                .context("decode structured-session credential-subject contract")?;
        contract.validate()?;
    }
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
                "progress_notifications",
                "http_method",
                "http_path",
            ],
        )?;
        let id = value_string(route, "id")?;
        let method = value_string(route, "method")?;
        validate_identifier(id)?;
        validate_identifier(method)?;
        if !route_ids.insert(id.to_owned()) || !upstream_methods.insert(method.to_owned()) {
            bail!("structured-session route id or method is duplicated");
        }
        let binding_action = route
            .get("session_binding")
            .and_then(Value::as_object)
            .and_then(|binding| binding.get("action"))
            .and_then(Value::as_str);
        validate_route_transport_addressing(route, http_transport, binding_action)?;
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
        if route.contains_key("progress_notifications") {
            validate_string_array(route, "progress_notifications", 1, false)?;
            for method in route["progress_notifications"].as_array().unwrap() {
                if route["session_binding"]["action"] != "require" {
                    bail!("command progress requires an already-bound upstream session");
                }
                let notification = object
                    .get("notifications")
                    .and_then(Value::as_array)
                    .and_then(|notifications| {
                        notifications
                            .iter()
                            .find(|notification| notification["method"] == *method)
                    })
                    .ok_or_else(|| anyhow!("command progress names an undeclared notification"))?;
                if notification["durable"] != true
                    || !notification["upstream_session_pointer"].is_string()
                    || notification["observations"]
                        .as_array()
                        .is_none_or(|values| values.len() != 1)
                {
                    bail!("command progress must select one durable lifecycle observation");
                }
                validate_progress_observation(
                    &notification["observations"][0],
                    "/message/params/",
                )?;
                if route["observations"]
                    .as_array()
                    .is_none_or(|values| values.len() != 1)
                {
                    bail!("progress route must corroborate one final lifecycle start");
                }
                validate_progress_observation(&route["observations"][0], "/response/result/")?;
            }
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

    for step in bounded_array(object, "initialization", usize::from(!http_transport), 8)? {
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
            &["upstream_session_pointer"],
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
        if let Some(pointer) = item.get("upstream_session_pointer") {
            validate_pointer(
                pointer
                    .as_str()
                    .ok_or_else(|| anyhow!("notification session pointer must be a string"))?,
            )?;
        }
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
    if let Some(invocation) = object["workload_client"]
        .get("structured_session")
        .filter(|value| !value.is_null())
    {
        let mapping: StructuredSessionInvocationMapping =
            serde_json::from_value(invocation.clone())
                .context("compile structured-session invocation mapping")?;
        validate_identifier(&mapping.method)?;
        validate_identifier(&mapping.registration_route)?;
        validate_field_name(&mapping.registration_field)?;
        if notification_methods.contains(&mapping.method) || ignored.contains_key(&mapping.method) {
            bail!("structured-session invocation method collides with a notification");
        }
        server_request_methods.insert(mapping.method.clone());
        let route = routes
            .iter()
            .find(|route| route["id"].as_str() == Some(mapping.registration_route.as_str()))
            .ok_or_else(|| anyhow!("invocation registration route is not admitted"))?;
        if route["session_binding"]["action"].as_str() != Some("bind_new")
            || !route["forbidden_fields"].as_array().is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| field.as_str() == Some(mapping.registration_field.as_str()))
            })
            || route["fixed_params"]
                .get(&mapping.registration_field)
                .is_some()
            || route["workspace_fields"].as_array().is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| field.as_str() == Some(mapping.registration_field.as_str()))
            })
        {
            bail!("invocation registration must exclusively own a caller-forbidden bind-new field");
        }
        let pointers = [
            &mapping.session_pointer,
            &mapping.operation_pointer,
            &mapping.call_pointer,
        ];
        let mut distinct = BTreeSet::new();
        for pointer in pointers {
            validate_pointer(pointer)?;
            if !pointer.starts_with("/message/params/") || !distinct.insert(pointer) {
                bail!("invocation correlations must be distinct request parameter pointers");
            }
        }
        if mapping.required_values.is_empty() || mapping.required_values.len() > 16 {
            bail!("invocation mapping requires bounded wire identity predicates");
        }
        for (pointer, value) in &mapping.required_values {
            validate_pointer(pointer)?;
            validate_bounded_value(value, 0, &mut 0)?;
        }
        for template in [&mapping.registration, &mapping.request, &mapping.response] {
            validate_template(Some(template), 0, &mut 0)?;
        }
        schema_ids.insert(mapping.request_schema);
        schema_ids.insert(mapping.response_schema);
    }
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
            &["required_review_fields", "reply_http_path"],
        )?;
        match (http_transport, item.get("reply_http_path")) {
            (true, Some(reply)) => validate_http_path(
                reply.as_str().ok_or_else(|| {
                    anyhow!("structured-session reply HTTP path must be a string")
                })?,
                &["request_id"],
            )?,
            (true, None) => {
                bail!("structured-session HTTP server request lacks its reply path")
            }
            (false, Some(_)) => {
                bail!("structured-session reply HTTP path requires the HTTP transport")
            }
            (false, None) => {}
        }
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
    if let Some(mapping) = object["workload_client"]
        .get("structured_session")
        .filter(|value| !value.is_null())
    {
        let mapping: StructuredSessionInvocationMapping = serde_json::from_value(mapping.clone())?;
        validate_invocation_projection(&mapping, routes, source_files)?;
    }
    for notification in object["notifications"].as_array().unwrap() {
        if let Some(pointer) = notification.get("upstream_session_pointer") {
            let schema: Value =
                serde_json::from_slice(&source_files[notification["schema"].as_str().unwrap()])?;
            let field = invocation_parameter_schema(&schema, pointer.as_str().unwrap())?;
            if resolve_invocation_schema(&schema, field)?["type"] != "string" {
                bail!("notification session correlation must select a required string schema");
            }
        }
    }
    for route in routes {
        for method in route
            .get("progress_notifications")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let notification = object["notifications"]
                .as_array()
                .unwrap()
                .iter()
                .find(|notification| notification["method"] == *method)
                .ok_or_else(|| anyhow!("progress notification disappeared"))?;
            for (name, pointer, prefix) in [
                (
                    &notification["schema"],
                    &notification["observations"][0]["value"]["fields"]["turn_id"]["pointer"],
                    "/message/params/",
                ),
                (
                    &route["response_schema"],
                    &route["observations"][0]["value"]["fields"]["turn_id"]["pointer"],
                    "/response/result/",
                ),
            ] {
                let schema: Value = serde_json::from_slice(
                    source_files
                        .get(
                            name.as_str()
                                .ok_or_else(|| anyhow!("progress schema identity is absent"))?,
                        )
                        .ok_or_else(|| anyhow!("progress schema source is absent"))?,
                )?;
                let relative = pointer
                    .as_str()
                    .and_then(|pointer| pointer.strip_prefix(prefix))
                    .ok_or_else(|| anyhow!("progress correlation is outside its message"))?;
                let field =
                    invocation_parameter_schema(&schema, &format!("/message/params/{relative}"))?;
                if resolve_invocation_schema(&schema, field)?["type"] != "string" {
                    bail!("progress correlation must select a required string schema");
                }
            }
        }
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

/// This ingress intentionally accepts a closed structural projection, not
/// the entire event-template language. Dynamic caller values still need wire
/// validation; missing paths or incompatible authored mapping shapes must
/// fail here, before launching an upstream process.
fn validate_invocation_projection(
    mapping: &StructuredSessionInvocationMapping,
    routes: &[Value],
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    let schema = |name: &str| -> Result<Value> {
        serde_json::from_slice(
            files
                .get(name)
                .ok_or_else(|| anyhow!("invocation schema is absent"))?,
        )
        .map_err(Into::into)
    };
    let request = schema(&mapping.request_schema)?;
    for pointer in [
        &mapping.session_pointer,
        &mapping.operation_pointer,
        &mapping.call_pointer,
    ] {
        let field = invocation_parameter_schema(&request, pointer)?;
        if field.get("type").and_then(Value::as_str) != Some("string") {
            bail!("invocation correlation must select a required string schema");
        }
    }
    for (pointer, expected) in &mapping.required_values {
        let field = invocation_parameter_schema_optional(&request, pointer, expected.is_null())?;
        validate_projected_value(&request, field, expected)?;
    }
    if mapping.request.get("op").and_then(Value::as_str) != Some("pointer")
        || mapping
            .request
            .get("optional")
            .is_some_and(|value| value != &Value::Bool(false))
    {
        bail!("invocation input must select one required request parameter");
    }
    let input = invocation_parameter_schema(
        &request,
        mapping
            .request
            .get("pointer")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("invocation input pointer is absent"))?,
    )?;
    // Vendor arguments may be deliberately untyped JSON. A statically typed
    // scalar/array cannot possibly decode as our execute-request object.
    let input = resolve_invocation_schema(&request, input)?;
    if input == &Value::Bool(false)
        || input.get("type").is_some_and(|kind| {
            kind != "object"
                && !kind
                    .as_array()
                    .is_some_and(|types| types.iter().any(|kind| kind == "object"))
        })
    {
        bail!("invocation input schema cannot describe an execute-request object");
    }
    let route = routes
        .iter()
        .find(|route| route["id"].as_str() == Some(&mapping.registration_route))
        .ok_or_else(|| anyhow!("registration route disappeared"))?;
    let registration_schema = schema(
        route["request_schema"]
            .as_str()
            .ok_or_else(|| anyhow!("registration route has no schema"))?,
    )?;
    let field = registration_schema
        .get("properties")
        .and_then(|properties| properties.get(&mapping.registration_field))
        .ok_or_else(|| anyhow!("registration field is absent from its signed route schema"))?;
    let registration = invocation_template_shape(&mapping.registration, true, false)?;
    validate_projected_value(&registration_schema, field, &registration)
        .context("registration projection")?;
    validate_dynamic_projection(
        &registration_schema,
        field,
        &mapping.registration,
        &registration,
        0,
    )?;
    let response_schema = schema(&mapping.response_schema)?;
    for success in [false, true] {
        let response = invocation_template_shape(&mapping.response, false, success)?;
        validate_projected_value(&response_schema, &response_schema, &response)
            .context("response projection")?;
        validate_dynamic_projection(
            &response_schema,
            &response_schema,
            &mapping.response,
            &response,
            0,
        )?;
    }
    Ok(())
}

fn invocation_parameter_schema<'a>(schema: &'a Value, pointer: &str) -> Result<&'a Value> {
    invocation_parameter_schema_optional(schema, pointer, false)
}

fn invocation_parameter_schema_optional<'a>(
    schema: &'a Value,
    pointer: &str,
    allow_absent: bool,
) -> Result<&'a Value> {
    let path = pointer
        .strip_prefix("/message/params/")
        .ok_or_else(|| anyhow!("invocation pointer is outside request parameters"))?;
    let mut selected = schema;
    for part in path.split('/') {
        for _ in 0..32 {
            let Some(reference) = selected.get("$ref").and_then(Value::as_str) else {
                break;
            };
            selected = schema
                .pointer(
                    reference
                        .strip_prefix('#')
                        .ok_or_else(|| anyhow!("nonlocal invocation schema"))?,
                )
                .ok_or_else(|| anyhow!("invocation schema reference is absent"))?;
        }
        let name = part.replace("~1", "/").replace("~0", "~");
        if !allow_absent
            && !selected
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| required.iter().any(|value| value.as_str() == Some(&name)))
        {
            bail!("invocation pointer must select a required schema field");
        }
        selected = selected
            .get("properties")
            .and_then(|fields| fields.get(&name))
            .ok_or_else(|| anyhow!("invocation pointer names an absent schema field"))?;
    }
    Ok(selected)
}

fn resolve_invocation_schema<'a>(root: &'a Value, mut selected: &'a Value) -> Result<&'a Value> {
    for _ in 0..32 {
        let Some(reference) = selected.get("$ref").and_then(Value::as_str) else {
            return Ok(selected);
        };
        selected = root
            .pointer(
                reference
                    .strip_prefix('#')
                    .ok_or_else(|| anyhow!("nonlocal invocation schema"))?,
            )
            .ok_or_else(|| anyhow!("invocation schema reference is absent"))?;
    }
    bail!("invocation schema reference depth exceeded")
}

fn validate_progress_observation(observation: &Value, correlation_prefix: &str) -> Result<()> {
    let fields = observation
        .pointer("/value/fields")
        .and_then(Value::as_object)
        .filter(|fields| fields.len() == 4)
        .ok_or_else(|| anyhow!("progress must project one closed lifecycle start"))?;
    if observation["when"]
        .as_array()
        .is_none_or(|predicates| !predicates.is_empty())
        || observation["value"]["op"] != "object"
    {
        bail!("progress lifecycle start must be unconditional");
    }
    for (name, value) in [
        ("kind", "state"),
        ("expected", "idle"),
        ("next", "turn_running"),
    ] {
        if fields.get(name) != Some(&serde_json::json!({"op":"literal","value":value})) {
            bail!("progress lifecycle edge is not the supported start transition");
        }
    }
    let turn = fields
        .get("turn_id")
        .ok_or_else(|| anyhow!("progress has no turn correlation"))?;
    if turn["op"] != "pointer"
        || turn["optional"].as_bool() == Some(true)
        || turn["pointer"]
            .as_str()
            .is_none_or(|pointer| !pointer.starts_with(correlation_prefix))
        || turn["max_string_bytes"]
            .as_u64()
            .is_none_or(|limit| limit == 0 || limit > 256)
    {
        bail!("progress turn correlation is not bounded and required");
    }
    Ok(())
}

fn validate_projected_value(root: &Value, selected: &Value, value: &Value) -> Result<()> {
    let schema = serde_json::json!({"definitions":root.get("definitions").cloned().unwrap_or(serde_json::json!({})),
        "$defs":root.get("$defs").cloned().unwrap_or(serde_json::json!({})),"allOf":[selected]});
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| anyhow!("compile invocation projection: {error}"))?;
    if !validator.is_valid(value) {
        bail!("authored invocation projection contradicts its signed schema");
    }
    Ok(())
}

fn invocation_template_shape(template: &Value, registration: bool, success: bool) -> Result<Value> {
    match template["op"].as_str() {
        Some("literal") => Ok(template["value"].clone()),
        Some("object") => template["fields"]
            .as_object()
            .ok_or_else(|| anyhow!("invalid invocation object"))?
            .iter()
            .map(|(name, child)| {
                Ok((
                    name.clone(),
                    invocation_template_shape(child, registration, success)?,
                ))
            })
            .collect::<Result<serde_json::Map<_, _>>>()
            .map(Value::Object),
        Some("array") => template["values"]
            .as_array()
            .ok_or_else(|| anyhow!("invalid invocation array"))?
            .iter()
            .map(|child| invocation_template_shape(child, registration, success))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        Some("json_string")
            if template["pointer"].as_str()
                == Some(if registration {
                    "/workload/executions"
                } else {
                    "/outcome/result"
                }) =>
        {
            if !registration
                && template["max_bytes"]
                    .as_u64()
                    .is_none_or(|limit| limit < 256)
            {
                bail!("invocation result budget cannot carry its bounded failure outcome");
            }
            Ok(Value::String("[]".to_owned()))
        }
        Some("pointer")
            if !registration && template["pointer"].as_str() == Some("/outcome/success") =>
        {
            Ok(Value::Bool(success))
        }
        _ => {
            bail!("invocation mapping exceeds the closed registration/result projection vocabulary")
        }
    }
}

/// A sample validates fixed fields, not arbitrary future result strings.
/// Prove the dynamic leaves against a deliberately closed structural subset
/// of JSON Schema. Unsupported cross-field constraints fail admission rather
/// than becoming a runtime surprise after a child has already executed.
fn validate_dynamic_projection(
    root: &Value,
    schema: &Value,
    template: &Value,
    witness: &Value,
    depth: usize,
) -> Result<()> {
    if depth > 32 {
        bail!("dynamic invocation projection exceeds depth bound");
    }
    if template["op"] == "literal" || template["op"] == "pointer" {
        return Ok(());
    }
    let schema = resolve_invocation_schema(root, schema)?;
    if schema == &Value::Bool(true) || schema.as_object().is_some_and(|object| object.is_empty()) {
        return Ok(());
    }
    for union in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(union).and_then(Value::as_array) {
            // Union siblings could constrain the dynamic value independently.
            if schema
                .as_object()
                .unwrap()
                .keys()
                .any(|key| ![union, "title", "description"].contains(&key.as_str()))
            {
                bail!("dynamic projection union has unsupported sibling constraints");
            }
            let matching = branches
                .iter()
                .enumerate()
                .filter(|(_, branch)| validate_projected_value(root, branch, witness).is_ok())
                .collect::<Vec<_>>();
            if matching.len() != 1 {
                bail!("dynamic invocation projection lacks one structural union branch");
            }
            let (selected_index, selected) = matching[0];
            if union == "oneOf" {
                for (index, branch) in branches.iter().enumerate() {
                    if index != selected_index
                        && !static_projection_refutes(root, branch, template)?
                    {
                        bail!("dynamic invocation union branches are not statically disjoint");
                    }
                }
            }
            return validate_dynamic_projection(root, selected, template, witness, depth + 1);
        }
    }
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow!("dynamic invocation schema is not structural"))?;
    let allowed: &[&str] = match template["op"].as_str() {
        Some("json_string") => &[
            "type",
            "title",
            "description",
            "default",
            "$comment",
            "maxLength",
        ],
        Some("object") => &[
            "type",
            "title",
            "description",
            "default",
            "$schema",
            "definitions",
            "$defs",
            "properties",
            "required",
            "additionalProperties",
            "minProperties",
            "maxProperties",
        ],
        Some("array") => &[
            "type",
            "title",
            "description",
            "default",
            "items",
            "minItems",
            "maxItems",
        ],
        _ => bail!("unsupported dynamic invocation template"),
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        bail!(
            "dynamic invocation schema contains unsupported value constraints: {:?}",
            object
                .keys()
                .filter(|key| !allowed.contains(&key.as_str()))
                .collect::<Vec<_>>()
        );
    }
    match template["op"].as_str() {
        Some("json_string") => {
            if template["pointer"] == "/outcome/result"
                && template["max_bytes"]
                    .as_u64()
                    .is_none_or(|limit| limit < 256)
            {
                bail!("invocation result budget cannot carry its bounded failure outcome");
            }
            if schema
                .get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|limit| limit < template["max_bytes"].as_u64().unwrap_or(u64::MAX))
            {
                bail!("dynamic invocation string exceeds target schema capacity");
            }
        }
        Some("object") => {
            for (name, child) in template["fields"].as_object().unwrap() {
                let selected = schema
                    .get("properties")
                    .and_then(|fields| fields.get(name))
                    .or_else(|| schema.get("additionalProperties"))
                    .unwrap_or(&Value::Bool(true));
                validate_dynamic_projection(root, selected, child, &witness[name], depth + 1)?;
            }
        }
        Some("array") => {
            let selected = schema.get("items").unwrap_or(&Value::Bool(true));
            for (index, child) in template["values"].as_array().unwrap().iter().enumerate() {
                validate_dynamic_projection(root, selected, child, &witness[index], depth + 1)?;
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn static_projection_refutes(root: &Value, schema: &Value, template: &Value) -> Result<bool> {
    let schema = resolve_invocation_schema(root, schema)?;
    if template["op"] == "literal" {
        return Ok(validate_projected_value(root, schema, &template["value"]).is_err());
    }
    let structural_type = match template["op"].as_str() {
        Some("object") => "object",
        Some("array") => "array",
        _ => return Ok(false),
    };
    if schema
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != structural_type)
    {
        return Ok(true);
    }
    if let Some(fields) = template.get("fields").and_then(Value::as_object) {
        for (name, child) in fields {
            if child["op"] == "literal"
                && let Some(selected) = schema
                    .get("properties")
                    .and_then(|properties| properties.get(name))
                && validate_projected_value(root, selected, &child["value"]).is_err()
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
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

/// Route HTTP addressing exists only under the HTTP transport, where it is
/// mandatory. A stdio profile must not carry HTTP addressing, and an HTTP
/// profile without complete addressing is not dispatchable.
fn validate_route_transport_addressing(
    route: &serde_json::Map<String, Value>,
    http_transport: bool,
    binding_action: Option<&str>,
) -> Result<()> {
    match (http_transport, route.get("http_method"), route.get("http_path")) {
        (true, Some(method), Some(path)) => {
            let method = method
                .as_str()
                .ok_or_else(|| anyhow!("structured-session route HTTP method must be a string"))?;
            if !matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
                bail!("structured-session route HTTP method is not admitted");
            }
            let path = path
                .as_str()
                .ok_or_else(|| anyhow!("structured-session route HTTP path must be a string"))?;
            validate_http_path(path, &["session_id"])?;
            if path.contains("{session_id}")
                && !matches!(binding_action, Some("require") | Some("bind_expected"))
            {
                bail!("structured-session HTTP session placeholder requires a bound session");
            }
        }
        (true, _, _) => {
            bail!("structured-session HTTP route lacks its method or path")
        }
        (false, None, None) => {}
        (false, _, _) => {
            bail!("structured-session route HTTP addressing requires the HTTP transport");
        }
    }
    Ok(())
}

/// An HTTP path template is bounded, absolute, carries no query or fragment,
/// and its only parameterization is the named closed placeholder set.
fn validate_http_path(value: &str, placeholders: &[&str]) -> Result<()> {
    if value.len() > 512
        || !value.starts_with('/')
        || value.chars().any(char::is_control)
        || value.contains('?')
        || value.contains('#')
        || value.contains("//")
    {
        bail!("structured-session HTTP path template is invalid");
    }
    let mut remainder = value;
    while let Some(start) = remainder.find('{') {
        let end = remainder[start..]
            .find('}')
            .ok_or_else(|| anyhow!("structured-session HTTP path placeholder is unterminated"))?;
        let name = &remainder[start + 1..start + end];
        if !placeholders.contains(&name) {
            bail!("structured-session HTTP path placeholder is not admitted");
        }
        remainder = &remainder[start + end + 1..];
        if remainder.starts_with('{') {
            bail!("structured-session HTTP path placeholders are not separated");
        }
    }
    if remainder.contains('}') {
        bail!("structured-session HTTP path has a stray closing placeholder");
    }
    for segment in value.split('/') {
        if segment.contains('{')
            && !segment
                .trim_start_matches('{')
                .trim_end_matches('}')
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            bail!("structured-session HTTP path placeholder segment is not canonical");
        }
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
        "json_string" => {
            require_keys(object, &["op", "pointer", "max_bytes"], &[])?;
            validate_pointer(value_string(object, "pointer")?)?;
            if object["max_bytes"]
                .as_u64()
                .is_none_or(|limit| limit == 0 || limit > 1024 * 1024)
            {
                bail!("structured-session JSON string byte bound is invalid");
            }
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

fn validate_workload_executable_member(value: &str) -> Result<()> {
    if value.len() > 4096 {
        bail!("structured-session workload executable exceeds its path bound");
    }
    ryeos_state::objects::validate_canonical_project_relative_path(value)
        .context("structured-session workload executable is not a canonical relative member")
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
            "schema_version":STRUCTURED_SESSION_PROFILE_SCHEMA_VERSION,
            "transport":"stdio_jsonrpc",
            "http_sse":null,
            "workload_realization_id":"fixture-runtime",
            "workload_executable":"fixture-worker",
            "required_process_environment":[],
            "workload_args":[],
            "workload_home_env":"FIXTURE_HOME",
            "workload_client":null,
            "baseline_config":"baseline.conf",
            "baseline_destination":"runtime.conf",
            "portable_state":null,
            "credential_subject":null,
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
    fn authored_profiles_compile_from_the_exact_local_source_set() {
        let root = crate::test_support::workspace_root()
            .join("bundles/codex/.ai/workers/codex/lib/hosted");
        fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    collect(root, &path, files);
                } else {
                    assert!(entry.file_type().unwrap().is_file());
                    files.insert(
                        path.strip_prefix(root)
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_owned(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut files = BTreeMap::new();
        collect(&root, &root, &mut files);
        for name in ["authoring.profile.json", "structured-session.profile.json"] {
            compile(&files[name], &files).unwrap();
        }
        // Early lifecycle authority must be correlated before the daemon ACK
        // can permit a child; final-response corroboration is too late.
        let profile: Value = serde_json::from_slice(&files["authoring.profile.json"]).unwrap();
        for pointer in [
            Value::Null,
            json!("/message/params/absent"),
            json!("/message/params/turn"),
        ] {
            let mut invalid = profile.clone();
            let notification = invalid["notifications"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|rule| rule["method"] == "turn/started")
                .unwrap();
            notification["upstream_session_pointer"] = pointer;
            assert!(compile(&serde_json::to_vec(&invalid).unwrap(), &files).is_err());
        }
    }

    #[test]
    fn invocation_mapping_refuses_missing_paths_and_incompatible_shapes_at_admission() {
        let mut profile: Value =
            serde_json::from_slice(&fixture_profile("session.start", "session/start")).unwrap();
        profile["routes"][0]["session_binding"] =
            json!({"action":"bind_new","request_field":null,"response_pointer":"/result/id"});
        profile["routes"][0]["forbidden_fields"] = json!(["tools"]);
        let mapping = json!({
            "registration_route":"session.start","registration_field":"tools",
            "registration":{"op":"array","values":[{"op":"object","fields":{
                "description":{"op":"json_string","pointer":"/workload/executions","max_bytes":49152}
            }}]},
            "method":"operation/execute","request_schema":"schema/invoke.json","response_schema":"schema/invoked.json",
            "session_pointer":"/message/params/session","operation_pointer":"/message/params/turn","call_pointer":"/message/params/call",
            "required_values":{"/message/params/name":"execute"},
            "request":{"op":"pointer","pointer":"/message/params/arguments"},
            "response":{"op":"object","fields":{"success":{"op":"pointer","pointer":"/outcome/success"}}}
        });
        profile["workload_client"] = json!({"cli_endpoint_env":null,"structured_session":mapping});
        let mut files = schemas();
        files.insert("schema/request.json".to_owned(), serde_json::to_vec(&json!({"type":"object","properties":{
            "tools":{"type":"array","items":{"type":"object","required":["description"],"properties":{"description":{"type":"string"}}}}
        }})).unwrap());
        files.insert("schema/invoke.json".to_owned(), serde_json::to_vec(&json!({"type":"object",
            "required":["session","turn","call","name","arguments"],"properties":{
                "session":{"type":"string"},"turn":{"type":"string"},"call":{"type":"string"},"name":{"type":"string"},"arguments":true
            }})).unwrap());
        files.insert(
            "schema/invoked.json".to_owned(),
            serde_json::to_vec(&json!({"type":"object","required":["success"],
            "properties":{"success":{"type":"boolean"}},"additionalProperties":false}))
            .unwrap(),
        );
        compile(&serde_json::to_vec(&profile).unwrap(), &files).unwrap();
        for (pointer, value) in [
            (
                "/workload_client/structured_session/request/pointer",
                json!("/message/params/absent"),
            ),
            (
                "/workload_client/structured_session/request/pointer",
                json!("/message/params/turn"),
            ),
            (
                "/workload_client/structured_session/session_pointer",
                json!("/message/params/absent"),
            ),
            (
                "/workload_client/structured_session/response/fields/success/pointer",
                json!("/outcome/unknown"),
            ),
            (
                "/workload_client/structured_session/registration",
                json!({"op":"literal","value":{}}),
            ),
            ("/routes/0/forbidden_fields", json!([])),
        ] {
            let mut invalid = profile.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            assert!(
                compile(&serde_json::to_vec(&invalid).unwrap(), &files).is_err(),
                "{pointer}"
            );
        }
        for constraint in [json!({"const":"[]"}), json!({"maxLength":2})] {
            let mut invalid_files = files.clone();
            let mut request: Value =
                serde_json::from_slice(&invalid_files["schema/request.json"]).unwrap();
            request["properties"]["tools"]["items"]["properties"]["description"]
                .as_object_mut()
                .unwrap()
                .extend(constraint.as_object().unwrap().clone());
            invalid_files.insert(
                "schema/request.json".to_owned(),
                serde_json::to_vec(&request).unwrap(),
            );
            assert!(compile(&serde_json::to_vec(&profile).unwrap(), &invalid_files).is_err());
        }
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
            Some(u64::from(STRUCTURED_SESSION_PROFILE_SCHEMA_VERSION))
        );
    }

    #[test]
    fn workload_executable_accepts_only_canonical_relative_members() {
        let mut profile: Value =
            serde_json::from_slice(&fixture_profile("job.status", "job/status")).unwrap();
        profile["workload_executable"] = json!("bin/fixture-worker");
        compile(&serde_json::to_vec(&profile).unwrap(), &schemas())
            .expect("a nested canonical tree member must be admitted");

        for invalid in [
            "",
            "/bin/fixture-worker",
            "../fixture-worker",
            "bin/../fixture-worker",
            "bin//fixture-worker",
            "bin/./fixture-worker",
            "bin\\fixture-worker",
            "bin/fixture-worker/",
            "bin/\u{0}fixture-worker",
        ] {
            profile["workload_executable"] = json!(invalid);
            assert!(
                compile(&serde_json::to_vec(&profile).unwrap(), &schemas()).is_err(),
                "non-canonical workload executable was admitted: {invalid:?}"
            );
        }
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

    #[test]
    fn transport_is_required_and_closed() {
        let profile: Value =
            serde_json::from_slice(&fixture_profile("job.status", "job/status")).unwrap();
        compile(&serde_json::to_vec(&profile).unwrap(), &schemas())
            .expect("the current dialect must declare the stdio transport");

        let mut missing = profile.clone();
        missing.as_object_mut().unwrap().remove("transport");
        assert!(compile(&serde_json::to_vec(&missing).unwrap(), &schemas()).is_err());

        let mut retired = profile.clone();
        retired["schema_version"] = json!(STRUCTURED_SESSION_PROFILE_SCHEMA_VERSION - 1);
        assert!(compile(&serde_json::to_vec(&retired).unwrap(), &schemas()).is_err());

        for invalid in ["", "stdio", "auto", "HTTP_SSE", "unix_socket"] {
            let mut closed = profile.clone();
            closed["transport"] = json!(invalid);
            assert!(
                compile(&serde_json::to_vec(&closed).unwrap(), &schemas()).is_err(),
                "un admitted transport was accepted: {invalid:?}"
            );
        }
    }

    #[test]
    fn http_addressing_is_gated_by_the_declared_transport() {
        let mut profile: Value =
            serde_json::from_slice(&fixture_profile("session.start", "session/start")).unwrap();
        profile["routes"][0]["http_method"] = json!("POST");
        profile["routes"][0]["http_path"] = json!("/session");
        assert!(compile(&serde_json::to_vec(&profile).unwrap(), &schemas()).is_err());

        let mut http = profile.clone();
        http["transport"] = json!("http_sse");
        http["http_sse"] = json!({
            "username_env":"FIXTURE_HTTP_USER",
            "password_env":"FIXTURE_HTTP_PASSWORD"
        });
        http["initialization"] = json!([]);
        compile(&serde_json::to_vec(&http).unwrap(), &schemas())
            .expect("complete HTTP addressing must be admitted");

        let mut missing_credentials = http.clone();
        missing_credentials["http_sse"] = Value::Null;
        assert!(compile(&serde_json::to_vec(&missing_credentials).unwrap(), &schemas()).is_err());

        let mut stdio_credentials = http.clone();
        stdio_credentials["transport"] = json!("stdio_jsonrpc");
        assert!(compile(&serde_json::to_vec(&stdio_credentials).unwrap(), &schemas()).is_err());

        let mut colliding = http.clone();
        colliding["http_sse"]["password_env"] = json!("FIXTURE_HOME");
        assert!(compile(&serde_json::to_vec(&colliding).unwrap(), &schemas()).is_err());

        let mut handshake = http.clone();
        handshake["initialization"] = json!([{
            "method":"initialize","effect_class":"pure_read","params":{},
            "response_schema":"schema/response.json","notification":null
        }]);
        assert!(compile(&serde_json::to_vec(&handshake).unwrap(), &schemas()).is_err());

        let mut missing_path = http.clone();
        missing_path["routes"][0]
            .as_object_mut()
            .unwrap()
            .remove("http_path");
        assert!(compile(&serde_json::to_vec(&missing_path).unwrap(), &schemas()).is_err());

        let mut bad_method = http.clone();
        bad_method["routes"][0]["http_method"] = json!("TRACE");
        assert!(compile(&serde_json::to_vec(&bad_method).unwrap(), &schemas()).is_err());

        for bad_path in [
            "session",
            "/session?query=1",
            "/session#fragment",
            "/session/{turn}",
            "/session/{session_id}{session_id}",
            "/session/}",
            "/session//message",
        ] {
            let mut invalid = http.clone();
            invalid["routes"][0]["http_path"] = json!(bad_path);
            assert!(
                compile(&serde_json::to_vec(&invalid).unwrap(), &schemas()).is_err(),
                "invalid HTTP path was admitted: {bad_path}"
            );
        }

        let mut bound = http.clone();
        bound["routes"][0]["http_path"] = json!("/session/{session_id}/message");
        assert!(compile(&serde_json::to_vec(&bound).unwrap(), &schemas()).is_err());

        bound["routes"][0]["session_binding"] =
            json!({"action":"require","request_field":"sessionID","response_pointer":null});
        compile(&serde_json::to_vec(&bound).unwrap(), &schemas())
            .expect("a bound session placeholder must be admitted");
    }

    #[test]
    fn http_server_requests_require_an_admitted_reply_path() {
        let mut profile: Value =
            serde_json::from_slice(&fixture_profile("session.start", "session/start")).unwrap();
        profile["transport"] = json!("http_sse");
        profile["http_sse"] = json!({
            "username_env":"FIXTURE_HTTP_USER",
            "password_env":"FIXTURE_HTTP_PASSWORD"
        });
        profile["initialization"] = json!([]);
        profile["routes"][0]["http_method"] = json!("POST");
        profile["routes"][0]["http_path"] = json!("/session");
        profile["server_requests"] = json!([{
            "method":"permission.asked",
            "schema":"schema/request.json",
            "operation_class":"command_execution",
            "correlation":{
                "upstream_session_pointer":"/message/params/sessionID",
                "operation_pointer":"/message/params/permissionID"
            },
            "responses":{
                "accept":{"op":"literal","value":{"response":"once"}},
                "cancel":{"op":"literal","value":{"response":"deny"}},
                "decline":{"op":"literal","value":{"response":"deny"}},
                "expire":{"op":"literal","value":{"response":"deny"}}
            },
            "deny_only":true,
            "permission_delta_fields":[],
            "display":{"op":"object","fields":{}}
        }]);
        assert!(compile(&serde_json::to_vec(&profile).unwrap(), &schemas()).is_err());

        profile["server_requests"][0]["reply_http_path"] =
            json!("/session/{permission}/permissions");
        assert!(compile(&serde_json::to_vec(&profile).unwrap(), &schemas()).is_err());

        profile["server_requests"][0]["reply_http_path"] =
            json!("/permissions/{request_id}");
        compile(&serde_json::to_vec(&profile).unwrap(), &schemas())
            .expect("a complete HTTP server request must be admitted");

        let mut stdio = profile.clone();
        stdio["transport"] = json!("stdio_jsonrpc");
        assert!(compile(&serde_json::to_vec(&stdio).unwrap(), &schemas()).is_err());
    }
}
