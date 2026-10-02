//! Narrow Q launch bindings derived from exact accepted consumer definitions.
//! This does not rerun ordinary WorkerExecution or change Q's zero-export
//! workspace authority. Source identity and full admission remain separate.

use anyhow::{Context as _, Result, ensure};
use ryeos_state::external_content::products::qualification::ProductQualificationConsumerWorkerRequest;
use ryeos_state::objects::canonical_value_digest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationWorkerCredentialRequirement {
    pub workload_family: String,
    pub required_state: String,
    pub subject_projection_contract: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QualificationWorkerLaunchContract {
    pub credential_profile_id: String,
    pub credential_requirement: QualificationWorkerCredentialRequirement,
    pub route_set: String,
    pub allowed_effect_classes: Vec<String>,
    pub credential_home_env: String,
    pub workspace_env: String,
    pub required_credential_state: String,
    pub max_lifetime_seconds: u64,
    pub session_start_route: String,
    pub turn_start_route: String,
    pub recover_upstream_session: bool,
}

impl QualificationWorkerLaunchContract {
    pub fn digest(&self) -> Result<String> {
        canonical_value_digest(&serde_json::json!({
            "schema":"ryeos.qualification_worker_launch_contract.v1", "contract":self
        }))
    }

    pub(super) fn from_accepted(
        execution: &Value,
        environment: &Value,
        profile: &Value,
        request: &ProductQualificationConsumerWorkerRequest,
    ) -> Result<Self> {
        request.validate()?;
        // Environment v6 has a required active credential object. Explicit
        // null selection is valid request data but cannot satisfy this lane.
        ensure!(
            environment.get("schema").and_then(Value::as_str)
                == Some("ryeos.worker_environment.v6"),
            "Q environment has no supported accepted credential contract"
        );
        let credential_requirement: QualificationWorkerCredentialRequirement =
            serde_json::from_value(
                environment
                    .get("credential_requirement")
                    .context("Q environment has no credential requirement")?
                    .clone(),
            )?;
        bounded_text(&credential_requirement.workload_family, 64)?;
        bounded_text(&credential_requirement.subject_projection_contract, 128)?;
        ensure!(
            credential_requirement.required_state == "active",
            "Q environment credential state is unsupported"
        );
        ensure!(
            profile
                .pointer("/credential_subject/contract")
                .and_then(Value::as_str)
                == Some(credential_requirement.subject_projection_contract.as_str()),
            "Q environment and accepted Worker credential subject contracts differ"
        );
        let credential_profile_id = request
            .credential_profile_id
            .clone()
            .context("Q accepted environment requires an explicit credential profile selection")?;
        let required_credential_state = field(execution, "required_credential_state")?.to_owned();
        ensure!(
            matches!(required_credential_state.as_str(), "any" | "active"),
            "Q WorkerExecution credential state is unsupported"
        );
        let route_set = field(execution, "route_set")?.to_owned();
        bounded_text(&route_set, 128)?;
        ensure!(
            route_set.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_')),
            "Q WorkerExecution route set is not canonical"
        );
        let allowed_effect_classes: Vec<String> = serde_json::from_value(
            execution
                .get("allowed_effect_classes")
                .context("Q WorkerExecution has no effect ceiling")?
                .clone(),
        )?;
        const EFFECTS: &[&str] = &[
            "credential_delete",
            "credential_read",
            "credential_write",
            "external_effect",
            "pure_read",
            "session_mutation",
        ];
        ensure!(
            !allowed_effect_classes.is_empty()
                && allowed_effect_classes.len() <= EFFECTS.len()
                && allowed_effect_classes
                    .iter()
                    .all(|effect| EFFECTS.contains(&effect.as_str()))
                && allowed_effect_classes
                    .windows(2)
                    .all(|pair| pair[0] < pair[1]),
            "Q WorkerExecution effect ceiling is not a sorted admitted subset"
        );
        let credential_home_env = field(execution, "credential_home_env")?.to_owned();
        let workspace_env = field(execution, "workspace_env")?.to_owned();
        for name in [&credential_home_env, &workspace_env] {
            ryeos_engine::protocol_vocabulary::validate_env_name(name)
                .map_err(anyhow::Error::msg)?;
            ensure!(
                !matches!(
                    name.as_str(),
                    "RYEOS_STRUCTURED_SESSION_ROUTE_SET"
                        | "RYEOS_STRUCTURED_SESSION_EFFECT_CLASSES"
                ),
                "Q WorkerExecution home/workspace slot collides with bridge policy"
            );
        }
        ensure!(
            credential_home_env != workspace_env,
            "Q credential and workspace slots must differ"
        );
        let max_lifetime_seconds = execution
            .get("max_lifetime_seconds")
            .and_then(Value::as_u64)
            .filter(|seconds| (1..=603_600).contains(seconds))
            .context("Q WorkerExecution lifetime is invalid")?;
        let mode = execution
            .get("mode")
            .context("Q WorkerExecution has no mode")?;
        let mode_object = mode
            .as_object()
            .context("Q WorkerExecution mode is not an object")?;
        ensure!(
            field(mode, "kind")? == "bounded_turn"
                && mode_object.keys().all(|key| matches!(
                    key.as_str(),
                    "kind"
                        | "session_start_route"
                        | "turn_start_route"
                        | "max_uncontacted_attempts"
                        | "require_post_completion_recovery"
                ))
                && mode
                    .get("max_uncontacted_attempts")
                    .and_then(Value::as_u64)
                    .is_some_and(|count| (1..=8).contains(&count))
                && mode
                    .get("require_post_completion_recovery")
                    .is_none_or(|value| value == &Value::Bool(false)),
            "Q supports only the finite setup/turn mode without post-completion recovery"
        );
        let session_start_route = field(mode, "session_start_route")?.to_owned();
        let turn_start_route = field(mode, "turn_start_route")?.to_owned();
        ensure!(
            session_start_route != turn_start_route,
            "Q setup and turn route must differ"
        );
        for (body, route) in [
            (&request.session_setup, &session_start_route),
            (&request.body, &turn_start_route),
        ] {
            let envelope = body
                .as_object()
                .context("Q signed request is not an envelope")?;
            ensure!(
                envelope.len() == 2
                    && field(body, "route_id")? == route
                    && body.get("payload").is_some_and(Value::is_object),
                "Q signed request differs from its accepted WorkerExecution route"
            );
        }
        let recover_upstream_session = execution
            .get("recover_upstream_session")
            .and_then(Value::as_bool)
            .context("Q WorkerExecution has no recovery policy")?;
        validate_profile_routes(
            &profile,
            &route_set,
            &allowed_effect_classes,
            [&session_start_route, &turn_start_route],
            recover_upstream_session,
        )?;
        ensure!(
            environment
                .get("workload_client")
                .is_some_and(Value::is_null)
                && profile.get("workload_client").is_some_and(Value::is_null),
            "Q does not implement an additional workload-client delegation channel"
        );
        Ok(Self {
            credential_profile_id,
            credential_requirement,
            route_set,
            allowed_effect_classes,
            credential_home_env,
            workspace_env,
            required_credential_state,
            max_lifetime_seconds,
            session_start_route,
            turn_start_route,
            recover_upstream_session,
        })
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("Q accepted contract lacks textual {key}"))
}

fn bounded_text(value: &str, maximum: usize) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= maximum
            && value.trim() == value
            && !value.chars().any(char::is_control),
        "Q accepted contract text is not canonical and bounded"
    );
    Ok(())
}

