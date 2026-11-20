//! `ui.ryeos.node.capabilities` — what this node contains and proves, as
//! facts, and `ui.ryeos.node.capabilities.wording` — the signed words for
//! those facts.
//!
//! The facts service reads the node's own sources and recomputes nothing: the
//! isolation mode, network mode and host class come from the isolation policy
//! the node is running under (the same snapshot `node/status` reports), and
//! the proofs come from the node's signed capability report as the launch
//! admission reader returns it, each with the grade, provider and basis the
//! report recorded or the reason it records for a missing one. The caller's
//! granted capabilities ride along unchanged. The response carries codes and
//! values only; it holds no sentence.
//!
//! The sentence and tone for each code come from the signed config
//! `config:ryeos/ui/node-capability-wording`, served by the wording service.
//! A client resolves a code against that config and falls back to the
//! config's own fallback for a code it does not list, so a new host class,
//! proof name or grant needs a config edit and no code change.
//!
//! This module decides no meaning. The wording handler verifies the config,
//! refuses an unknown field, an empty sentence or a tone outside the closed
//! list, and returns the tables as the config wrote them.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use lillux::process_control::HostCapabilities;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::node_requirement_admission::{
    NodeCapabilitySource, current_node_capabilities, is_report_unreadable,
};
use ryeos_app::state::AppState;
use ryeos_engine::isolation::{IsolationMode, IsolationNetworkMode};
use ryeos_executor::executor::ServiceAvailability;

pub const FACTS_SCHEMA: &str = "ryeos.ui.node.capabilities.v1";
pub const WORDING_CONTRACT: &str = "ryeos.ui.node_capability_wording.v1";
pub const CONFIG_REF: &str = "config:ryeos/ui/node-capability-wording";
const CONFIG_PATH: &str = "ryeos/ui/node-capability-wording.yaml";

/// The tones a wording entry may name.
pub const TONES: [&str; 5] = ["none", "good", "warn", "danger", "accent"];

/// Every report state the facts service can emit.
pub const REPORT_STATES: [&str; 4] = ["present", "not_composed", "refused", "unreadable"];
/// Every isolation mode the facts service can emit.
pub const ISOLATION_MODES: [&str; 2] = ["enforce", "disabled"];
/// Every network mode the facts service can emit.
pub const NETWORK_MODES: [&str; 2] = ["host", "isolated"];
/// Every proof state the facts service can emit.
pub const PROOF_STATES: [&str; 2] = ["provided", "missing"];
/// Every proof grade the report grades a proof with.
pub const PROOF_GRADES: [&str; 4] = ["measured", "provider_attested", "peer_attested", "trusted"];

/// What reading the node's signed capability report came to.
pub enum ReportOutcome {
    /// The report stands and these are its proofs.
    Present(HostCapabilities),
    /// This node was composed with no report reader.
    NotComposed,
    /// A report is missing, stale, or does not verify; nothing this node does
    /// while it runs changes that.
    Refused,
    /// The report could not be read at this moment.
    Unreadable,
}

/// Everything the facts are made from.
pub struct FactInputs<'a> {
    pub host_class: &'a str,
    pub isolation_mode: IsolationMode,
    pub network_mode: IsolationNetworkMode,
    pub report: ReportOutcome,
    pub granted: &'a [String],
}

fn code<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// The facts response for `inputs`.
pub fn facts(inputs: FactInputs<'_>) -> Value {
    let (report_state, proofs) = match &inputs.report {
        ReportOutcome::Present(view) => ("present", proofs(view)),
        ReportOutcome::NotComposed => ("not_composed", Vec::new()),
        ReportOutcome::Refused => ("refused", Vec::new()),
        ReportOutcome::Unreadable => ("unreadable", Vec::new()),
    };
    json!({
        "schema_version": FACTS_SCHEMA,
        "report": { "state": report_state },
        "host": { "class": inputs.host_class },
        "isolation": {
            "mode": code(&inputs.isolation_mode),
            "network_mode": code(&inputs.network_mode),
        },
        "proofs": proofs,
        "granted": inputs.granted,
    })
}

