//! Node selection among the protocols an item admits.
//!
//! An item may admit several protocols, each declaring the holding a node
//! needs to run it (`ProtocolHolding`). The running node picks the one whose
//! required guarantees its own report meets. The selection reads only the
//! descriptors and the node's guarantees, never the item or its kind, and does
//! not depend on the order the candidates are listed in.

use anyhow::{anyhow, bail, ensure};
use ryeos_state::node_guarantees::{NodeGuarantees, unmet_guarantees};

use super::descriptor::ProtocolHolding;
use super::registry::{ProtocolRegistry, VerifiedProtocol};

/// The protocol a node chose, with the holding it was chosen under. `holding`
/// is `None` only for a lone candidate that declares none, which asks nothing
/// of the node.
#[derive(Debug, Clone, Copy)]
pub struct SelectedProtocol<'a> {
    pub protocol: &'a VerifiedProtocol,
    pub holding: Option<&'a ProtocolHolding>,
}

struct Evaluated<'a> {
    protocol: &'a VerifiedProtocol,
    holding: &'a ProtocolHolding,
    unmet: Vec<(String, String)>,
}

/// The single candidate whose declared holding `node` meets.
///
/// One candidate that declares no holding is returned as is. Otherwise every
/// candidate must declare a holding and exactly one must be met: none and more
/// than one are both refused, and the refusal names every candidate with the
/// guarantees it requires that the node lacks.
pub fn select_protocol<'a>(
    candidates: &[String],
    registry: &'a ProtocolRegistry,
    node: NodeGuarantees<'_>,
) -> anyhow::Result<SelectedProtocol<'a>> {
    ensure!(
        !candidates.is_empty(),
        "no candidate protocols to select among"
    );
    let mut resolved: Vec<&'a VerifiedProtocol> = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        resolved.push(registry.require(candidate)?);
    }
    if let [only] = resolved.as_slice()
        && only.descriptor.holding.is_none()
    {
        return Ok(SelectedProtocol {
            protocol: only,
            holding: None,
        });
    }
    let mut evaluated: Vec<Evaluated<'a>> = Vec::with_capacity(resolved.len());
    for protocol in resolved {
        let holding = protocol.descriptor.holding.as_ref().ok_or_else(|| {
            anyhow!(
                "protocol {} declares no holding, so it cannot be selected among several candidates",
                protocol.canonical_ref
            )
        })?;
        evaluated.push(Evaluated {
            protocol,
            holding,
            unmet: unmet_guarantees(&holding.requires_guarantees, node),
        });
    }
    evaluated.sort_by(|a, b| a.protocol.canonical_ref.cmp(&b.protocol.canonical_ref));
    let met: Vec<&Evaluated<'a>> = evaluated
        .iter()
        .filter(|candidate| candidate.unmet.is_empty())
        .collect();
    match met.as_slice() {
        [one] => Ok(SelectedProtocol {
            protocol: one.protocol,
            holding: Some(one.holding),
        }),
        [] => bail!(
            "no candidate protocol's required guarantees are held by this node; candidates: {}",
            evaluated
                .iter()
                .map(|candidate| {
                    let unmet = candidate
                        .unmet
                        .iter()
                        .map(|(label, reason)| format!("{label}: {reason}"))
                        .collect::<Vec<_>>()
                        .join("; ");
                    format!("{} (unmet: {unmet})", candidate.protocol.canonical_ref)
                })
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        several => bail!(
            "more than one candidate protocol's required guarantees are held by this node: {}",
            several
                .iter()
                .map(|candidate| candidate.protocol.canonical_ref.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The protocol a node runs for an item's subprocess terminator, chosen from
/// the item's composed definition and the node's own reported guarantees.
///
/// An item that states one protocol resolves to exactly that protocol and asks
/// nothing further of the node here; the holding it declares, if any, rides
/// along for the launch checks that read it. An item that lists several
/// admissible protocols goes through `select_protocol`. `None` means the kind
/// has no subprocess terminator. The result is a node-local launch fact: it is
/// never part of the resolution output or the definition digest.
pub fn select_subprocess_protocol<'a>(
    execution: &crate::kind_registry::ExecutionSchema,
    composed: &serde_json::Value,
    registry: &'a ProtocolRegistry,
    isolation: &crate::isolation::IsolationRuntime,
) -> anyhow::Result<Option<SelectedProtocol<'a>>> {
    let Some(crate::kind_registry::TerminatorDecl::Subprocess { protocol }) =
        execution.terminator.as_ref()
    else {
        return Ok(None);
    };
    if !protocol.selects_from_set(composed) {
        let protocol_ref = protocol.resolve(composed).map_err(anyhow::Error::msg)?;
        let protocol = registry.require(&protocol_ref)?;
        return Ok(Some(SelectedProtocol {
            protocol,
            holding: protocol.descriptor.holding.as_ref(),
        }));
    }
    let candidates = protocol.candidates(composed).map_err(anyhow::Error::msg)?;
    let reported = isolation.reported_guarantees();
    select_protocol(
        &candidates,
        registry,
        NodeGuarantees::new(&reported, isolation.capability_floor()),
    )
    .map(Some)
}

/// Whether `node` still holds what a retained protocol's holding requires.
///
/// Recovery of an admitted launch pins the protocol it was admitted under: it
/// re-checks that one holding against the node's current report and never
/// selects among candidates again, so a node that lost the guarantee refuses
/// even when it now meets another candidate's.
pub fn require_holding_met(
    protocol_ref: &str,
    holding: &ProtocolHolding,
    node: NodeGuarantees<'_>,
) -> anyhow::Result<()> {
    let unmet = unmet_guarantees(&holding.requires_guarantees, node);
    ensure!(
        unmet.is_empty(),
        "this node no longer holds the guarantees retained protocol {protocol_ref} requires: {}",
        unmet
            .iter()
            .map(|(label, reason)| format!("{label}: {reason}"))
            .collect::<Vec<_>>()
            .join("; ")
    );
    Ok(())
}

/// The selection for an item that lists several admissible protocols, and
/// `None` for one that states a single protocol or has no subprocess
/// terminator. Launch checks that must keep the scalar path exactly as it was
/// call this instead of `select_subprocess_protocol`.
pub fn select_listed_protocol<'a>(
    execution: &crate::kind_registry::ExecutionSchema,
    composed: &serde_json::Value,
    registry: &'a ProtocolRegistry,
    isolation: &crate::isolation::IsolationRuntime,
) -> anyhow::Result<Option<SelectedProtocol<'a>>> {
    let lists_protocols = matches!(
        execution.terminator.as_ref(),
        Some(crate::kind_registry::TerminatorDecl::Subprocess { protocol })
            if protocol.selects_from_set(composed)
    );
    if !lists_protocols {
        return Ok(None);
    }
    select_subprocess_protocol(execution, composed, registry, isolation)
}