// Same route/effect relationship as the ordinary dedicated-session boundary;
// unlike that boundary this reads the already accepted Q profile directly.
fn validate_profile_routes(
    profile: &Value,
    route_set: &str,
    effects: &[String],
    selected: [&String; 2],
    recovery: bool,
) -> Result<()> {
    let routes = profile
        .get("routes")
        .and_then(Value::as_array)
        .context("Q profile has no routes")?;
    let route_map = routes
        .iter()
        .map(|route| Ok((field(route, "id")?, route)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let selected_routes = profile
        .get("route_sets")
        .and_then(|sets| sets.get(route_set))
        .and_then(Value::as_array)
        .context("Q profile does not admit the WorkerExecution route set")?;
    for name in selected_routes {
        let name = name.as_str().context("Q profile route set is malformed")?;
        let route = route_map
            .get(name)
            .context("Q profile selected route is absent")?;
        ensure!(
            effects
                .iter()
                .any(|effect| effect == field(route, "effect_class").unwrap_or("")),
            "Q selected profile route exceeds accepted effects"
        );
    }
    for name in selected {
        bounded_text(name, 128)?;
        ensure!(
            selected_routes
                .iter()
                .any(|value| value.as_str() == Some(name.as_str())),
            "Q setup/turn route is outside accepted route set"
        );
        let route = route_map
            .get(name.as_str())
            .context("Q selected route is absent")?;
        ensure!(
            route
                .get("audience")
                .is_none_or(|value| value.as_str() == Some("public")),
            "Q setup/turn is not a public route"
        );
    }
    for step in profile
        .get("initialization")
        .and_then(Value::as_array)
        .context("Q profile has no initialization")?
    {
        ensure!(
            effects
                .iter()
                .any(|effect| effect == field(step, "effect_class").unwrap_or("")),
            "Q initialization exceeds accepted effects"
        );
    }
    if recovery {
        ensure!(
            profile
                .pointer("/recovery/route_sets")
                .and_then(Value::as_array)
                .is_some_and(|sets| sets.iter().any(|value| value.as_str() == Some(route_set))),
            "Q WorkerExecution recovery policy differs from the accepted profile"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::external_content::products::qualification::PRODUCT_QUALIFICATION_CONSUMER_WORKER_REQUEST_SCHEMA;
    use serde_json::json;

    fn inputs() -> (
        Value,
        Value,
        Value,
        ProductQualificationConsumerWorkerRequest,
    ) {
        let execution = json!({
            "required_credential_state":"active", "route_set":"session",
            "allowed_effect_classes":["credential_read","external_effect","pure_read","session_mutation"],
            "credential_home_env":"RYEOS_WORKLOAD_HOME", "workspace_env":"RYEOS_WORKSPACE",
            "max_lifetime_seconds":100, "recover_upstream_session":true,
            "mode":{"kind":"bounded_turn","session_start_route":"session.start","turn_start_route":"turn.start","max_uncontacted_attempts":3}
        });
        let environment = json!({"schema":"ryeos.worker_environment.v6",
            "credential_requirement":{"workload_family":"fixture","required_state":"active","subject_projection_contract":"fixture.account.v1"},
            "workload_client":null
        });
        let profile = json!({"credential_subject":{"contract":"fixture.account.v1"}, "workload_client":null,
            "route_sets":{"session":["session.start","turn.start"]},
            "routes":[{"id":"session.start","effect_class":"session_mutation"},{"id":"turn.start","effect_class":"external_effect"}],
            "initialization":[{"effect_class":"pure_read"}], "recovery":{"route_sets":["session"]}
        });
        let request = ProductQualificationConsumerWorkerRequest {
            controller_canary: None,
            schema: PRODUCT_QUALIFICATION_CONSUMER_WORKER_REQUEST_SCHEMA.into(),
            credential_profile_id: Some("profile-one".into()),
            fixture: None,
            session_setup: json!({"route_id":"session.start","payload":{}}),
            body: json!({"route_id":"turn.start","payload":{}}),
        };
        (execution, environment, profile, request)
    }

    #[test]
    fn accepted_contract_derives_only_signed_binding_values() {
        let (execution, environment, profile, request) = inputs();
        let contract = QualificationWorkerLaunchContract::from_accepted(
            &execution,
            &environment,
            &profile,
            &request,
        )
        .unwrap();
        assert_eq!(contract.credential_profile_id, "profile-one");
        assert_eq!(contract.credential_home_env, "RYEOS_WORKLOAD_HOME");
        assert_eq!(contract.workspace_env, "RYEOS_WORKSPACE");
        assert_eq!(contract.max_lifetime_seconds, 100);
        let mut changed = request.clone();
        changed.credential_profile_id = Some("profile-two".into());
        let changed = QualificationWorkerLaunchContract::from_accepted(
            &execution,
            &environment,
            &profile,
            &changed,
        )
        .unwrap();
        assert_ne!(contract.digest().unwrap(), changed.digest().unwrap());
    }

    #[test]
    fn missing_selection_or_changed_environment_requirement_refuses() {
        let (execution, environment, profile, request) = inputs();
        let mut none = request.clone();
        none.credential_profile_id = None;
        none.validate().unwrap();
        assert!(
            QualificationWorkerLaunchContract::from_accepted(
                &execution,
                &environment,
                &profile,
                &none
            )
            .is_err()
        );
        for (pointer, replacement) in [
            ("/schema", json!("ryeos.worker_environment.v5")),
            ("/credential_requirement", Value::Null),
            ("/credential_requirement/required_state", json!("any")),
            (
                "/credential_requirement/subject_projection_contract",
                json!("other.account.v1"),
            ),
            ("/workload_client", json!({})),
        ] {
            let mut changed = environment.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                QualificationWorkerLaunchContract::from_accepted(
                    &execution, &changed, &profile, &request
                )
                .is_err(),
                "accepted {pointer}"
            );
        }
    }

    #[test]
    fn route_policy_and_launch_slot_substitution_refuses() {
        let (execution, environment, profile, request) = inputs();
        for (pointer, replacement) in [
            ("/route_set", json!("other")),
            ("/allowed_effect_classes", json!(["pure_read"])),
            ("/allowed_effect_classes", json!(["pure_read", "pure_read"])),
            ("/allowed_effect_classes", json!(["unknown"])),
            ("/credential_home_env", json!("RYEOS_WORKSPACE")),
            (
                "/credential_home_env",
                json!("RYEOS_STRUCTURED_SESSION_ROUTE_SET"),
            ),
            ("/workspace_env", json!("not-an-env")),
            ("/max_lifetime_seconds", json!(0)),
            ("/max_lifetime_seconds", json!(603601)),
            ("/mode/kind", json!("session")),
            ("/mode/turn_start_route", json!("other.start")),
        ] {
            let mut changed = execution.clone();
            *changed.pointer_mut(pointer).unwrap() = replacement;
            assert!(
                QualificationWorkerLaunchContract::from_accepted(
                    &changed,
                    &environment,
                    &profile,
                    &request
                )
                .is_err(),
                "accepted {pointer}"
            );
        }
        let mut changed = execution.clone();
        changed["mode"]["require_post_completion_recovery"] = json!(true);
        assert!(
            QualificationWorkerLaunchContract::from_accepted(
                &changed,
                &environment,
                &profile,
                &request
            )
            .is_err()
        );
        let mut changed = profile.clone();
        changed["routes"][1]["effect_class"] = json!("credential_write");
        assert!(
            QualificationWorkerLaunchContract::from_accepted(
                &execution,
                &environment,
                &changed,
                &request
            )
            .is_err()
        );
        let mut changed = profile.clone();
        changed["routes"][0]["audience"] = json!("runtime");
        assert!(
            QualificationWorkerLaunchContract::from_accepted(
                &execution,
                &environment,
                &changed,
                &request
            )
            .is_err()
        );
        let mut changed = profile;
        changed["recovery"]["route_sets"] = json!([]);
        assert!(
            QualificationWorkerLaunchContract::from_accepted(
                &execution,
                &environment,
                &changed,
                &request
            )
            .is_err()
        );
    }
}
