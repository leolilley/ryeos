//! The one launch-trust gate.
//!
//! Every path that launches an item (direct tools, in-process services,
//! method dispatch, managed launches, recovery) asks this module whether the
//! item's resolution space and trust class are admitted for its kind. The
//! admitted set is the kind's declared default (or a runtime's own launch
//! list for managed launches) plus whatever node-signed operator policy adds
//! within what the kind lets an operator admit. Unsigned work is refused
//! everywhere. Refusals are launch-policy errors that name the class seen and
//! the classes admitted.

use ryeos_engine::contracts::ItemSpace;
use ryeos_engine::kind_registry::KindRegistry;
use ryeos_engine::kind_registry::launch_trust::{EffectiveLaunchTrust, trust_class_name};
use ryeos_engine::resolution::TrustClass;
use ryeos_node_runtime::node_policy::NodePolicySnapshot;
use ryeos_node_runtime::node_policy::sections::launch_trust::NodeLaunchTrustPolicy;

use crate::dispatch_error::DispatchError;

/// Where in a launch the judged item sits, for the refusal text.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LaunchTrustPosition<'a> {
    Root,
    Hop(&'a str),
    Ancestor(&'a str),
    Referenced(&'a str),
}

impl std::fmt::Display for LaunchTrustPosition<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Root => formatter.write_str("the launch root"),
            Self::Hop(item) => write!(formatter, "executor hop `{item}`"),
            Self::Ancestor(item) => write!(formatter, "ancestor `{item}`"),
            Self::Referenced(item) => write!(formatter, "referenced item `{item}`"),
        }
    }
}

/// One item to judge.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LaunchTrustSubject<'a> {
    pub item_ref: &'a str,
    pub kind: &'a str,
    pub space: ItemSpace,
    pub class: TrustClass,
    pub position: LaunchTrustPosition<'a>,
}

/// The admitted classes for a launch of `kind`. `declared` is a runtime's
/// own launch list for managed launches; `None` uses the kind's default.
pub(crate) fn effective_launch_trust(
    kinds: &KindRegistry,
    node_policy: &NodePolicySnapshot,
    kind: &str,
    declared: Option<&[TrustClass]>,
) -> Result<EffectiveLaunchTrust, DispatchError> {
    let policy = kind_launch_trust(kinds, kind)?;
    let operator = operator_policy(node_policy)?;
    let declared = declared.unwrap_or(&policy.allowed_trust);
    Ok(policy.effective(declared, operator.admitted_for(kind)))
}

/// Refuse `subject` unless its space and trust class are admitted for its
/// kind. `declared` as for [`effective_launch_trust`].
pub(crate) fn enforce_launch_trust(
    kinds: &KindRegistry,
    node_policy: &NodePolicySnapshot,
    subject: LaunchTrustSubject<'_>,
    declared: Option<&[TrustClass]>,
) -> Result<(), DispatchError> {
    let policy = kind_launch_trust(kinds, subject.kind)?;
    if !policy.admits_space(subject.space) {
        return Err(DispatchError::LaunchPolicyForbidden {
            code: "launch_space_refused".to_owned(),
            message: format!(
                "refusing to launch `{}` ({}): {} comes from the {} space, which this kind does not admit",
                subject.item_ref,
                subject.kind,
                subject.position,
                space_name(subject.space)
            ),
            binding: None,
        });
    }
    let effective = effective_launch_trust(kinds, node_policy, subject.kind, declared)?;
    if effective.admits(subject.class) {
        return Ok(());
    }
    Err(trust_refusal(&subject, &effective))
}

/// The refusal for a subject whose class is not admitted. Unsigned work keeps
/// its own code so a caller can tell "sign this" from "the operator has not
/// admitted this signer".
pub(crate) fn trust_refusal(
    subject: &LaunchTrustSubject<'_>,
    effective: &EffectiveLaunchTrust,
) -> DispatchError {
    let code = if subject.class == TrustClass::Unsigned {
        "effective_trust_unsigned"
    } else {
        "effective_trust_refused"
    };
    DispatchError::LaunchPolicyForbidden {
        code: code.to_owned(),
        message: format!(
            "refusing to launch `{}` ({}): {} has trust class {}; admitted: [{}]; operator policy admits: [{}]",
            subject.item_ref,
            subject.kind,
            subject.position,
            trust_class_name(subject.class),
            effective.admitted_names().join(", "),
            effective.operator_added_names().join(", ")
        ),
        binding: None,
    }
}

/// What an in-process service may run: the compiled handler's endpoint and
/// the capabilities its compiled descriptor requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdmittedService {
    pub endpoint: String,
    pub required_caps: Vec<String>,
}