/// One row per capability name the report records, sorted by name.
fn proofs(view: &HostCapabilities) -> Vec<Value> {
    let mut rows: BTreeMap<&str, Value> = BTreeMap::new();
    for (name, reason) in &view.missing {
        rows.insert(
            name,
            json!({ "name": name, "state": "missing", "reason": reason }),
        );
    }
    for (name, evidence) in &view.provided {
        rows.insert(
            name,
            json!({
                "name": name,
                "state": "provided",
                "grade": evidence.source.as_str(),
                "provider": evidence.provider,
                "basis": evidence.basis,
            }),
        );
    }
    rows.into_values().collect()
}

fn read_report(state: &AppState) -> ReportOutcome {
    if state.node_capability_report_reader().is_none() {
        return ReportOutcome::NotComposed;
    }
    match current_node_capabilities(state) {
        Ok(view) => ReportOutcome::Present(view),
        Err(error) if is_report_unreadable(&error) => ReportOutcome::Unreadable,
        Err(_) => ReportOutcome::Refused,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesRequest {}

pub async fn handle_facts(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let _request: CapabilitiesRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid request: {error}")))?;
    let granted = ctx.scopes.clone();
    let value = tokio::task::spawn_blocking(move || {
        let inspection = state.isolation.inspection();
        facts(FactInputs {
            host_class: &inspection.host_class,
            isolation_mode: state.isolation.mode(),
            network_mode: inspection.network.mode,
            report: read_report(&state),
            granted: &granted,
        })
    })
    .await
    .map_err(|error| HandlerError::Internal(format!("read node capabilities: {error}")))?;
    Ok(value)
}

pub const FACTS_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/node/capabilities",
    endpoint: "ui.ryeos.node.capabilities",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle_facts(params, ctx, state).await }),
};

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
    pub network_modes: BTreeMap<String, Entry>,
    pub host_classes: BTreeMap<String, Entry>,
    pub proof_states: BTreeMap<String, Entry>,
    pub grades: BTreeMap<String, Entry>,
    pub guarantees: BTreeMap<String, Entry>,
    pub grants: BTreeMap<String, Entry>,
}