/// The filesystem-authority ceiling an item runs under on this node. A kind
/// without a protocol list projects exactly as
/// `ExecutionSchema::project_filesystem_authority_ceiling`; an item that lists
/// several admissible protocols takes the ceiling of the one this node selects.
pub fn project_node_filesystem_authority_ceiling(
    execution: &crate::kind_registry::ExecutionSchema,
    composed: &serde_json::Value,
    registry: &ProtocolRegistry,
    isolation: &crate::isolation::IsolationRuntime,
) -> anyhow::Result<crate::isolation::IsolationFilesystemAuthorityCeiling> {
    let selected = select_listed_protocol(execution, composed, registry, isolation)?;
    Ok(execution.project_filesystem_authority_ceiling_selected(
        composed,
        selected.as_ref().and_then(|selected| selected.holding),
    )?)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use lillux::process_control::{CapabilityRequirement, HostCapabilities, ProofSource};
    use serde_json::json;

    use super::*;
    use crate::protocols::ProtocolDescriptor;

    const CONTAINED: &str = "protocol:test/contained";
    const TRUSTED: &str = "protocol:test/trusted";
    const PLAIN: &str = "protocol:test/plain";

    fn verified(name: &str, holding: Option<serde_json::Value>) -> VerifiedProtocol {
        let mut value = json!({
            "kind": "protocol",
            "name": name,
            "category": "test",
            "abi_version": "v1",
            "stdin": {"shape": "opaque"},
            "stdout": {"shape": "opaque_bytes", "mode": "terminal"},
            "capabilities": {
                "allows_pushed_head": false,
                "allows_target_site": false,
                "allows_detached": false,
            },
            "lifecycle": {"mode": "managed"},
            "callback_channel": "none",
        });
        if let Some(holding) = holding {
            value["holding"] = holding;
        }
        let descriptor: ProtocolDescriptor = serde_json::from_value(value).unwrap();
        VerifiedProtocol {
            canonical_ref: format!("protocol:test/{name}"),
            raw_content_digest: "0".repeat(64),
            signer_fingerprint: "test".to_owned(),
            descriptor,
            trust_class: crate::resolution::TrustClass::TrustedBundle,
            bundle_root: PathBuf::new(),
            descriptor_path: PathBuf::new(),
        }
    }

    fn registry() -> ProtocolRegistry {
        ProtocolRegistry::from_verified(vec![
            verified(
                "contained",
                Some(json!({
                    "requires_guarantees": [
                        "isolation:enforced",
                        {"any_of": ["termination:scope", "termination:pid_namespace"]},
                    ],
                    "filesystem_authority": "captured_execution",
                    "root_settlement": "containment_empty",
                })),
            ),
            verified(
                "trusted",
                Some(json!({
                    "requires_guarantees": [
                        {"name": "sessions:trusted_process_group", "accept": ["trusted"]},
                    ],
                    "filesystem_authority": "node_policy",
                    "root_settlement": "trusted_process_group_absent",
                })),
            ),
            verified("plain", None),
        ])
    }

    fn node(held: &[(&str, ProofSource)]) -> HostCapabilities {
        let mut view = HostCapabilities::default();
        for (name, source) in held {
            view.provide(name, source.clone(), "test", "held");
        }
        for name in [
            "isolation:enforced",
            "termination:scope",
            "termination:pid_namespace",
            "sessions:trusted_process_group",
        ] {
            view.refuse(name, format!("{name} is not held"));
        }
        view
    }

    fn pair() -> Vec<String> {
        vec![CONTAINED.to_owned(), TRUSTED.to_owned()]
    }

    fn selected(
        candidates: &[String],
        node: &HostCapabilities,
        floor: &[CapabilityRequirement],
    ) -> anyhow::Result<String> {
        let registry = registry();
        select_protocol(candidates, &registry, NodeGuarantees::new(node, floor))
            .map(|selection| selection.protocol.canonical_ref.clone())
    }

    #[test]
    fn a_lone_holding_free_candidate_passes_through_on_any_node() {
        let node = node(&[]);
        let registry = registry();
        let selection = select_protocol(
            &[PLAIN.to_owned()],
            &registry,
            NodeGuarantees::new(&node, &[]),
        )
        .unwrap();
        assert_eq!(selection.protocol.canonical_ref, PLAIN);
        assert!(selection.holding.is_none());
    }

    #[test]
    fn a_lone_candidate_with_a_holding_must_still_be_met() {
        let held = node(&[("sessions:trusted_process_group", ProofSource::Trusted)]);
        assert_eq!(
            selected(&[TRUSTED.to_owned()], &held, &[]).unwrap(),
            TRUSTED
        );
        let error = selected(&[CONTAINED.to_owned()], &held, &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains(CONTAINED), "{error}");
    }

    #[test]
    fn several_candidates_each_need_a_holding() {
        let held = node(&[("sessions:trusted_process_group", ProofSource::Trusted)]);
        let error = selected(&[TRUSTED.to_owned(), PLAIN.to_owned()], &held, &[])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(PLAIN) && error.contains("no holding"),
            "{error}"
        );
    }

    #[test]
    fn an_unregistered_candidate_is_refused() {
        let held = node(&[]);
        assert!(selected(&["protocol:test/absent".to_owned()], &held, &[]).is_err());
    }

    #[test]
    fn an_enforced_contained_node_selects_the_contained_protocol() {
        let held = node(&[
            ("isolation:enforced", ProofSource::Measured),
            ("termination:pid_namespace", ProofSource::Measured),
        ]);
        let registry = registry();
        let selection =
            select_protocol(&pair(), &registry, NodeGuarantees::new(&held, &[])).unwrap();
        assert_eq!(selection.protocol.canonical_ref, CONTAINED);
        assert!(selection.holding.is_some());
    }

    #[test]
    fn a_trusted_node_selects_the_trusted_protocol() {
        let held = node(&[("sessions:trusted_process_group", ProofSource::Trusted)]);
        assert_eq!(selected(&pair(), &held, &[]).unwrap(), TRUSTED);
    }

    #[test]
    fn a_node_holding_neither_is_refused_with_each_unmet_guarantee() {
        // Enforced but uncontained, and not trusted: no candidate is met.
        let held = node(&[("isolation:enforced", ProofSource::Measured)]);
        let error = selected(&pair(), &held, &[]).unwrap_err().to_string();
        assert!(
            error.contains(CONTAINED) && error.contains(TRUSTED),
            "{error}"
        );
        assert!(
            error.contains("any_of(termination:scope, termination:pid_namespace)"),
            "{error}"
        );
        assert!(
            error.contains("sessions:trusted_process_group is not held"),
            "{error}"
        );
        assert!(!error.contains("isolation:enforced is not held"), "{error}");
    }

    #[test]
    fn a_weaker_grade_does_not_meet_a_bare_requirement() {
        let held = node(&[
            ("isolation:enforced", ProofSource::ProviderAttested),
            ("termination:scope", ProofSource::Measured),
        ]);
        let error = selected(&pair(), &held, &[]).unwrap_err().to_string();
        assert!(error.contains("provider_attested-proven"), "{error}");
    }

    #[test]
    fn an_attested_grade_is_admitted_only_where_the_node_floor_names_it() {
        let attested = node(&[
            ("isolation:enforced", ProofSource::Measured),
            ("termination:pid_namespace", ProofSource::ProviderAttested),
        ]);
        assert!(selected(&pair(), &attested, &[]).is_err());
        let other: Vec<CapabilityRequirement> = serde_json::from_value(json!([
            {"name": "termination:scope", "accept": ["provider_attested"]},
            {"name": "termination:pid_namespace", "accept": ["measured"]},
        ]))
        .unwrap();
        assert!(selected(&pair(), &attested, &other).is_err());
        let floor: Vec<CapabilityRequirement> = serde_json::from_value(json!([
            {"name": "termination:pid_namespace", "accept": ["measured", "provider_attested"]},
        ]))
        .unwrap();
        assert_eq!(selected(&pair(), &attested, &floor).unwrap(), CONTAINED);
    }

    #[test]
    fn two_met_candidates_are_refused_by_name() {
        let both = node(&[
            ("isolation:enforced", ProofSource::Measured),
            ("termination:scope", ProofSource::Measured),
            ("sessions:trusted_process_group", ProofSource::Trusted),
        ]);
        let error = selected(&pair(), &both, &[]).unwrap_err().to_string();
        assert!(error.contains("more than one"), "{error}");
        assert!(
            error.contains(CONTAINED) && error.contains(TRUSTED),
            "{error}"
        );
    }

    fn holding_of(registry: &ProtocolRegistry, protocol_ref: &str) -> ProtocolHolding {
        registry
            .require(protocol_ref)
            .unwrap()
            .descriptor
            .holding
            .clone()
            .unwrap()
    }

    #[test]
    fn a_retained_holding_is_met_while_the_node_still_holds_its_guarantees() {
        let registry = registry();
        let held = node(&[("sessions:trusted_process_group", ProofSource::Trusted)]);
        require_holding_met(
            TRUSTED,
            &holding_of(&registry, TRUSTED),
            NodeGuarantees::new(&held, &[]),
        )
        .unwrap();
    }

    #[test]
    fn a_retained_holding_is_refused_when_the_node_lost_a_guarantee_and_never_reselects() {
        let registry = registry();
        // The node now meets the OTHER candidate's guarantees only. Recovery
        // of a launch admitted under the trusted protocol must refuse rather
        // than move to the contained one.
        let now_contained = node(&[
            ("isolation:enforced", ProofSource::Measured),
            ("termination:scope", ProofSource::Measured),
        ]);
        let error = require_holding_met(
            TRUSTED,
            &holding_of(&registry, TRUSTED),
            NodeGuarantees::new(&now_contained, &[]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains(TRUSTED) && error.contains("sessions:trusted_process_group"),
            "{error}"
        );
        assert_eq!(
            selected(&pair(), &now_contained, &[]).unwrap(),
            CONTAINED,
            "a fresh launch on the same node would select the other protocol"
        );
    }

    #[test]
    fn selection_does_not_depend_on_candidate_order() {
        let reversed: Vec<String> = pair().into_iter().rev().collect();
        let trusted = node(&[("sessions:trusted_process_group", ProofSource::Trusted)]);
        assert_eq!(selected(&reversed, &trusted, &[]).unwrap(), TRUSTED);
        let contained = node(&[
            ("isolation:enforced", ProofSource::Measured),
            ("termination:scope", ProofSource::Measured),
        ]);
        assert_eq!(selected(&reversed, &contained, &[]).unwrap(), CONTAINED);
        let neither = node(&[]);
        assert_eq!(
            selected(&pair(), &neither, &[]).unwrap_err().to_string(),
            selected(&reversed, &neither, &[]).unwrap_err().to_string()
        );
        let both = node(&[
            ("isolation:enforced", ProofSource::Measured),
            ("termination:scope", ProofSource::Measured),
            ("sessions:trusted_process_group", ProofSource::Trusted),
        ]);
        assert_eq!(
            selected(&pair(), &both, &[]).unwrap_err().to_string(),
            selected(&reversed, &both, &[]).unwrap_err().to_string()
        );
    }
}
