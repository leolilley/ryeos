//! Node guarantees: what a node holds, and the matcher that decides whether a
//! declared requirement list is met.
//!
//! A holder declares what it requires as data, in the capability requirement
//! shape and names a node already reports; the node reports what it holds and
//! the matcher says which entries it does not meet. Nothing here names a
//! consumer, an ecosystem or a protocol.

use anyhow::{bail, ensure};
use lillux::process_control::{
    CapabilityRequirement, CapabilityRequirementSpec, HostCapabilities, satisfies,
};

pub const MAX_REQUIRED_GUARANTEES: usize = 16;

/// Why a requirement list is malformed, or `Ok` when every entry names its
/// capabilities and grades well. A holder that required nothing would be met
/// by every node, so an empty list is refused.
pub fn validate_required_guarantees(requires: &[CapabilityRequirement]) -> anyhow::Result<()> {
    ensure!(
        !requires.is_empty() && requires.len() <= MAX_REQUIRED_GUARANTEES,
        "required guarantee count is out of bounds"
    );
    for entry in requires {
        if let Some(refusal) = entry.spec_refusal() {
            bail!("required guarantee is malformed: {refusal}");
        }
    }
    Ok(())
}

/// What a node holds: the guarantees it reports and the signed capability
/// floor that decides which weaker grades of them it admits. A requirement
/// names grades itself (bare means measured only); a node whose floor
/// explicitly accepts a grade for a capability admits that grade for the same
/// capability here, exactly as it does for its own startup.
#[derive(Debug, Clone, Copy)]
pub struct NodeGuarantees<'a> {
    pub reported: &'a HostCapabilities,
    pub floor: &'a [CapabilityRequirement],
}

impl<'a> NodeGuarantees<'a> {
    pub fn new(reported: &'a HostCapabilities, floor: &'a [CapabilityRequirement]) -> Self {
        Self { reported, floor }
    }

    /// `Ok` when `entry` is met, else the reason.
    fn meets(&self, entry: &CapabilityRequirement) -> Result<(), String> {
        let refusal = match self.reported.meets(entry) {
            Ok(_) => return Ok(()),
            Err(refusal) => refusal,
        };
        for name in entry.names() {
            let Some(evidence) = self.reported.provided.get(name) else {
                continue;
            };
            let mut accept: Vec<String> = entry.accepted_grades().to_vec();
            for floor_entry in self.floor {
                if floor_entry.names().contains(name) {
                    for grade in floor_entry.accepted_grades() {
                        if !accept.contains(grade) {
                            accept.push(grade.clone());
                        }
                    }
                }
            }
            let widened = CapabilityRequirement::Spec(CapabilityRequirementSpec {
                name: Some(name.clone()),
                any_of: Vec::new(),
                accept,
                providers: entry.providers().to_vec(),
            });
            if satisfies(&widened, name, evidence).is_ok() {
                return Ok(());
            }
        }
        Err(refusal)
    }
}

