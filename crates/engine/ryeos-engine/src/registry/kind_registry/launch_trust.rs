//! Which spaces and trust classes may supply the root of a launch.
//!
//! Every root-executable kind schema declares a `launch_trust` block. Its
//! `allowed_spaces` decide which resolution roots may supply an item of the
//! kind at all, its `allowed_trust` is the default set of trust classes a
//! launch root may carry, and `operator_may_admit` names the classes a node
//! operator may add through node-signed policy. Unsigned work is admissible
//! nowhere, and no default admits work signed by an unpinned key.

use serde::{Deserialize, Serialize};

use crate::contracts::ItemSpace;
use crate::error::EngineError;
use crate::resolution::TrustClass;

/// The signed, kind-owned launch trust declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchTrustPolicy {
    /// Resolution spaces that may supply an item of this kind. A candidate
    /// found in any other space is not a candidate.
    pub allowed_spaces: Vec<ItemSpace>,
    /// Trust classes a launch root of this kind is admitted with by default.
    pub allowed_trust: Vec<TrustClass>,
    /// Trust classes an operator may add for this kind through node policy.
    #[serde(default)]
    pub operator_may_admit: Vec<TrustClass>,
}

impl LaunchTrustPolicy {
    pub fn validate(&self, display: &str) -> Result<(), EngineError> {
        let invalid = |detail: String| EngineError::SchemaLoaderError {
            reason: format!("{display}: launch_trust {detail}"),
        };
        if self.allowed_spaces.is_empty() {
            return Err(invalid("allowed_spaces must not be empty".to_owned()));
        }
        if has_duplicates(&self.allowed_spaces) {
            return Err(invalid("allowed_spaces contains a duplicate".to_owned()));
        }
        if self.allowed_spaces.contains(&ItemSpace::Node) {
            return Err(invalid(
                "allowed_spaces may not name `node`: node-local configuration is never a launch root"
                    .to_owned(),
            ));
        }
        if self.allowed_trust.is_empty() {
            return Err(invalid("allowed_trust must not be empty".to_owned()));
        }
        if has_duplicates(&self.allowed_trust) || has_duplicates(&self.operator_may_admit) {
            return Err(invalid("lists a trust class twice".to_owned()));
        }
        for class in &self.allowed_trust {
            match class {
                TrustClass::TrustedBundle | TrustClass::TrustedProject => {}
                TrustClass::UntrustedProject => {
                    return Err(invalid(
                        "allowed_trust may not admit `untrusted_project` by default; \
                         declare it in operator_may_admit"
                            .to_owned(),
                    ));
                }
                TrustClass::TrustedNode | TrustClass::Unsigned => {
                    return Err(invalid(format!(
                        "allowed_trust may not name `{}`",
                        trust_class_name(*class)
                    )));
                }
            }
            if !self.allowed_spaces.contains(&space_of(*class)) {
                return Err(invalid(format!(
                    "allowed_trust names `{}` but allowed_spaces does not admit its space",
                    trust_class_name(*class)
                )));
            }
        }
        for class in &self.operator_may_admit {
            if *class != TrustClass::UntrustedProject {
                return Err(invalid(format!(
                    "operator_may_admit may name only `untrusted_project`, not `{}`",
                    trust_class_name(*class)
                )));
            }
            if !self.allowed_spaces.contains(&ItemSpace::Project) {
                return Err(invalid(
                    "operator_may_admit names `untrusted_project` but allowed_spaces does not admit `project`"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }

    pub fn admits_space(&self, space: ItemSpace) -> bool {
        self.allowed_spaces.contains(&space)
    }

    /// The trust classes admitted for a launch root of this kind: the
    /// `declared` list (the kind default, or a runtime's own launch list)
    /// plus whatever the operator admits that this kind lets an operator
    /// admit. Unsigned is never admitted.
    pub fn effective(
        &self,
        declared: &[TrustClass],
        operator_admits: &[TrustClass],
    ) -> EffectiveLaunchTrust {
        let mut admitted: Vec<TrustClass> = declared
            .iter()
            .copied()
            .filter(|class| *class != TrustClass::Unsigned)
            .collect();
        let mut operator_added = Vec::new();
        for class in operator_admits {
            if self.operator_may_admit.contains(class) && !admitted.contains(class) {
                admitted.push(*class);
                operator_added.push(*class);
            }
        }
        EffectiveLaunchTrust {
            admitted,
            operator_added,
        }
    }
}

/// The admitted classes for one launch, with the operator's additions kept
/// apart so a refusal can say what the operator admits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveLaunchTrust {
    pub admitted: Vec<TrustClass>,
    pub operator_added: Vec<TrustClass>,
}

impl EffectiveLaunchTrust {
    pub fn admits(&self, class: TrustClass) -> bool {
        class != TrustClass::Unsigned && self.admitted.contains(&class)
    }

    pub fn admitted_names(&self) -> Vec<&'static str> {
        self.admitted
            .iter()
            .map(|class| trust_class_name(*class))
            .collect()
    }

    pub fn operator_added_names(&self) -> Vec<&'static str> {
        self.operator_added
            .iter()
            .map(|class| trust_class_name(*class))
            .collect()
    }
}

/// The wire name of a trust class, as kind schemas and policies spell it.
pub fn trust_class_name(class: TrustClass) -> &'static str {
    match class {
        TrustClass::TrustedBundle => "trusted_bundle",
        TrustClass::TrustedProject => "trusted_project",
        TrustClass::TrustedNode => "trusted_node",
        TrustClass::UntrustedProject => "untrusted_project",
        TrustClass::Unsigned => "unsigned",
    }
}

