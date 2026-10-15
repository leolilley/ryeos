//! The lease admission request a lifecycle adapter judges before spend, and
//! the verdict it returns.
//!
//! Workload: a placed session whose structured-session profile declares
//! what a remote lease provider and its guest must attest. The controller
//! asks the binding's lifecycle adapter, offline, whether its signed
//! provider spec can meet those entries before it reserves or creates
//! anything. The adapter owns the comparison; this module only assembles the
//! adapter's request from typed controller inputs and reads back the
//! verdict's identity fields and plain refusal reason. The admitted request
//! bytes are then retained verbatim on the allocation and handed back to the
//! adapter on every later lifecycle contact, so the controller never
//! reinterprets them.

use std::collections::BTreeSet;

use anyhow::{Context as _, Result, bail, ensure};
use lillux::process_control::CapabilityRequirement;
use serde::Serialize;

/// The adapter's lease request schema this controller writes.
pub(crate) const LEASE_ADMISSION_SCHEMA: u32 = 3;
/// The longest refusal reason surfaced to the caller.
const MAX_REFUSAL_REASON_BYTES: usize = 4096;

/// What the controller supplies for one lease, all from signed data: the
/// workload's requirement lists from its admitted profile, the host the
/// guest must reach from the binding's controller transport, the lifetime
/// from the binding's execution timeout (already checked against the node
/// policy's lease lifetime ceiling) and the activation deadline from the
/// admitted session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaseAdmissionInputs {
    pub(crate) workload: String,
    pub(crate) requires_before_spend: Vec<CapabilityRequirement>,
    pub(crate) requires_from_guest: Vec<CapabilityRequirement>,
    pub(crate) controller_host: String,
    pub(crate) maximum_lifetime_seconds: u32,
    pub(crate) activation_deadline_seconds: u32,
}

impl LeaseAdmissionInputs {
    /// The host part of the binding's HTTPS controller origin, the one host
    /// a lease guest may reach. A controller named by an IP address is
    /// refused: a provider egress allow-list names DNS hosts.
    pub(crate) fn controller_host_of(https_origin: &str) -> Result<String> {
        let origin = url::Url::parse(https_origin)
            .context("lease admission: the binding's controller origin is not a URL")?;
        match origin.host() {
            Some(url::Host::Domain(host)) if origin.scheme() == "https" => {
                Ok(host.to_ascii_lowercase())
            }
            _ => bail!(
                "lease admission refused: the binding's controller origin {https_origin} names \
                 no HTTPS DNS host for the lease guest's egress allow-list"
            ),
        }
    }
}

/// The adapter's request shape. Field names and their order match the
/// adapter's strict reader, which re-serializes what it reads in its own
/// field order and refuses any request whose bytes differ from that form. No
/// clock skew is declared, so the adapter's default applies.
#[derive(Serialize)]
struct LeaseAdmissionRequest<'a> {
    schema: u32,
    adapter_id: &'a str,
    provider_spec_sha256: &'a str,
    workload: &'a str,
    requires_before_spend: &'a [CapabilityRequirement],
    requires_from_guest: &'a [CapabilityRequirement],
    guest_network_allow: BTreeSet<&'a str>,
    maximum_lifetime_seconds: u32,
    activation_deadline_seconds: u32,
}