impl Wording {
    fn entries(&self) -> impl Iterator<Item = &Entry> {
        std::iter::once(&self.fallback).chain(
            [
                &self.report_states,
                &self.isolation_modes,
                &self.network_modes,
                &self.host_classes,
                &self.proof_states,
                &self.grades,
                &self.guarantees,
                &self.grants,
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
    use lillux::process_control::{CapabilityEvidence, ProofSource};

    fn shipped() -> Value {
        let path = ryeos_engine::test_support::workspace_root()
            .join("bundles/ryeos-ui/.ai/config/ryeos/ui/node-capability-wording.yaml");
        serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn view(provided: &[(&str, ProofSource)], missing: &[(&str, &str)]) -> HostCapabilities {
        let mut view = HostCapabilities::default();
        for (name, source) in provided {
            view.provided.insert(
                (*name).to_owned(),
                CapabilityEvidence {
                    source: source.clone(),
                    provider: "host".into(),
                    basis: "probed".into(),
                },
            );
        }
        for (name, reason) in missing {
            view.missing
                .insert((*name).to_owned(), (*reason).to_owned());
        }
        view
    }

    fn granted() -> Vec<String> {
        vec!["ryeos.execute.service.projects/open".to_owned()]
    }

    #[test]
    fn a_contained_local_node_reports_its_measured_proofs() {
        let granted = granted();
        let value = facts(FactInputs {
            host_class: "local",
            isolation_mode: IsolationMode::Enforce,
            network_mode: IsolationNetworkMode::Isolated,
            report: ReportOutcome::Present(view(
                &[("isolation:namespaces", ProofSource::Measured)],
                &[],
            )),
            granted: &granted,
        });
        assert_eq!(value["schema_version"], FACTS_SCHEMA);
        assert_eq!(value["report"]["state"], "present");
        assert_eq!(value["host"]["class"], "local");
        assert_eq!(value["isolation"]["mode"], "enforce");
        assert_eq!(value["isolation"]["network_mode"], "isolated");
        assert_eq!(value["proofs"][0]["name"], "isolation:namespaces");
        assert_eq!(value["proofs"][0]["state"], "provided");
        assert_eq!(value["proofs"][0]["grade"], "measured");
        assert_eq!(value["granted"], json!(granted));
    }

    #[test]
    fn a_hosted_node_that_is_not_contained_names_what_it_lacks() {
        let value = facts(FactInputs {
            host_class: "hosted",
            isolation_mode: IsolationMode::Disabled,
            network_mode: IsolationNetworkMode::Host,
            report: ReportOutcome::Present(view(
                &[("storage:durable_root", ProofSource::ProviderAttested)],
                &[("isolation:namespaces", "the host gives no namespace rights")],
            )),
            granted: &[],
        });
        assert_eq!(value["host"]["class"], "hosted");
        assert_eq!(value["isolation"]["mode"], "disabled");
        assert_eq!(value["isolation"]["network_mode"], "host");
        let proofs = value["proofs"].as_array().unwrap();
        assert_eq!(proofs.len(), 2);
        assert_eq!(proofs[0]["name"], "isolation:namespaces");
        assert_eq!(proofs[0]["state"], "missing");
        assert_eq!(proofs[0]["reason"], "the host gives no namespace rights");
        assert_eq!(proofs[1]["grade"], "provider_attested");
        assert_eq!(value["granted"], json!([]));
    }

    #[test]
    fn an_unconfined_local_node_is_a_different_host_class_with_the_same_mode_code() {
        let value = facts(FactInputs {
            host_class: "local_unconfined",
            isolation_mode: IsolationMode::Disabled,
            network_mode: IsolationNetworkMode::Host,
            report: ReportOutcome::Present(HostCapabilities::default()),
            granted: &[],
        });
        assert_eq!(value["host"]["class"], "local_unconfined");
        assert_eq!(value["isolation"]["mode"], "disabled");
        assert_eq!(value["proofs"], json!([]));
    }

    #[test]
    fn a_report_that_does_not_stand_carries_no_proofs() {
        for (outcome, state) in [
            (ReportOutcome::NotComposed, "not_composed"),
            (ReportOutcome::Refused, "refused"),
            (ReportOutcome::Unreadable, "unreadable"),
        ] {
            let value = facts(FactInputs {
                host_class: "hosted",
                isolation_mode: IsolationMode::Disabled,
                network_mode: IsolationNetworkMode::Host,
                report: outcome,
                granted: &[],
            });
            assert_eq!(value["report"]["state"], state);
            assert_eq!(value["proofs"], json!([]));
        }
    }

    #[test]
    fn the_facts_hold_no_sentence_the_wording_could_replace() {
        let value = facts(FactInputs {
            host_class: "hosted",
            isolation_mode: IsolationMode::Disabled,
            network_mode: IsolationNetworkMode::Host,
            report: ReportOutcome::Refused,
            granted: &[],
        });
        assert!(value.get("label").is_none() && value.get("wording").is_none());
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

    /// The shipped wording words every closed code the facts service can emit,
    /// so a code added there without a sentence here fails this test.
    #[test]
    fn the_shipped_wording_covers_every_code_the_facts_service_can_emit() {
        let wording = wording_from_config(shipped()).unwrap();
        for (table, codes, name) in [
            (&wording.report_states, &REPORT_STATES[..], "report_states"),
            (
                &wording.isolation_modes,
                &ISOLATION_MODES[..],
                "isolation_modes",
            ),
            (&wording.network_modes, &NETWORK_MODES[..], "network_modes"),
            (&wording.proof_states, &PROOF_STATES[..], "proof_states"),
            (&wording.grades, &PROOF_GRADES[..], "grades"),
        ] {
            for code in codes {
                assert!(
                    table.contains_key(*code),
                    "{name} has no sentence for {code}"
                );
            }
        }
        // The emitted sets are the types' own serialised forms.
        for (mode, expected) in [
            (IsolationMode::Enforce, "enforce"),
            (IsolationMode::Disabled, "disabled"),
        ] {
            assert!(ISOLATION_MODES.contains(&expected));
            assert_eq!(code(&mode), expected);
        }
        for (mode, expected) in [
            (IsolationNetworkMode::Host, "host"),
            (IsolationNetworkMode::Isolated, "isolated"),
        ] {
            assert!(NETWORK_MODES.contains(&expected));
            assert_eq!(code(&mode), expected);
        }
        for source in [
            ProofSource::Measured,
            ProofSource::ProviderAttested,
            ProofSource::PeerAttested,
            ProofSource::Trusted,
        ] {
            assert!(PROOF_GRADES.contains(&source.as_str()));
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
}