/// Admit a verified service item for dispatch. The item must come from a
/// space and carry a trust class its kind admits, and it must name exactly
/// the endpoint and capabilities of the compiled handler descriptor for its
/// ref: an item can select neither a different handler nor weaker caps. The
/// returned endpoint and caps are the descriptor's.
pub(crate) fn admit_verified_service(
    verified: &ryeos_engine::contracts::VerifiedItem,
    kinds: &KindRegistry,
    node_policy: &NodePolicySnapshot,
    descriptors: &[ryeos_app::service_registry::ServiceDescriptor],
) -> Result<AdmittedService, DispatchError> {
    let service_ref = verified.resolved.canonical_ref.to_string();
    enforce_launch_trust(
        kinds,
        node_policy,
        LaunchTrustSubject {
            item_ref: &service_ref,
            kind: &verified.resolved.kind,
            space: verified.resolved.source_space,
            class: verified.launch_trust_class(),
            position: LaunchTrustPosition::Root,
        },
        None,
    )?;
    let descriptor = descriptors
        .iter()
        .find(|descriptor| descriptor.service_ref == service_ref)
        .ok_or_else(|| DispatchError::LaunchPolicyForbidden {
            code: "service_descriptor_missing".to_owned(),
            message: format!("refusing to run `{service_ref}`: no compiled handler serves it"),
            binding: None,
        })?;
    let endpoint = ryeos_app::service_registry::extract_endpoint(&verified.resolved.metadata.extra)
        .map_err(|error| DispatchError::InvalidRef(service_ref.clone(), format!("{error:#}")))?;
    let mut item_caps =
        ryeos_app::service_registry::extract_required_caps(&verified.resolved.metadata.extra);
    let mut compiled_caps: Vec<String> = descriptor
        .required_caps
        .iter()
        .map(|cap| (*cap).to_owned())
        .collect();
    item_caps.sort();
    compiled_caps.sort();
    if endpoint != descriptor.endpoint || item_caps != compiled_caps {
        return Err(DispatchError::LaunchPolicyForbidden {
            code: "service_descriptor_mismatch".to_owned(),
            message: format!(
                "refusing to run `{service_ref}`: the item names endpoint `{endpoint}` and caps {item_caps:?}, \
                 but its compiled handler serves `{}` with caps {compiled_caps:?}",
                descriptor.endpoint
            ),
            binding: None,
        });
    }
    Ok(AdmittedService {
        endpoint: descriptor.endpoint.to_owned(),
        required_caps: compiled_caps,
    })
}

fn kind_launch_trust<'k>(
    kinds: &'k KindRegistry,
    kind: &str,
) -> Result<&'k ryeos_engine::kind_registry::launch_trust::LaunchTrustPolicy, DispatchError> {
    kinds
        .get(kind)
        .and_then(|schema| schema.launch_trust.as_ref())
        .ok_or_else(|| DispatchError::LaunchPolicyForbidden {
            code: "launch_trust_undeclared".to_owned(),
            message: format!(
                "refusing to launch a `{kind}` item: its kind declares no launch_trust"
            ),
            binding: None,
        })
}

fn operator_policy(
    node_policy: &NodePolicySnapshot,
) -> Result<&NodeLaunchTrustPolicy, DispatchError> {
    node_policy
        .require::<NodeLaunchTrustPolicy>()
        .map_err(|error| DispatchError::Internal(error.context("node launch_trust policy")))
}

fn space_name(space: ItemSpace) -> &'static str {
    match space {
        ItemSpace::Bundle => "bundle",
        ItemSpace::Project => "project",
        ItemSpace::Node => "node",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::kind_registry::launch_trust::LaunchTrustPolicy;
    use std::sync::Arc;

    fn tool_policy() -> LaunchTrustPolicy {
        LaunchTrustPolicy {
            allowed_spaces: vec![ItemSpace::Bundle, ItemSpace::Project],
            allowed_trust: vec![TrustClass::TrustedBundle, TrustClass::TrustedProject],
            operator_may_admit: vec![TrustClass::UntrustedProject],
        }
    }

    fn subject(class: TrustClass) -> LaunchTrustSubject<'static> {
        LaunchTrustSubject {
            item_ref: "tool:gap/echo",
            kind: "tool",
            space: ItemSpace::Project,
            class,
            position: LaunchTrustPosition::Root,
        }
    }

    #[test]
    fn unsigned_refusal_keeps_its_code_and_names_what_is_admitted() {
        let effective = tool_policy().effective(&tool_policy().allowed_trust, &[]);
        let DispatchError::LaunchPolicyForbidden { code, message, .. } =
            trust_refusal(&subject(TrustClass::Unsigned), &effective)
        else {
            panic!("expected a launch-policy refusal");
        };
        assert_eq!(code, "effective_trust_unsigned");
        assert!(message.contains("trust class unsigned"), "{message}");
        assert!(
            message.contains("admitted: [trusted_bundle, trusted_project]"),
            "{message}"
        );
        assert!(message.contains("operator policy admits: []"), "{message}");
    }

    #[test]
    fn unpinned_signer_refusal_has_its_own_code() {
        let effective = tool_policy().effective(&tool_policy().allowed_trust, &[]);
        let DispatchError::LaunchPolicyForbidden { code, message, .. } =
            trust_refusal(&subject(TrustClass::UntrustedProject), &effective)
        else {
            panic!("expected a launch-policy refusal");
        };
        assert_eq!(code, "effective_trust_refused");
        assert!(
            message.contains("trust class untrusted_project"),
            "{message}"
        );
    }

    #[test]
    fn operator_policy_is_read_from_the_node_policy_snapshot() {
        let snapshot = NodePolicySnapshot::from_test_records(vec![Arc::new(
            NodeLaunchTrustPolicy::admitting(&[("tool", &[TrustClass::UntrustedProject])]),
        )]);
        let operator = operator_policy(&snapshot).unwrap();
        assert_eq!(
            operator.admitted_for("tool"),
            [TrustClass::UntrustedProject]
        );
        let effective =
            tool_policy().effective(&tool_policy().allowed_trust, operator.admitted_for("tool"));
        assert!(effective.admits(TrustClass::UntrustedProject));
        assert!(!effective.admits(TrustClass::Unsigned));
    }

    #[test]
    fn a_snapshot_without_the_section_fails_closed() {
        let snapshot = NodePolicySnapshot::from_test_records(Vec::new());
        assert!(matches!(
            operator_policy(&snapshot),
            Err(DispatchError::Internal(_))
        ));
    }
}