/// Canonical request bytes for one lease, bound to the exact adapter and
/// captured provider spec that will judge it.
pub(crate) fn canonical_request(
    adapter_id: &str,
    provider_spec_sha256: &str,
    inputs: &LeaseAdmissionInputs,
) -> Result<Vec<u8>> {
    ensure!(
        inputs.maximum_lifetime_seconds > 0 && inputs.activation_deadline_seconds > 0,
        "lease admission lifetime or activation deadline is zero"
    );
    let request = LeaseAdmissionRequest {
        schema: LEASE_ADMISSION_SCHEMA,
        adapter_id,
        provider_spec_sha256,
        workload: &inputs.workload,
        requires_before_spend: &inputs.requires_before_spend,
        requires_from_guest: &inputs.requires_from_guest,
        guest_network_allow: BTreeSet::from([inputs.controller_host.as_str()]),
        maximum_lifetime_seconds: inputs.maximum_lifetime_seconds,
        activation_deadline_seconds: inputs.activation_deadline_seconds,
    };
    let bytes = ryeos_external_execution_contract::canonical_json(&request)?;
    ensure!(
        bytes.len() <= ryeos_external_execution_contract::MAX_LEASE_ADMISSION_BYTES,
        "lease admission request exceeds its bound"
    );
    Ok(bytes)
}

/// Accept the adapter's verdict on exactly `request`, or refuse with its
/// plain reason. A verdict about another request or another provider spec is
/// no verdict.
pub(crate) fn require_admitted(
    response: &[u8],
    request: &[u8],
    provider_spec_sha256: &str,
    adapter_id: &str,
) -> Result<()> {
    let verdict: serde_json::Value = ryeos_external_execution_contract::from_json_slice_strict(
        response,
        ryeos_external_execution_contract::MAX_LIFECYCLE_RESPONSE_BYTES,
    )
    .with_context(|| format!("lifecycle adapter `{adapter_id}` returned no lease verdict"))?;
    let field = |name: &str| verdict.get(name).and_then(serde_json::Value::as_str);
    ensure!(
        field("request_sha256") == Some(lillux::sha256_hex(request).as_str())
            && field("provider_spec_sha256") == Some(provider_spec_sha256)
            && verdict.get("schema").and_then(serde_json::Value::as_u64)
                == Some(u64::from(LEASE_ADMISSION_SCHEMA)),
        "lifecycle adapter `{adapter_id}` answered lease admission for a different request"
    );
    match field("verdict") {
        Some("admitted") => Ok(()),
        Some("refused") => bail!(
            "lease admission refused before any provider contact by lifecycle adapter \
             `{adapter_id}`: {}",
            plain_reason(field("reason").unwrap_or("no reason given"))
        ),
        _ => bail!("lifecycle adapter `{adapter_id}` returned an unknown lease verdict"),
    }
}

