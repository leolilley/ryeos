//! `ui.ryeos.node.capabilities.wording` — the signed words for the codes a
//! client shows about a node's capabilities.
//!
//! The sentence and tone for each code come from the signed config
//! `config:ryeos/ui/node-capability-wording`. A client resolves a code against
//! that config and falls back to the config's own fallback for a code it does
//! not list, so a new host class, proof name or grant needs a config edit and
//! no code change.
//!
//! This module decides no meaning. The wording handler verifies the config,
//! refuses an unknown field, an empty sentence or a tone outside the closed
//! list, and returns the tables as the config wrote them.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

pub const WORDING_CONTRACT: &str = "ryeos.ui.node_capability_wording.v1";
pub const CONFIG_REF: &str = "config:ryeos/ui/node-capability-wording";
const CONFIG_PATH: &str = "ryeos/ui/node-capability-wording.yaml";

/// The tones a wording entry may name.
pub const TONES: [&str; 5] = ["none", "good", "warn", "danger", "accent"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesRequest {}

/// One sentence, the tone it is drawn in, and optionally a longer line that
/// says what it means.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub label: String,
    pub tone: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The signed words, keyed on the facts' codes. A code a table does not list
/// reads as `fallback`; `guarantees` and `grants` are open by design, and only
/// the names they list are worded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wording {
    pub fallback: Entry,
    pub report_states: BTreeMap<String, Entry>,
    pub isolation_modes: BTreeMap<String, Entry>,
    pub host_classes: BTreeMap<String, Entry>,
    pub proof_states: BTreeMap<String, Entry>,
    pub grades: BTreeMap<String, Entry>,
    pub guarantees: BTreeMap<String, Entry>,
    pub grants: BTreeMap<String, Entry>,
    /// The coverage word of each collection of a record read.
    pub coverage_states: BTreeMap<String, Entry>,
    /// The verification status of a record read.
    pub verify_states: BTreeMap<String, Entry>,
    /// The reason code of a record that failed or is stale.
    pub verify_reasons: BTreeMap<String, Entry>,
}

impl Wording {
    fn entries(&self) -> impl Iterator<Item = &Entry> {
        std::iter::once(&self.fallback).chain(
            [
                &self.report_states,
                &self.isolation_modes,
                &self.host_classes,
                &self.proof_states,
                &self.grades,
                &self.guarantees,
                &self.grants,
                &self.coverage_states,
                &self.verify_states,
                &self.verify_reasons,
            ]
            .into_iter()
            .flat_map(BTreeMap::values),
        )
    }
}

/// Parse a verified config body into the response clients read. The envelope
/// fields are dropped first; any other unknown field is refused, an empty
/// sentence is refused and a tone outside the closed list is refused.
pub fn response_from_config(value: Value) -> Result<Value, String> {
    let wording = wording_from_config(value)?;
    let mut response = serde_json::to_value(&wording).map_err(|error| error.to_string())?;
    if let Some(object) = response.as_object_mut() {
        object.insert("schema_version".into(), json!(WORDING_CONTRACT));
    }
    Ok(response)
}

/// Parse and check a verified config body.
pub fn wording_from_config(mut value: Value) -> Result<Wording, String> {
    let Some(object) = value.as_object_mut() else {
        return Err("node capability wording is not a mapping".into());
    };
    for envelope in ["category", "version", "description"] {
        object.remove(envelope);
    }
    let wording: Wording = serde_json::from_value(value)
        .map_err(|error| format!("node capability wording: {error}"))?;
    for entry in wording.entries() {
        if entry.label.trim().is_empty()
            || entry.detail.as_deref().is_some_and(|d| d.trim().is_empty())
        {
            return Err("node capability wording has an empty sentence".into());
        }
        if !TONES.contains(&entry.tone.as_str()) {
            return Err(format!(
                "node capability wording has an unknown tone {}",
                entry.tone
            ));
        }
    }
    Ok(wording)
}