/// The entries of `requires` the node does not hold, each with its reason.
pub fn unmet_guarantees(
    requires: &[CapabilityRequirement],
    node: NodeGuarantees<'_>,
) -> Vec<(String, String)> {
    requires
        .iter()
        .filter_map(|entry| {
            node.meets(entry)
                .err()
                .map(|reason| (entry.label(), reason))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use lillux::process_control::ProofSource;
    use serde_json::json;

    use super::*;

    fn requires(value: serde_json::Value) -> Vec<CapabilityRequirement> {
        serde_json::from_value(value).unwrap()
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

    fn contained() -> Vec<CapabilityRequirement> {
        requires(json!([
            "isolation:enforced",
            {"any_of": ["termination:scope", "termination:pid_namespace"]},
        ]))
    }

    fn trusted() -> Vec<CapabilityRequirement> {
        requires(json!([
            {"name": "sessions:trusted_process_group", "accept": ["trusted"]},
        ]))
    }

    fn unmet(requires: &[CapabilityRequirement], node: &HostCapabilities) -> Vec<String> {
        unmet_guarantees(requires, NodeGuarantees::new(node, &[]))
            .into_iter()
            .map(|(label, reason)| format!("{label}: {reason}"))
            .collect()
    }

    #[test]
    fn an_enforced_contained_node_meets_the_contained_requirements_only() {
        let node = node(&[
            ("isolation:enforced", ProofSource::Measured),
            ("termination:pid_namespace", ProofSource::Measured),
        ]);
        assert!(unmet(&contained(), &node).is_empty());
        assert!(!unmet(&trusted(), &node).is_empty());
    }

    #[test]
    fn a_trusted_node_meets_the_trusted_requirements_only() {
        let node = node(&[("sessions:trusted_process_group", ProofSource::Trusted)]);
        assert!(unmet(&trusted(), &node).is_empty());
        assert!(!unmet(&contained(), &node).is_empty());
    }

    #[test]
    fn an_unmet_entry_is_listed_with_its_reason() {
        // Enforced but uncontained, and not trusted.
        let node = node(&[("isolation:enforced", ProofSource::Measured)]);
        let contained = unmet(&contained(), &node).join(" | ");
        assert!(
            contained.contains("any_of(termination:scope, termination:pid_namespace)"),
            "{contained}"
        );
        assert!(!contained.contains("isolation:enforced is not held"), "{contained}");
        let trusted = unmet(&trusted(), &node).join(" | ");
        assert!(
            trusted.contains("sessions:trusted_process_group is not held"),
            "{trusted}"
        );
    }

    #[test]
    fn a_weaker_grade_does_not_meet_a_bare_requirement() {
        let node = node(&[
            ("isolation:enforced", ProofSource::ProviderAttested),
            ("termination:scope", ProofSource::Measured),
        ]);
        let refusal = unmet(&contained(), &node).join(" | ");
        assert!(refusal.contains("provider_attested-proven"), "{refusal}");
    }

    #[test]
    fn an_attested_grade_is_admitted_only_where_the_node_floor_names_it() {
        let attested = node(&[
            ("isolation:enforced", ProofSource::Measured),
            ("termination:pid_namespace", ProofSource::ProviderAttested),
        ]);
        let requires = contained();
        assert!(!unmet_guarantees(&requires, NodeGuarantees::new(&attested, &[])).is_empty());
        // A floor naming a different capability, or accepting only measured,
        // does not widen it.
        let other = self::requires(json!([
            {"name": "termination:scope", "accept": ["provider_attested"]},
            {"name": "termination:pid_namespace", "accept": ["measured"]},
        ]));
        assert!(!unmet_guarantees(&requires, NodeGuarantees::new(&attested, &other)).is_empty());
        let floor = self::requires(json!([
            {"name": "termination:pid_namespace", "accept": ["measured", "provider_attested"]},
        ]));
        assert!(unmet_guarantees(&requires, NodeGuarantees::new(&attested, &floor)).is_empty());
    }

    #[test]
    fn a_requirement_list_that_asks_nothing_or_is_malformed_is_refused() {
        assert!(validate_required_guarantees(&[]).is_err());
        assert!(validate_required_guarantees(&requires(json!([""]))).is_err());
        assert!(
            validate_required_guarantees(&requires(json!([{"name": "a", "any_of": ["b"]}])))
                .is_err()
        );
        assert!(
            validate_required_guarantees(&requires(json!([
                {"name": "a", "accept": ["peer_attested"]}
            ])))
            .is_err()
        );
        assert!(validate_required_guarantees(&requires(json!(["isolation:enforced"]))).is_ok());
        let too_many = vec![CapabilityRequirement::Name("a".to_owned()); MAX_REQUIRED_GUARANTEES + 1];
        assert!(validate_required_guarantees(&too_many).is_err());
    }
}
