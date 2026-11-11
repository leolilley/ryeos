//! Guarantee requirements for qualification variants.
//!
//! One consumer role can be served by several signed variants (a Worker, its
//! environment, relationship and policy), each valid only on a node that
//! holds particular guarantees: a variant that holds captured filesystem
//! authority needs enforced isolation and qualified descendant containment,
//! one that runs in a trusted process group needs node policy to accept
//! that weaker lane. A variant declares what it requires as data, in the
//! capability requirement shape and names a node already reports; the node
//! reports what it holds and the one variant whose requirements it meets is
//! selected. Nothing here names a consumer, an ecosystem or a lane.

use anyhow::{Context as _, bail, ensure};
use lillux::process_control::{
    CapabilityRequirement, CapabilityRequirementSpec, HostCapabilities, satisfies,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{qualification::bounded, validate_name};

pub const QUALIFICATION_VARIANTS_SCHEMA: &str = "ryeos.qualification_variants.v1";
const MAX_VARIANTS: usize = 8;
pub const MAX_REQUIRED_GUARANTEES: usize = 16;
const MAX_VARIANTS_BYTES: usize = 16 * 1024;

/// One accepted variant: the guarantees it requires and the signed
/// coordinates that select it. The Worker, environment and verifier identities
/// are pinned downstream from the selected policy exactly as for any single
/// policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationVariant {
    pub policy_ref: String,
    pub relationship_name: String,
    pub requires_guarantees: Vec<CapabilityRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualificationVariants {
    pub schema: String,
    pub variants: Vec<QualificationVariant>,
}

/// Why a requirement list is malformed, or `Ok` when every entry names its
/// capabilities and grades well. A variant that required nothing would be met
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

impl QualificationVariants {
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        bounded(value, MAX_VARIANTS_BYTES, "qualification variants")?;
        let set: Self =
            serde_json::from_value(value.clone()).context("decode qualification variants")?;
        set.validate()?;
        Ok(set)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.schema == QUALIFICATION_VARIANTS_SCHEMA,
            "unsupported qualification variants schema"
        );
        ensure!(
            !self.variants.is_empty() && self.variants.len() <= MAX_VARIANTS,
            "qualification variant count is out of bounds"
        );
        for variant in &self.variants {
            super::validate_canonical_unsuffixed_ref("variant policy", &variant.policy_ref)?;
            validate_name(&variant.relationship_name)?;
            validate_required_guarantees(&variant.requires_guarantees)
                .with_context(|| format!("variant {}", variant.policy_ref))?;
        }
        Ok(())
    }

    /// The single variant whose required guarantees `node` holds. No match
    /// and more than one match are both refused; the refusal lists every
    /// candidate with the guarantees it requires that the node lacks.
    pub fn select(&self, node: NodeGuarantees<'_>) -> anyhow::Result<&QualificationVariant> {
        let admitted: Vec<_> = self
            .variants
            .iter()
            .filter(|variant| unmet_guarantees(&variant.requires_guarantees, node).is_empty())
            .collect();
        match admitted.as_slice() {
            [one] => Ok(one),
            [] => bail!(
                "no qualification variant's required guarantees are held by this node; candidates: {}",
                self.candidates(node)
            ),
            several => bail!(
                "more than one qualification variant's required guarantees are held by this node: {}",
                several
                    .iter()
                    .map(|variant| variant.policy_ref.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn candidates(&self, node: NodeGuarantees<'_>) -> String {
        self.variants
            .iter()
            .map(|variant| {
                let unmet = unmet_guarantees(&variant.requires_guarantees, node)
                    .into_iter()
                    .map(|(label, reason)| format!("{label}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("{} (unmet: {unmet})", variant.policy_ref)
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

/// A policy that declares required guarantees may be proven or launched only
/// on a node that holds them. An empty declaration asks nothing of the node.
pub fn require_policy_guarantees(
    requires: &[CapabilityRequirement],
    node: NodeGuarantees<'_>,
    policy_ref: &str,
) -> anyhow::Result<()> {
    let unmet = unmet_guarantees(requires, node);
    if unmet.is_empty() {
        return Ok(());
    }
    bail!(
        "qualification policy {policy_ref} requires guarantees this node does not hold: {}",
        unmet
            .into_iter()
            .map(|(label, reason)| format!("{label}: {reason}"))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

#[cfg(test)]
mod tests {
    use lillux::process_control::ProofSource;
    use serde_json::json;

    use super::*;

    const CONTAINED: &str = "config:test/policy-contained";
    const TRUSTED: &str = "config:test/policy-trusted";

    fn set(extra: Option<Value>) -> QualificationVariants {
        let mut variants = vec![
            json!({
                "policy_ref": CONTAINED,
                "relationship_name": "contained",
                "requires_guarantees": [
                    "isolation:enforced",
                    {"any_of": ["termination:scope", "termination:pid_namespace"]},
                ],
            }),
            json!({
                "policy_ref": TRUSTED,
                "relationship_name": "trusted",
                "requires_guarantees": [
                    {"name": "sessions:trusted_process_group", "accept": ["trusted"]},
                ],
            }),
        ];
        variants.extend(extra);
        QualificationVariants::from_value(
            &json!({"schema": QUALIFICATION_VARIANTS_SCHEMA, "variants": variants}),
        )
        .unwrap()
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

    #[test]
    fn an_enforced_contained_node_selects_the_contained_variant() {
        let node = node(&[
            ("isolation:enforced", ProofSource::Measured),
            ("termination:pid_namespace", ProofSource::Measured),
        ]);
        assert_eq!(
            set(None)
                .select(NodeGuarantees::new(&node, &[]))
                .unwrap()
                .policy_ref,
            CONTAINED
        );
    }

    #[test]
    fn a_trusted_node_selects_the_trusted_variant() {
        let node = node(&[("sessions:trusted_process_group", ProofSource::Trusted)]);
        assert_eq!(
            set(None)
                .select(NodeGuarantees::new(&node, &[]))
                .unwrap()
                .policy_ref,
            TRUSTED
        );
    }

    #[test]
    fn a_node_holding_neither_is_refused_with_each_unmet_guarantee() {
        // Enforced but uncontained, and not trusted: no variant is met.
        let node = node(&[("isolation:enforced", ProofSource::Measured)]);
        let error = set(None)
            .select(NodeGuarantees::new(&node, &[]))
            .unwrap_err()
            .to_string();
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
        let node = node(&[
            ("isolation:enforced", ProofSource::ProviderAttested),
            ("termination:scope", ProofSource::Measured),
        ]);
        let error = set(None)
            .select(NodeGuarantees::new(&node, &[]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("provider_attested-proven"), "{error}");
    }

    #[test]
    fn an_attested_grade_is_admitted_only_where_the_node_floor_names_it() {
        let attested = node(&[
            ("isolation:enforced", ProofSource::Measured),
            ("termination:pid_namespace", ProofSource::ProviderAttested),
        ]);
        let variants = set(None);
        assert!(
            variants
                .select(NodeGuarantees::new(&attested, &[]))
                .is_err()
        );
        // A floor naming a different capability, or accepting only measured,
        // does not widen it.
        let other: Vec<CapabilityRequirement> = serde_json::from_value(json!([
            {"name": "termination:scope", "accept": ["provider_attested"]},
            {"name": "termination:pid_namespace", "accept": ["measured"]},
        ]))
        .unwrap();
        assert!(
            variants
                .select(NodeGuarantees::new(&attested, &other))
                .is_err()
        );
        let floor: Vec<CapabilityRequirement> = serde_json::from_value(json!([
            {"name": "termination:pid_namespace", "accept": ["measured", "provider_attested"]},
        ]))
        .unwrap();
        assert_eq!(
            variants
                .select(NodeGuarantees::new(&attested, &floor))
                .unwrap()
                .policy_ref,
            CONTAINED
        );
    }

    #[test]
    fn two_met_variants_are_refused_by_name() {
        let both = node(&[
            ("isolation:enforced", ProofSource::Measured),
            ("termination:scope", ProofSource::Measured),
            ("sessions:trusted_process_group", ProofSource::Trusted),
        ]);
        let error = set(None)
            .select(NodeGuarantees::new(&both, &[]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than one"), "{error}");
        assert!(
            error.contains(CONTAINED) && error.contains(TRUSTED),
            "{error}"
        );
    }

    #[test]
    fn a_variant_that_requires_nothing_or_is_malformed_is_refused() {
        let bad = |requires: Value| {
            QualificationVariants::from_value(&json!({
                "schema": QUALIFICATION_VARIANTS_SCHEMA,
                "variants": [{
                    "policy_ref": CONTAINED,
                    "relationship_name": "x",
                    "requires_guarantees": requires,
                }],
            }))
        };
        assert!(bad(json!([])).is_err());
        assert!(bad(json!([""])).is_err());
        assert!(bad(json!([{"name": "a", "any_of": ["b"]}])).is_err());
        assert!(bad(json!([{"name": "a", "accept": ["peer_attested"]}])).is_err());
        assert!(bad(json!(["isolation:enforced"])).is_ok());
        let mut value = json!({
            "schema": QUALIFICATION_VARIANTS_SCHEMA,
            "variants": [{
                "policy_ref": CONTAINED,
                "relationship_name": "x",
                "requires_guarantees": ["a"],
                "isolation_lane": "enforced",
            }],
        });
        assert!(QualificationVariants::from_value(&value).is_err());
        value["variants"][0]
            .as_object_mut()
            .unwrap()
            .remove("isolation_lane");
        assert!(QualificationVariants::from_value(&value).is_ok());
    }

    #[test]
    fn a_policy_declaration_must_be_held_by_the_node() {
        let requires: Vec<CapabilityRequirement> =
            serde_json::from_value(json!(["isolation:enforced"])).unwrap();
        let held = node(&[("isolation:enforced", ProofSource::Measured)]);
        require_policy_guarantees(&requires, NodeGuarantees::new(&held, &[]), "p").unwrap();
        require_policy_guarantees(&[], NodeGuarantees::new(&node(&[]), &[]), "p").unwrap();
        let error = require_policy_guarantees(&requires, NodeGuarantees::new(&node(&[]), &[]), "p")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("policy p") && error.contains("isolation:enforced is not held"),
            "{error}"
        );
    }
}