fn space_of(class: TrustClass) -> ItemSpace {
    match class {
        TrustClass::TrustedBundle => ItemSpace::Bundle,
        TrustClass::TrustedNode => ItemSpace::Node,
        TrustClass::TrustedProject | TrustClass::UntrustedProject | TrustClass::Unsigned => {
            ItemSpace::Project
        }
    }
}

fn has_duplicates<T: PartialEq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(index, value)| values[..index].contains(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(
        spaces: &[ItemSpace],
        trust: &[TrustClass],
        operator: &[TrustClass],
    ) -> LaunchTrustPolicy {
        LaunchTrustPolicy {
            allowed_spaces: spaces.to_vec(),
            allowed_trust: trust.to_vec(),
            operator_may_admit: operator.to_vec(),
        }
    }

    fn tool_default() -> LaunchTrustPolicy {
        policy(
            &[ItemSpace::Bundle, ItemSpace::Project],
            &[TrustClass::TrustedBundle, TrustClass::TrustedProject],
            &[TrustClass::UntrustedProject],
        )
    }

    #[test]
    fn shipped_shapes_validate() {
        tool_default().validate("tool").unwrap();
        policy(&[ItemSpace::Bundle], &[TrustClass::TrustedBundle], &[])
            .validate("service")
            .unwrap();
    }

    #[test]
    fn unsigned_and_untrusted_defaults_are_refused() {
        for class in [TrustClass::Unsigned, TrustClass::UntrustedProject] {
            let error = policy(
                &[ItemSpace::Bundle, ItemSpace::Project],
                &[TrustClass::TrustedBundle, class],
                &[],
            )
            .validate("tool")
            .unwrap_err();
            assert!(error.to_string().contains("allowed_trust"), "{error}");
        }
    }

    #[test]
    fn node_space_and_node_trust_are_refused() {
        assert!(
            policy(
                &[ItemSpace::Bundle, ItemSpace::Node],
                &[TrustClass::TrustedBundle],
                &[]
            )
            .validate("tool")
            .is_err()
        );
        assert!(
            policy(
                &[ItemSpace::Bundle],
                &[TrustClass::TrustedBundle, TrustClass::TrustedNode],
                &[]
            )
            .validate("tool")
            .is_err()
        );
    }

    #[test]
    fn operator_may_admit_only_untrusted_project_where_projects_are_allowed() {
        assert!(
            policy(
                &[ItemSpace::Bundle],
                &[TrustClass::TrustedBundle],
                &[TrustClass::UntrustedProject]
            )
            .validate("service")
            .is_err()
        );
        assert!(
            policy(
                &[ItemSpace::Bundle, ItemSpace::Project],
                &[TrustClass::TrustedBundle],
                &[TrustClass::Unsigned]
            )
            .validate("tool")
            .is_err()
        );
    }

    #[test]
    fn a_trust_class_needs_its_space() {
        assert!(
            policy(&[ItemSpace::Bundle], &[TrustClass::TrustedProject], &[])
                .validate("service")
                .is_err()
        );
    }

    #[test]
    fn effective_list_adds_only_what_the_kind_lets_an_operator_admit() {
        let tool = tool_default();
        let declared = [TrustClass::TrustedBundle, TrustClass::TrustedProject];
        let none = tool.effective(&declared, &[]);
        assert!(!none.admits(TrustClass::UntrustedProject));
        assert!(none.operator_added.is_empty());

        let widened = tool.effective(&declared, &[TrustClass::UntrustedProject]);
        assert!(widened.admits(TrustClass::UntrustedProject));
        assert_eq!(widened.operator_added_names(), ["untrusted_project"]);

        let service = policy(&[ItemSpace::Bundle], &[TrustClass::TrustedBundle], &[]);
        let refused = service.effective(
            &[TrustClass::TrustedBundle],
            &[TrustClass::UntrustedProject],
        );
        assert!(!refused.admits(TrustClass::UntrustedProject));
    }

    #[test]
    fn unsigned_is_never_admitted_even_if_declared() {
        let effective = tool_default().effective(&[TrustClass::Unsigned], &[TrustClass::Unsigned]);
        assert!(!effective.admits(TrustClass::Unsigned));
        assert!(effective.admitted.is_empty());
    }
}
