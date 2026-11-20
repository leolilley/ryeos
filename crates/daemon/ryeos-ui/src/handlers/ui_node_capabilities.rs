//! `ui.ryeos.node.capabilities` — what this node contains and proves, as
//! facts, and `ui.ryeos.node.capabilities.wording` — the signed words for
//! those facts.
//!
//! The facts come from the node's signed capability report, the one launch
//! admission judges: it is read back, its digest and this node's signature
//! verified, and refused unless it still describes the running node. The
//! response carries the report's signer, digest and the time the node
//! measured it, its host class, its proof rows (each with the grade,
//! provider and basis the report recorded, or the reason it records for a
//! missing one) and whether its floor was met. The posture is the report's own
//! `isolation:enforced` row; nothing is recomputed from `node/status`. The
//! caller's granted capabilities ride along unchanged. The response holds
//! codes and values only; it holds no sentence of this module's own.
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
    NodeReportFacts, current_node_report_facts, is_report_unreadable,
};
use ryeos_app::state::AppState;
use ryeos_engine::isolation::ISOLATION_ENFORCED;
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
/// Every proof state the facts service can emit.
pub const PROOF_STATES: [&str; 2] = ["provided", "missing"];
/// Every proof grade the report grades a proof with.
pub const PROOF_GRADES: [&str; 4] = ["measured", "provider_attested", "peer_attested", "trusted"];

/// What reading the node's signed capability report came to.
pub enum ReportOutcome {
    /// The report stands.
    Present(Box<NodeReportFacts>),
    /// This node was composed with no report reader.
    NotComposed,
    /// A report is missing, stale, or does not verify; nothing this node does
    /// while it runs changes that. The text is the node's reason.
    Refused(String),
    /// The report could not be read at this moment.
    Unreadable(String),
}

/// Everything the facts are made from.
pub struct FactInputs<'a> {
    pub report: ReportOutcome,
    pub granted: &'a [String],
}

/// The facts response for `inputs`.
pub fn facts(inputs: FactInputs<'_>) -> Value {
    let mut response = json!({
        "schema_version": FACTS_SCHEMA,
        "host": null,
        "isolation": { "mode": null },
        "floor": null,
        "proofs": [],
        "granted": inputs.granted,
    });
    let report = match &inputs.report {
        ReportOutcome::Present(report) => {
            response["host"] = json!({
                "class": report.host_class,
                "profile": report.profile,
                "kernel": report.kernel_kind,
            });
            response["isolation"]["mode"] = posture(&report.capabilities);
            response["floor"] = json!({
                "admitted": report.floor_admitted,
                "unmet": report.floor_unmet,
            });
            response["proofs"] = json!(proofs(&report.capabilities));
            json!({
                "state": "present",
                "verified": true,
                "signer": report.signer,
                "digest": report.digest,
                "measured_at": report.generated_at,
            })
        }
        ReportOutcome::NotComposed => json!({ "state": "not_composed" }),
        ReportOutcome::Refused(reason) => json!({ "state": "refused", "reason": reason }),
        ReportOutcome::Unreadable(reason) => json!({ "state": "unreadable", "reason": reason }),
    };
    response["report"] = report;
    response
}