/// Resolve and verify the signed wording.
pub fn load_wording(state: &AppState) -> Result<Value> {
    let value = super::ui_attention::load_signed_config(state, CONFIG_PATH, CONFIG_REF)?;
    Ok(response_from_config(value).map_err(HandlerError::Internal)?)
}

pub async fn handle_wording(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let _request: CapabilitiesRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid request: {error}")))?;
    load_wording(&state)
}

pub const WORDING_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/node/capabilities/wording",
    endpoint: "ui.ryeos.node.capabilities.wording",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle_wording(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped() -> Value {
        let path = ryeos_engine::test_support::workspace_root()
            .join("bundles/ryeos-ui/.ai/config/ryeos/ui/node-capability-wording.yaml");
        serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn the_shipped_wording_is_a_readable_response() {
        let response = response_from_config(shipped()).unwrap();
        assert_eq!(response["schema_version"], WORDING_CONTRACT);
        assert_eq!(
            response["isolation_modes"]["disabled"]["label"],
            "Trusted, not contained"
        );
    }

    #[test]
    fn a_bad_tone_an_empty_sentence_a_missing_table_or_a_stray_field_is_refused() {
        let mut bad_tone = shipped();
        bad_tone["isolation_modes"]["disabled"]["tone"] = json!("loud");
        assert!(response_from_config(bad_tone).is_err());
        let mut empty = shipped();
        empty["host_classes"]["hosted"]["label"] = json!("  ");
        assert!(response_from_config(empty).is_err());
        let mut empty_detail = shipped();
        empty_detail["isolation_modes"]["enforce"]["detail"] = json!("");
        assert!(response_from_config(empty_detail).is_err());
        let mut stray = shipped();
        stray["extra"] = json!(true);
        assert!(response_from_config(stray).is_err());
        let mut stray_entry = shipped();
        stray_entry["fallback"]["colour"] = json!("red");
        assert!(response_from_config(stray_entry).is_err());
        let mut missing_table = shipped();
        missing_table.as_object_mut().unwrap().remove("grades");
        assert!(response_from_config(missing_table).is_err());
        assert!(response_from_config(json!([])).is_err());
    }

    /// Every table entry of the shipped wording has a tone from the closed
    /// list, and the config reads as a response.
    #[test]
    fn the_shipped_wording_tones_are_all_closed_codes() {
        let wording = wording_from_config(shipped()).unwrap();
        assert!(response_from_config(shipped()).is_ok());
        for entry in wording.entries() {
            assert!(TONES.contains(&entry.tone.as_str()), "{}", entry.tone);
        }
        // The host classes the shipped isolation profiles declare.
        for class in ["hosted", "local", "local_unconfined"] {
            assert!(wording.host_classes.contains_key(class), "{class}");
        }
        // The guarantees a hosted node and a contained node both report on.
        for name in [
            "isolation:namespaces",
            "limits:scope",
            "accounting",
            "residency:locked",
            "residency:swap_limit",
            "residency:swap_absent",
            "storage:durable_root",
            "credential:private_home",
        ] {
            assert!(wording.guarantees.contains_key(name), "{name}");
        }
    }

    #[test]
    fn the_shipped_wording_covers_every_coverage_word_and_verify_code_and_a_missing_one_reads_not_known()
     {
        let wording = wording_from_config(shipped()).unwrap();
        for word in [
            "present",
            "not_reported",
            "not_a_map",
            "key_field_collision",
        ] {
            assert!(wording.coverage_states.contains_key(word), "{word}");
        }
        for status in ["verified", "failed", "unverified_kind"] {
            assert!(wording.verify_states.contains_key(status), "{status}");
        }
        for reason in [
            "absent",
            "unreadable",
            "oversized",
            "stale_format",
            "digest_mismatch",
            "wrong_signer",
            "bad_signature",
            "host_restarted",
            "policy_generation_changed",
            "policy_generation_unreadable",
            "stale",
        ] {
            assert!(wording.verify_reasons.contains_key(reason), "{reason}");
        }
        // A code no table lists reads as the fallback, which says "not known".
        assert!(
            wording.fallback.label.to_lowercase().contains("not known"),
            "{}",
            wording.fallback.label
        );
    }
}
