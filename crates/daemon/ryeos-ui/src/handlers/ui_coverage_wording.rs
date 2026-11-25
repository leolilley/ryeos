//! `ui.ryeos.coverage.wording` — the signed words for a table's coverage line.
//!
//! Services report coverage as `{state, reason}` codes. The sentence a person
//! reads for a pair, and the tone it is drawn in, come from the signed config
//! `config:ryeos/ui/coverage-wording`: a default sentence per state, a sentence
//! per reason, and a fallback for a state the config does not list. Clients
//! read this service as a Surface source and resolve the pair themselves, so
//! Rune and the terminal share one wording and neither carries any of it.
//!
//! This module decides no meaning. It verifies the config, checks that every
//! sentence is non-empty and every tone is in the closed list, and returns the
//! tables as the config wrote them. A config that fails either check is
//! refused, never served half-read.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_client_base::ui::dto::{COVERAGE_WORDING_CONTRACT, RyeOsCoverageWordingDto};
use ryeos_executor::executor::ServiceAvailability;

pub const CONFIG_REF: &str = "config:ryeos/ui/coverage-wording";
const CONFIG_PATH: &str = "ryeos/ui/coverage-wording.yaml";

/// Parse a verified config body into the response the clients read. The
/// envelope fields are dropped first; any other unknown field is refused.
pub fn response_from_config(mut value: Value) -> Result<Value, String> {
    if let Some(object) = value.as_object_mut() {
        for envelope in ["category", "version", "description"] {
            object.remove(envelope);
        }
    }
    let Some(body) = value.as_object() else {
        return Err("coverage wording is not a mapping".into());
    };
    if let Some(unknown) = body
        .keys()
        .find(|key| !matches!(key.as_str(), "fallback" | "states" | "reasons"))
    {
        return Err(format!("coverage wording has an unknown field {unknown}"));
    }
    let mut response = Map::new();
    response.insert("schema_version".into(), json!(COVERAGE_WORDING_CONTRACT));
    response.extend(body.clone());
    let response = Value::Object(response);
    RyeOsCoverageWordingDto::from_response(&response)?;
    Ok(response)
}

/// Resolve and verify the signed wording.
pub fn load_response(state: &AppState) -> Result<Value> {
    let value = super::ui_attention::load_signed_config(state, CONFIG_PATH, CONFIG_REF)?;
    Ok(response_from_config(value).map_err(HandlerError::Internal)?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageWordingRequest {}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let _request: CoverageWordingRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid request: {error}")))?;
    load_response(&state)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/coverage/wording",
    endpoint: "ui.ryeos.coverage.wording",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped() -> Value {
        let path = ryeos_engine::test_support::workspace_root()
            .join("bundles/ryeos-ui/.ai/config/ryeos/ui/coverage-wording.yaml");
        serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn the_shipped_config_is_a_readable_response() {
        let response = response_from_config(shipped()).unwrap();
        let wording = RyeOsCoverageWordingDto::from_response(&response).unwrap();
        for state in ["complete", "observed", "partial", "failed", "unavailable"] {
            assert!(wording.states.contains_key(state), "{state}");
        }
        assert_eq!(wording.fallback.label, "This page's coverage is not known.");
        assert_eq!(
            wording
                .resolve("observed", "projection_locator_not_exhaustive")
                .label,
            "The page does not prove exhaustive membership."
        );
        for slot in ["empty", "empty_project"] {
            assert!(wording.reasons.contains_key(slot), "{slot}");
        }
    }

    #[test]
    fn the_shipped_config_has_a_sentence_for_each_seat_project_absence() {
        let response = response_from_config(shipped()).unwrap();
        let wording = RyeOsCoverageWordingDto::from_response(&response).unwrap();
        for code in [
            "unregistered",
            "owner_key_not_active",
            "no_project_name",
            "checkout_name_changed",
        ] {
            let entry = wording
                .reasons
                .get(&format!("seat_project_{code}"))
                .unwrap_or_else(|| panic!("no sentence for {code}"));
            assert!(!entry.label.trim().is_empty(), "{code}");
        }
    }

    #[test]
    fn a_bad_tone_an_empty_sentence_or_a_stray_field_is_refused() {
        let mut bad_tone = shipped();
        bad_tone["states"]["failed"]["tone"] = json!("loud");
        assert!(response_from_config(bad_tone).is_err());
        let mut empty = shipped();
        empty["reasons"]["ready"]["label"] = json!("  ");
        assert!(response_from_config(empty).is_err());
        let mut stray = shipped();
        stray["extra"] = json!(true);
        assert!(response_from_config(stray).is_err());
        assert!(response_from_config(json!([])).is_err());
    }
}