/// The report's own isolation decision as a code: `enforce` when it records
/// `isolation:enforced` as provided, `disabled` when it records it as
/// refused, and nothing when it records neither.
fn posture(view: &HostCapabilities) -> Value {
    if view.provided.contains_key(ISOLATION_ENFORCED) {
        json!("enforce")
    } else if view.missing.contains_key(ISOLATION_ENFORCED) {
        json!("disabled")
    } else {
        Value::Null
    }
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
    match current_node_report_facts(state) {
        None => ReportOutcome::NotComposed,
        Some(Ok(report)) => ReportOutcome::Present(Box::new(report)),
        Some(Err(error)) if is_report_unreadable(&error) => {
            ReportOutcome::Unreadable(format!("{error:#}"))
        }
        Some(Err(error)) => ReportOutcome::Refused(format!("{error:#}")),
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
        facts(FactInputs {
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
    use ryeos_app::node_requirement_admission::NodeReportFacts;

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

    fn report(
        host_class: &str,
        provided: &[(&str, ProofSource)],
        missing: &[(&str, &str)],
    ) -> ReportOutcome {
        ReportOutcome::Present(Box::new(NodeReportFacts {
            signer: "fp:node".into(),
            digest: "sha256:abc".into(),
            generated_at: "2026-11-20T00:00:00Z".into(),
            host_class: host_class.into(),
            profile: Some("core".into()),
            kernel_kind: "native".into(),
            floor_admitted: missing.is_empty(),
            floor_unmet: missing
                .iter()
                .map(|(name, reason)| ((*name).to_owned(), (*reason).to_owned()))
                .collect(),
            capabilities: view(provided, missing),
        }))
    }

    #[test]
    fn a_contained_local_node_reports_its_measured_proofs_and_signer() {
        let granted = granted();
        let value = facts(FactInputs {
            report: report(
                "local",
                &[
                    ("isolation:enforced", ProofSource::Measured),
                    ("isolation:namespaces", ProofSource::Measured),
                ],
                &[],
            ),
            granted: &granted,
        });
        assert_eq!(value["schema_version"], FACTS_SCHEMA);
        assert_eq!(value["report"]["state"], "present");
        assert_eq!(value["report"]["verified"], true);
        assert_eq!(value["report"]["signer"], "fp:node");
        assert_eq!(value["report"]["digest"], "sha256:abc");
        assert_eq!(value["report"]["measured_at"], "2026-11-20T00:00:00Z");
        assert_eq!(value["host"]["class"], "local");
        assert_eq!(value["host"]["kernel"], "native");
        assert_eq!(value["isolation"]["mode"], "enforce");
        assert_eq!(value["floor"]["admitted"], true);
        assert_eq!(value["proofs"][1]["name"], "isolation:namespaces");
        assert_eq!(value["proofs"][1]["state"], "provided");
        assert_eq!(value["proofs"][1]["grade"], "measured");
        assert_eq!(value["granted"], json!(granted));
    }

    #[test]
    fn a_hosted_node_that_is_not_contained_names_what_it_lacks() {
        let value = facts(FactInputs {
            report: report(
                "hosted",
                &[("storage:durable_root", ProofSource::ProviderAttested)],
                &[
                    (
                        "isolation:enforced",
                        "isolation is not enforced on this node",
                    ),
                    ("isolation:namespaces", "the host gives no namespace rights"),
                ],
            ),
            granted: &[],
        });
        assert_eq!(value["host"]["class"], "hosted");
        assert_eq!(value["isolation"]["mode"], "disabled");
        assert_eq!(value["floor"]["admitted"], false);
        assert_eq!(
            value["floor"]["unmet"]["isolation:namespaces"],
            "the host gives no namespace rights"
        );
        let proofs = value["proofs"].as_array().unwrap();
        assert_eq!(proofs.len(), 3);
        assert_eq!(proofs[1]["name"], "isolation:namespaces");
        assert_eq!(proofs[1]["state"], "missing");
        assert_eq!(proofs[2]["grade"], "provider_attested");
        assert_eq!(value["granted"], json!([]));
    }

    #[test]
    fn a_report_that_says_nothing_of_enforcement_carries_no_posture() {
        let value = facts(FactInputs {
            report: report("local_unconfined", &[], &[]),
            granted: &[],
        });
        assert_eq!(value["host"]["class"], "local_unconfined");
        assert_eq!(value["isolation"]["mode"], Value::Null);
        assert_eq!(value["proofs"], json!([]));
    }

    #[test]
    fn a_report_that_does_not_stand_carries_no_host_or_proofs() {
        for (outcome, state, reason) in [
            (ReportOutcome::NotComposed, "not_composed", None),
            (
                ReportOutcome::Refused("stale".into()),
                "refused",
                Some("stale"),
            ),
            (
                ReportOutcome::Unreadable("io".into()),
                "unreadable",
                Some("io"),
            ),
        ] {
            let value = facts(FactInputs {
                report: outcome,
                granted: &[],
            });
            assert_eq!(value["report"]["state"], state);
            assert_eq!(
                value["report"].get("reason").and_then(Value::as_str),
                reason
            );
            assert_eq!(value["host"], Value::Null);
            assert_eq!(value["proofs"], json!([]));
        }
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
