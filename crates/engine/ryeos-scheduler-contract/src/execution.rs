//! Durable schedule execution testimony; live grant revalidation stays App-owned.

use anyhow::{Context, Result};
use ryeos_engine::execution_contract::{
    ExecutionOwnership, ExecutionPolicy, ExecutionRecovery, ExecutionResponse, ExecutionTarget,
    ProjectExecutionPolicy,
};
use ryeos_engine::principal_contract::{AuthenticatedGrantAuthority, AuthorizedKeyPrincipalClass};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleExecution {
    pub authority: ScheduleExecutionAuthority,
    pub capabilities: Vec<String>,
    pub policy: ExecutionPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleExecutionAuthority {
    Node {
        principal_id: String,
        effective_origin_site_id: String,
    },
    Authenticated {
        principal_id: String,
        principal_class: AuthorizedKeyPrincipalClass,
        effective_origin_site_id: String,
        grant_authority: AuthenticatedGrantAuthority,
        /// Canonical digest of the typed schedule-registration request. This
        /// is semantic request identity, not the ingress nonce or signature.
        registration_request_hash: String,
    },
}

impl ScheduleExecutionAuthority {
    pub fn principal_id(&self) -> &str {
        match self {
            Self::Node { principal_id, .. } | Self::Authenticated { principal_id, .. } => {
                principal_id
            }
        }
    }

    pub fn origin_site_id(&self) -> &str {
        match self {
            Self::Node {
                effective_origin_site_id,
                ..
            }
            | Self::Authenticated {
                effective_origin_site_id,
                ..
            } => effective_origin_site_id,
        }
    }
}

impl ScheduleExecution {
    pub fn principal_id(&self) -> &str {
        self.authority.principal_id()
    }
}

fn require_non_empty(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("schedule {field} must be non-empty");
    }
    Ok(())
}

pub fn validate_schedule_ref_bindings(ref_bindings: &BTreeMap<String, String>) -> Result<()> {
    if ref_bindings.len() > 32 {
        anyhow::bail!("schedule ref_bindings exceeds the limit of 32");
    }
    for (name, raw_ref) in ref_bindings {
        let mut segments = name.split('_');
        let valid_name = !name.is_empty()
            && name.len() <= 64
            && segments.next().is_some_and(|segment| {
                segment
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_lowercase)
                    && segment
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            })
            && segments.all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            });
        if !valid_name {
            anyhow::bail!(
                "schedule ref binding names must be lower snake case and at most 64 bytes"
            );
        }
        if raw_ref.len() > 2_048 {
            anyhow::bail!("schedule ref binding '{name}' exceeds 2048 UTF-8 bytes");
        }
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(raw_ref)
            .with_context(|| format!("schedule ref binding '{name}' is not a canonical ref"))?;
        if canonical.to_string() != *raw_ref {
            anyhow::bail!("schedule ref binding '{name}' is not in canonical form");
        }
    }
    Ok(())
}

pub fn validate_schedule_execution(execution: &ScheduleExecution) -> Result<()> {
    execution.policy.validate()?;
    if execution.policy.ownership != ExecutionOwnership::DaemonOwned
        || execution.policy.recovery != ExecutionRecovery::RestartRecoverable
        || execution.policy.response != ExecutionResponse::Accepted
        || execution.policy.target != ExecutionTarget::Here
    {
        anyhow::bail!(
            "scheduled execution must be daemon-owned, restart-recoverable, accepted, and targeted here"
        );
    }
    // Scheduling does not redefine the ordinary live-versus-pinned policy
    // distinction. A signed `live_direct` policy deliberately follows the
    // named live path; current-head and explicit-snapshot policies retain one
    // immutable authority for the fire. Capture-live remains an interactive
    // admission operation: a recurring schedule must either follow live bytes
    // explicitly or point at already-durable project state.
    if matches!(
        &execution.policy.project,
        ProjectExecutionPolicy::Pinned {
            source: ryeos_engine::execution_contract::PinnedSource::CaptureLive { .. },
            ..
        }
    ) {
        anyhow::bail!(
            "scheduled execution cannot use capture_live; select explicit live_direct, current_head, or a snapshot"
        );
    }
    if execution.capabilities.is_empty() {
        anyhow::bail!("schedule execution.capabilities must not be empty");
    }
    let mut previous: Option<&str> = None;
    for capability in &execution.capabilities {
        require_non_empty(capability, "execution.capabilities entry")?;
        if previous.is_some_and(|value| value >= capability.as_str()) {
            anyhow::bail!("schedule execution.capabilities must be sorted and unique");
        }
        previous = Some(capability);
    }
    ryeos_engine::principal_contract::validate_canonical_site_id(
        execution.authority.origin_site_id(),
    )?;
    match &execution.authority {
        ScheduleExecutionAuthority::Node { principal_id, .. } => {
            validate_principal_id(principal_id)?;
        }
        ScheduleExecutionAuthority::Authenticated {
            principal_id,
            principal_class,
            effective_origin_site_id,
            grant_authority,
            registration_request_hash,
        } => {
            validate_principal_id(principal_id)?;
            grant_authority.validate_for_class(*principal_class)?;
            for (label, hash) in [
                (
                    "principal grant",
                    grant_authority.principal_grant_hash.as_str(),
                ),
                ("registering request", registration_request_hash.as_str()),
            ] {
                validate_lower_hash(label, hash)?;
            }
            match principal_class {
                AuthorizedKeyPrincipalClass::LocalClient => {
                    if grant_authority.forwarding.is_some() {
                        anyhow::bail!(
                            "local-client schedule authority cannot carry forwarding evidence"
                        );
                    }
                }
                AuthorizedKeyPrincipalClass::RemoteNode => {
                    if grant_authority.forwarding.is_some() {
                        anyhow::bail!(
                            "remote-node schedule authority cannot carry forwarding evidence"
                        );
                    }
                }
                AuthorizedKeyPrincipalClass::RemoteOperator => {
                    let forwarding = grant_authority.forwarding.as_ref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "remote-operator schedule authority requires forwarding evidence"
                        )
                    })?;
                    validate_principal_id(&format!("fp:{}", forwarding.source_node_fingerprint))?;
                    validate_lower_hash("source-node grant", &forwarding.source_node_grant_hash)?;
                }
            }
            if matches!(principal_class, AuthorizedKeyPrincipalClass::LocalClient)
                && effective_origin_site_id.is_empty()
            {
                anyhow::bail!("local-client schedule authority has no target site identity");
            }
        }
    }
    Ok(())
}