/// The adapter holds no credential while it judges a lease, but its reason
/// still reaches operators verbatim; keep it printable and bounded.
fn plain_reason(reason: &str) -> String {
    let mut plain = String::new();
    for character in reason.chars() {
        let character = if character.is_control() { ' ' } else { character };
        if plain.len() + character.len_utf8() > MAX_REFUSAL_REASON_BYTES {
            plain.push('…');
            break;
        }
        plain.push(character);
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = "5f1e1b0f6b5a4c3d2e1f00112233445566778899aabbccddeeff001122334455";

    fn requirement(value: serde_json::Value) -> CapabilityRequirement {
        serde_json::from_value(value).unwrap()
    }

    fn inputs() -> LeaseAdmissionInputs {
        LeaseAdmissionInputs {
            workload: "codex.authoring.remote".into(),
            requires_before_spend: vec![
                requirement(serde_json::json!({
                    "name": "lease.network_policy", "accept": ["provider_attested"]
                })),
                requirement(serde_json::json!("time")),
            ],
            requires_from_guest: vec![requirement(serde_json::json!({
                "name": "guest.exec_route_closed", "accept": ["provider_attested"]
            }))],
            controller_host: "controller.example.com".into(),
            maximum_lifetime_seconds: 14_400,
            activation_deadline_seconds: 300,
        }
    }

    #[test]
    fn request_is_the_adapters_exact_canonical_shape() {
        let bytes = canonical_request("render-sandbox", SPEC, &inputs()).unwrap();
        // The adapter re-serializes the request in its own field order, with
        // each requirement entry in Lillux's field order, and must get these
        // exact bytes back.
        let expected = format!(
            "{{\"schema\":3,\"adapter_id\":\"render-sandbox\",\
             \"provider_spec_sha256\":\"{SPEC}\",\"workload\":\"codex.authoring.remote\",\
             \"requires_before_spend\":[{{\"name\":\"lease.network_policy\",\
             \"accept\":[\"provider_attested\"]}},\"time\"],\
             \"requires_from_guest\":[{{\"name\":\"guest.exec_route_closed\",\
             \"accept\":[\"provider_attested\"]}}],\
             \"guest_network_allow\":[\"controller.example.com\"],\
             \"maximum_lifetime_seconds\":14400,\"activation_deadline_seconds\":300}}"
        );
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), expected);
        assert_eq!(
            canonical_request("render-sandbox", SPEC, &inputs()).unwrap(),
            bytes,
            "the same inputs always yield the same retained bytes"
        );
    }

    #[test]
    fn the_guest_may_reach_only_the_bindings_controller_host() {
        assert_eq!(
            LeaseAdmissionInputs::controller_host_of("https://Controller.Example.com").unwrap(),
            "controller.example.com"
        );
        assert_eq!(
            LeaseAdmissionInputs::controller_host_of("https://controller.example:7443").unwrap(),
            "controller.example"
        );
        for refused in ["https://192.0.2.1", "http://controller.example.com", "relay"] {
            assert!(
                LeaseAdmissionInputs::controller_host_of(refused).is_err(),
                "{refused}"
            );
        }
    }

    fn verdict(request: &[u8], verdict: &str, extra: serde_json::Value) -> Vec<u8> {
        let mut value = serde_json::json!({
            "verdict": verdict,
            "schema": 3,
            "workload": "codex.authoring.remote",
            "request_sha256": lillux::sha256_hex(request),
            "provider_spec_sha256": SPEC,
        });
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn an_admitted_verdict_must_name_the_exact_request_and_spec() {
        let request = canonical_request("render-sandbox", SPEC, &inputs()).unwrap();
        let admitted = verdict(&request, "admitted", serde_json::json!({"met_by": {}}));
        require_admitted(&admitted, &request, SPEC, "render-sandbox").unwrap();

        let other = canonical_request(
            "render-sandbox",
            SPEC,
            &LeaseAdmissionInputs {
                activation_deadline_seconds: 301,
                ..inputs()
            },
        )
        .unwrap();
        let error = require_admitted(&admitted, &other, SPEC, "render-sandbox").unwrap_err();
        assert!(error.to_string().contains("different request"), "{error:#}");
        let other_spec = "0".repeat(64);
        assert!(require_admitted(&admitted, &request, &other_spec, "render-sandbox").is_err());
        assert!(require_admitted(b"not json", &request, SPEC, "render-sandbox").is_err());
        let unknown = verdict(&request, "maybe", serde_json::json!({}));
        assert!(require_admitted(&unknown, &request, SPEC, "render-sandbox").is_err());
    }

    #[test]
    fn a_stage_one_refusal_is_plain_and_names_its_adapter() {
        let request = canonical_request("render-sandbox", SPEC, &inputs()).unwrap();
        let refused = verdict(
            &request,
            "refused",
            serde_json::json!({
                "unmet": {"guest.ready_authenticated": "unattested"},
                "reason": "`codex.authoring.remote` requires `guest.ready_authenticated`:\n\
                           provider spec render_sandbox_v1 declares guest_channel \
                           unsupported_pending"
            }),
        );
        let text = require_admitted(&refused, &request, SPEC, "render-sandbox")
            .unwrap_err()
            .to_string();
        assert!(text.starts_with("lease admission refused before any provider contact"));
        assert!(text.contains("`render-sandbox`"), "{text}");
        assert!(text.contains("guest_channel unsupported_pending"), "{text}");
        assert!(!text.contains('\n'), "{text}");
        assert!(plain_reason(&"x".repeat(MAX_REFUSAL_REASON_BYTES * 2)).len() < 4100);
    }
}
