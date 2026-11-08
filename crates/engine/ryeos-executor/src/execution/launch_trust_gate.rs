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
}

impl std::fmt::Display for LaunchTrustPosition<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Root => formatter.write_str("the launch root"),
            Self::Hop(item) => write!(formatter, "executor hop `{item}`"),
            Self::Ancestor(item) => write!(formatter, "ancestor `{item}`"),
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

/// Refuse a direct launch root that its kind does not admit.
pub(crate) fn enforce_direct_root(
    kinds: &KindRegistry,
    node_policy: &NodePolicySnapshot,
    item_ref: &str,
    kind: &str,
    space: ItemSpace,
    class: TrustClass,
) -> Result<(), DispatchError> {
    enforce_launch_trust(
        kinds,
        node_policy,
        LaunchTrustSubject {
            item_ref,
            kind,
            space,
            class,
            position: LaunchTrustPosition::Root,
        },
        None,
    )
}

/// Refuse a launch whose executor chain passes through any hop its kind
/// does not admit. Every hop is judged, so an unsigned or unpinned item can
/// stand in for neither a runtime nor the subprocess terminal.
pub(crate) fn enforce_executor_chain(
    kinds: &KindRegistry,
    node_policy: &NodePolicySnapshot,
    chain: &[ryeos_engine::contracts::PlanTrustAuthority],
) -> Result<(), DispatchError> {
    for authority in chain {
        let hop = ryeos_engine::canonical_ref::CanonicalRef::parse(&authority.canonical_ref)
            .map_err(|error| {
                DispatchError::InvalidRef(authority.canonical_ref.clone(), error.to_string())
            })?;
        enforce_launch_trust(
            kinds,
            node_policy,
            LaunchTrustSubject {
                item_ref: &authority.canonical_ref,
                kind: &hop.kind,
                space: authority.source_space,
                class: authority.trust_class,
                position: LaunchTrustPosition::Hop(&authority.canonical_ref),
            },
            None,
        )?;
    }
    Ok(())
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

    const SERVICE_KIND_SCHEMA: &str = "location:
  directory: services
effective_trust:
  include_references: false
resolution: []
launch_trust:
  allowed_spaces: [bundle]
  allowed_trust: [trusted_bundle]
  operator_may_admit: []
execution:
  terminator:
    kind: in_process
    registry: services
  thread_profile:
    name: service_run
formats:
  - extensions: [\".yaml\"]
    parser: parser:ryeos/core/yaml/yaml
    signature:
      prefix: \"#\"
composer: handler:ryeos/core/identity
composed_value_contract:
  root_type: mapping
  required: {}
";

    fn service_kinds() -> (tempfile::TempDir, KindRegistry) {
        let key = lillux::crypto::SigningKey::from_bytes(&[7u8; 32]);
        let verifying_key = key.verifying_key();
        let trust_store = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: ryeos_engine::trust::compute_fingerprint(&verifying_key),
                verifying_key,
                label: None,
            },
        ]);
        let root = tempfile::tempdir().unwrap();
        let kind_dir = root.path().join("service");
        std::fs::create_dir_all(&kind_dir).unwrap();
        std::fs::write(
            kind_dir.join("service.kind-schema.yaml"),
            lillux::signature::sign_content(SERVICE_KIND_SCHEMA, &key, "#", None),
        )
        .unwrap();
        let kinds = KindRegistry::load_base(&[root.path().to_path_buf()], &trust_store).unwrap();
        (root, kinds)
    }

    fn noop_handler(
        _params: serde_json::Value,
        _context: ryeos_app::handler_context::HandlerContext,
        _state: Arc<ryeos_app::state::AppState>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<serde_json::Value>> + Send>,
    > {
        Box::pin(async { Ok(serde_json::Value::Null) })
    }

    const HEALTH: ryeos_app::service_registry::ServiceDescriptor =
        ryeos_app::service_registry::ServiceDescriptor {
            service_ref: "service:health/status",
            endpoint: "health.status",
            availability: ryeos_app::service_registry::ServiceAvailability::Both,
            required_caps: &["ryeos.execute.service.health/status"],
            handler: noop_handler,
        };

    fn service_item(
        space: ItemSpace,
        trust: ryeos_engine::contracts::TrustClass,
        endpoint: &str,
        caps: &[&str],
    ) -> ryeos_engine::contracts::VerifiedItem {
        use ryeos_engine::contracts::{
            ItemMetadata, ItemSourceRoot, ResolvedItem, ResolvedSourceFormat, SignatureEnvelope,
            SubjectResolutionAuthority, VerifiedItem,
        };
        let mut metadata = ItemMetadata::default();
        metadata
            .extra
            .insert("endpoint".to_owned(), serde_json::json!(endpoint));
        metadata
            .extra
            .insert("required_caps".to_owned(), serde_json::json!(caps));
        VerifiedItem {
            resolved: ResolvedItem {
                canonical_ref: ryeos_engine::canonical_ref::CanonicalRef::parse(
                    "service:health/status",
                )
                .unwrap(),
                kind: "service".to_owned(),
                source_path: "/root/.ai/services/health/status.yaml".into(),
                source_space: space,
                source_root: match space {
                    ItemSpace::Project => ItemSourceRoot::Project,
                    _ => ItemSourceRoot::Bundle {
                        name: "core".to_owned(),
                    },
                },
                resolved_from: "test".to_owned(),
                shadowed: Vec::new(),
                probed_absent: Vec::new(),
                materialized_project_root: None,
                subject_resolution_authority: SubjectResolutionAuthority::Projectless,
                raw_content_digest: "digest".to_owned(),
                content_hash: "digest".to_owned(),
                signature_header: None,
                source_format: ResolvedSourceFormat {
                    extension: ".yaml".to_owned(),
                    parser: "parser:ryeos/core/yaml/yaml".to_owned(),
                    signature: SignatureEnvelope {
                        prefix: "#".to_owned(),
                        suffix: None,
                        after_shebang: false,
                    },
                },
                metadata,
            },
            signer: None,
            trust_class: trust,
            pinned_version: None,
        }
    }

    fn admit(
        kinds: &KindRegistry,
        item: &ryeos_engine::contracts::VerifiedItem,
    ) -> Result<AdmittedService, DispatchError> {
        let policy = NodePolicySnapshot::from_test_records(vec![
            ryeos_node_runtime::node_policy::default_launch_trust_record(),
        ]);
        admit_verified_service(item, kinds, &policy, &[HEALTH])
    }

    fn refusal_code(result: Result<AdmittedService, DispatchError>) -> String {
        match result {
            Err(DispatchError::LaunchPolicyForbidden { code, .. }) => code,
            other => panic!("expected a launch-policy refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_bundle_service_matching_its_descriptor_runs_the_descriptor() {
        use ryeos_engine::contracts::TrustClass as Verdict;
        let (_root, kinds) = service_kinds();
        let item = service_item(
            ItemSpace::Bundle,
            Verdict::Trusted,
            "health.status",
            &["ryeos.execute.service.health/status"],
        );
        let admitted = admit(&kinds, &item).unwrap();
        assert_eq!(admitted.endpoint, "health.status");
        assert_eq!(
            admitted.required_caps,
            ["ryeos.execute.service.health/status"]
        );
    }

    #[test]
    fn a_service_naming_another_handler_or_weaker_caps_is_refused() {
        use ryeos_engine::contracts::TrustClass as Verdict;
        let (_root, kinds) = service_kinds();
        let other_handler = service_item(
            ItemSpace::Bundle,
            Verdict::Trusted,
            "remote.configure",
            &["ryeos.execute.service.health/status"],
        );
        assert_eq!(
            refusal_code(admit(&kinds, &other_handler)),
            "service_descriptor_mismatch"
        );
        let no_caps = service_item(ItemSpace::Bundle, Verdict::Trusted, "health.status", &[]);
        assert_eq!(
            refusal_code(admit(&kinds, &no_caps)),
            "service_descriptor_mismatch"
        );
    }

    #[test]
    fn project_and_unsigned_services_are_refused() {
        use ryeos_engine::contracts::TrustClass as Verdict;
        let (_root, kinds) = service_kinds();
        let caps = ["ryeos.execute.service.health/status"];
        let project = service_item(ItemSpace::Project, Verdict::Trusted, "health.status", &caps);
        assert_eq!(
            refusal_code(admit(&kinds, &project)),
            "launch_space_refused"
        );
        let unsigned = service_item(ItemSpace::Bundle, Verdict::Unsigned, "health.status", &caps);
        assert_eq!(
            refusal_code(admit(&kinds, &unsigned)),
            "effective_trust_unsigned"
        );
    }

    #[test]
    fn a_service_without_a_compiled_handler_is_refused() {
        use ryeos_engine::contracts::TrustClass as Verdict;
        let (_root, kinds) = service_kinds();
        let item = service_item(
            ItemSpace::Bundle,
            Verdict::Trusted,
            "health.status",
            &["ryeos.execute.service.health/status"],
        );
        let policy = NodePolicySnapshot::from_test_records(vec![
            ryeos_node_runtime::node_policy::default_launch_trust_record(),
        ]);
        assert_eq!(
            refusal_code(admit_verified_service(&item, &kinds, &policy, &[])),
            "service_descriptor_missing"
        );
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