fn validate_principal_id(value: &str) -> Result<()> {
    let fingerprint = value
        .strip_prefix("fp:")
        .ok_or_else(|| anyhow::anyhow!("schedule principal_id must begin with `fp:`"))?;
    validate_lower_hash("principal fingerprint", fingerprint)
}

fn validate_lower_hash(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        anyhow::bail!("schedule {label} must be lowercase SHA-256 hex");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execution(policy: ExecutionPolicy) -> ScheduleExecution {
        ScheduleExecution {
            authority: ScheduleExecutionAuthority::Node {
                principal_id: format!("fp:{}", "33".repeat(32)),
                effective_origin_site_id: "site:test".into(),
            },
            capabilities: vec!["ryeos.execute.directive.test/job".into()],
            policy,
        }
    }

    #[test]
    fn scheduled_execution_preserves_explicit_live_direct_lane() {
        let value = execution(ExecutionPolicy::local_live(ExecutionResponse::Accepted));
        validate_schedule_execution(&value).unwrap();
        assert!(matches!(
            value.policy.project,
            ProjectExecutionPolicy::LiveDirect { .. }
        ));
    }

    #[test]
    fn scheduled_execution_preserves_explicit_pinned_lane() {
        let value = execution(ExecutionPolicy::local_pinned_current_head(
            ExecutionResponse::Accepted,
        ));
        validate_schedule_execution(&value).unwrap();
        assert!(matches!(
            value.policy.project,
            ProjectExecutionPolicy::Pinned {
                source: ryeos_engine::execution_contract::PinnedSource::CurrentHead,
                ..
            }
        ));
    }

    #[test]
    fn scheduled_execution_rejects_interactive_capture_live_lane() {
        let value = execution(ExecutionPolicy::local_pinned_capture(
            ExecutionResponse::Accepted,
        ));
        let error = validate_schedule_execution(&value).unwrap_err();
        assert!(format!("{error:#}").contains("cannot use capture_live"));
    }

    #[test]
    fn scheduled_execution_rejects_duplicate_capabilities_and_noncanonical_principal() {
        let mut value = execution(ExecutionPolicy::local_live(ExecutionResponse::Accepted));
        value.capabilities.push(value.capabilities[0].clone());
        assert!(validate_schedule_execution(&value).is_err());
        value.capabilities.pop();
        value.authority = ScheduleExecutionAuthority::Node {
            principal_id: format!("fp:{}", "AA".repeat(32)),
            effective_origin_site_id: "site:test".into(),
        };
        assert!(validate_schedule_execution(&value).is_err());
    }

    #[test]
    fn ref_bindings_refuse_invalid_names_and_oversized_refs() {
        let mut bindings = BTreeMap::new();
        bindings.insert("Invalid".into(), "directive:test/job".into());
        assert!(validate_schedule_ref_bindings(&bindings).is_err());
        bindings.clear();
        bindings.insert("valid_name".into(), "x".repeat(2049));
        assert!(validate_schedule_ref_bindings(&bindings).is_err());
    }

    fn authenticated_execution(class: AuthorizedKeyPrincipalClass) -> ScheduleExecution {
        let mut value = execution(ExecutionPolicy::local_live(ExecutionResponse::Accepted));
        value.authority = ScheduleExecutionAuthority::Authenticated {
            principal_id: format!("fp:{}", "44".repeat(32)),
            principal_class: class,
            effective_origin_site_id: "site:test".into(),
            grant_authority: AuthenticatedGrantAuthority {
                principal_grant_hash: "55".repeat(32),
                forwarding: matches!(class, AuthorizedKeyPrincipalClass::RemoteOperator).then(
                    || ryeos_engine::principal_contract::ForwardingAuthorityEvidence {
                        source_node_fingerprint: "66".repeat(32),
                        source_node_grant_hash: "77".repeat(32),
                    },
                ),
            },
            registration_request_hash: "88".repeat(32),
        };
        value
    }

    #[test]
    fn execution_wire_keeps_tagged_authority_and_refuses_unknown_fields() {
        let value = authenticated_execution(AuthorizedKeyPrincipalClass::RemoteOperator);
        let wire = serde_json::to_value(&value).unwrap();
        assert_eq!(
            wire["authority"],
            serde_json::json!({
                "kind": "authenticated",
                "principal_id": format!("fp:{}", "44".repeat(32)),
                "principal_class": "remote_operator",
                "effective_origin_site_id": "site:test",
                "grant_authority": {
                    "principal_grant_hash": "55".repeat(32),
                    "forwarding": {
                        "source_node_fingerprint": "66".repeat(32),
                        "source_node_grant_hash": "77".repeat(32),
                    },
                },
                "registration_request_hash": "88".repeat(32),
            })
        );
        let decoded: ScheduleExecution = serde_json::from_value(wire.clone()).unwrap();
        validate_schedule_execution(&decoded).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire);
        let mut extra_outer = wire.clone();
        extra_outer["authenticated"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ScheduleExecution>(extra_outer).is_err());
        let mut extra_authority = wire.clone();
        extra_authority["authority"]["authorized"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ScheduleExecution>(extra_authority).is_err());
        let mut missing_tag = wire;
        missing_tag["authority"]
            .as_object_mut()
            .unwrap()
            .remove("kind");
        assert!(serde_json::from_value::<ScheduleExecution>(missing_tag).is_err());
    }

    #[test]
    fn authenticated_classes_require_their_exact_forwarding_shape() {
        for class in [
            AuthorizedKeyPrincipalClass::LocalClient,
            AuthorizedKeyPrincipalClass::RemoteNode,
            AuthorizedKeyPrincipalClass::RemoteOperator,
        ] {
            let mut value = authenticated_execution(class);
            validate_schedule_execution(&value).unwrap();
            let ScheduleExecutionAuthority::Authenticated {
                grant_authority, ..
            } = &mut value.authority
            else {
                unreachable!();
            };
            grant_authority.forwarding = if class == AuthorizedKeyPrincipalClass::RemoteOperator {
                None
            } else {
                Some(
                    ryeos_engine::principal_contract::ForwardingAuthorityEvidence {
                        source_node_fingerprint: "66".repeat(32),
                        source_node_grant_hash: "77".repeat(32),
                    },
                )
            };
            assert!(
                validate_schedule_execution(&value).is_err(),
                "class {class:?}"
            );
        }
    }

    #[test]
    fn authenticated_testimony_refuses_noncanonical_hashes_and_site() {
        let valid = authenticated_execution(AuthorizedKeyPrincipalClass::RemoteOperator);
        for field in [
            "principal_id",
            "registration_request_hash",
            "effective_origin_site_id",
        ] {
            let mut wire = serde_json::to_value(&valid).unwrap();
            wire["authority"][field] = match field {
                "principal_id" => serde_json::json!(format!("fp:{}", "AA".repeat(32))),
                "registration_request_hash" => serde_json::json!("AA".repeat(32)),
                _ => serde_json::json!("test"),
            };
            let decoded: ScheduleExecution = serde_json::from_value(wire).unwrap();
            assert!(
                validate_schedule_execution(&decoded).is_err(),
                "field {field}"
            );
        }
        for field in ["source_node_fingerprint", "source_node_grant_hash"] {
            let mut wire = serde_json::to_value(&valid).unwrap();
            wire["authority"]["grant_authority"]["forwarding"][field] =
                serde_json::json!("AA".repeat(32));
            let decoded: ScheduleExecution = serde_json::from_value(wire).unwrap();
            assert!(
                validate_schedule_execution(&decoded).is_err(),
                "forwarding {field}"
            );
        }
        let mut wire = serde_json::to_value(&valid).unwrap();
        wire["authority"]["grant_authority"]["principal_grant_hash"] = serde_json::json!("short");
        let decoded: ScheduleExecution = serde_json::from_value(wire).unwrap();
        assert!(validate_schedule_execution(&decoded).is_err());
    }
}
