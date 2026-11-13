//! Revalidation of durable scheduler delegation testimony.
//!
//! A schedule stores grant generations, not transport signatures or bearer
//! credentials. Every fire reopens the current node-signed grants and proves
//! they are still the exact generations admitted at registration.
//!
//! When that proof fails the error carries a typed
//! [`ScheduledAuthorityRefusal`]; I/O and grant-verification failures stay
//! untyped, because they say nothing about whether authority was withdrawn.

use std::path::Path;

use anyhow::Context as _;
use ryeos_runtime::authorizer::{AuthorizationPolicy, Authorizer};
use ryeos_scheduler_contract::{
    ScheduleExecution, ScheduleExecutionAuthority, ScheduledAuthorityRefusal,
    ScheduledAuthorityRefusalKind,
};

use crate::handler_context::HandlerContext;
use crate::state::AppState;
use ryeos_node_runtime::identity::{
    AuthorizedKeyPrincipalClass, FORWARDED_OPERATOR_ATTESTATION_SCOPE, NodeIdentity,
    VerifiedAuthorizedKeyGrant, load_verified_authorized_key,
};

fn refused(kind: ScheduledAuthorityRefusalKind, detail: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ScheduledAuthorityRefusal::new(kind, detail))
}

pub fn revalidate_scheduled_execution(
    state: &AppState,
    execution: &ScheduleExecution,
) -> anyhow::Result<HandlerContext> {
    execution.policy.validate()?;
    match &execution.authority {
        ScheduleExecutionAuthority::Node {
            principal_id,
            effective_origin_site_id,
        } => {
            if principal_id != &state.identity.principal_id()
                || effective_origin_site_id != state.threads.site_id()
            {
                return Err(refused(
                    ScheduledAuthorityRefusalKind::NodeAuthorityChanged,
                    "node-owned schedule authority no longer identifies this node",
                ));
            }
            Ok(HandlerContext::new_with_authority(
                principal_id.clone(),
                execution.capabilities.clone(),
                true,
                None,
                None,
            ))
        }
        ScheduleExecutionAuthority::Authenticated {
            principal_id,
            principal_class,
            effective_origin_site_id,
            grant_authority,
            ..
        } => {
            check_authenticated_grants(
                execution,
                &state.config.authorized_keys_dir,
                &state.identity,
                state.threads.site_id(),
                &state.authorizer,
            )?;
            Ok(HandlerContext::new_with_grant_authority(
                principal_id.clone(),
                execution.capabilities.clone(),
                *principal_class,
                (principal_class.is_remote()).then(|| effective_origin_site_id.clone()),
                grant_authority.clone(),
            ))
        }
    }
}

/// The principal grant and, for a forwarded operator, the source-node grant
/// an authenticated schedule pinned.
pub type PinnedScheduleGrants = (
    VerifiedAuthorizedKeyGrant,
    Option<VerifiedAuthorizedKeyGrant>,
);

/// The current grants an authenticated schedule pinned, when they are still
/// the exact generations admitted at registration. A missing or superseded
/// generation is a typed refusal; a node-owned schedule pins no grant and
/// returns `Ok(None)`.
///
/// This is the generation check alone, without the site and capability
/// checks, so a stopped-node diagnostic can report superseded generations.
pub fn pinned_schedule_grants(
    execution: &ScheduleExecution,
    authorized_keys_dir: &Path,
    node_identity: &NodeIdentity,
) -> anyhow::Result<Option<PinnedScheduleGrants>> {
    let ScheduleExecutionAuthority::Authenticated {
        principal_id,
        principal_class,
        effective_origin_site_id,
        grant_authority,
        ..
    } = &execution.authority
    else {
        return Ok(None);
    };
    grant_authority.validate_for_class(*principal_class)?;
    let fingerprint = principal_id
        .strip_prefix("fp:")
        .context("scheduled principal is not canonical")?;
    let grant = load_verified_authorized_key(fingerprint, authorized_keys_dir, node_identity)?
        .ok_or_else(|| {
            refused(
                ScheduledAuthorityRefusalKind::GrantRevoked,
                format!("the authorized-key grant for {principal_id} no longer exists"),
            )
        })?;
    if grant.source_file_hash != grant_authority.principal_grant_hash {
        return Err(refused(
            ScheduledAuthorityRefusalKind::GrantGenerationChanged,
            format!(
                "the grant for {principal_id} is generation {}, not the pinned {}",
                grant.source_file_hash, grant_authority.principal_grant_hash
            ),
        ));
    }
    if grant.principal_class != *principal_class {
        return Err(refused(
            ScheduledAuthorityRefusalKind::ClassOrOriginChanged,
            format!(
                "the grant for {principal_id} is class {:?}, not the pinned {:?}",
                grant.principal_class, principal_class
            ),
        ));
    }
    let Some(forwarding) = &grant_authority.forwarding else {
        return Ok(Some((grant, None)));
    };
    let source = load_verified_authorized_key(
        &forwarding.source_node_fingerprint,
        authorized_keys_dir,
        node_identity,
    )?
    .ok_or_else(|| {
        refused(
            ScheduledAuthorityRefusalKind::ForwardingGrantRevoked,
            format!(
                "the forwarding node grant fp:{} no longer exists",
                forwarding.source_node_fingerprint
            ),
        )
    })?;
    if source.source_file_hash != forwarding.source_node_grant_hash
        || source.principal_class != AuthorizedKeyPrincipalClass::RemoteNode
        || source.configured_origin_site_id.as_deref() != Some(effective_origin_site_id.as_str())
    {
        return Err(refused(
            ScheduledAuthorityRefusalKind::ForwardingGrantChanged,
            format!(
                "the forwarding node grant fp:{} is not the pinned generation, class or origin",
                forwarding.source_node_fingerprint
            ),
        ));
    }
    Ok(Some((grant, Some(source))))
}

/// Prove an authenticated schedule's pinned grants still admit it on this
/// site: the exact generations, class and origin, and every declared
/// capability.
pub fn check_authenticated_grants(
    execution: &ScheduleExecution,
    authorized_keys_dir: &Path,
    node_identity: &NodeIdentity,
    site_id: &str,
    authorizer: &Authorizer,
) -> anyhow::Result<()> {
    let ScheduleExecutionAuthority::Authenticated {
        principal_class,
        effective_origin_site_id,
        ..
    } = &execution.authority
    else {
        anyhow::bail!("schedule authority is not grant-bound");
    };
    let (grant, source) = pinned_schedule_grants(execution, authorized_keys_dir, node_identity)?
        .context("authenticated schedule pinned no grant")?;
    match principal_class {
        AuthorizedKeyPrincipalClass::LocalClient => {
            if grant.configured_origin_site_id.is_some() || effective_origin_site_id != site_id {
                return Err(refused(
                    ScheduledAuthorityRefusalKind::ClassOrOriginChanged,
                    "scheduled local-client origin authority changed",
                ));
            }
        }
        AuthorizedKeyPrincipalClass::RemoteNode | AuthorizedKeyPrincipalClass::RemotePrincipal => {
            if grant.configured_origin_site_id.as_deref() != Some(effective_origin_site_id.as_str())
            {
                return Err(refused(
                    ScheduledAuthorityRefusalKind::ClassOrOriginChanged,
                    "scheduled remote principal origin authority changed",
                ));
            }
        }
    }
    for capability in &execution.capabilities {
        if authorizer
            .authorize(&grant.scopes, &AuthorizationPolicy::require(capability))
            .is_err()
        {
            return Err(refused(
                ScheduledAuthorityRefusalKind::CapabilityNoLongerCovered,
                format!(
                    "scheduled capability {capability:?} is no longer covered by the principal grant"
                ),
            ));
        }
    }
    if let Some(source) = source
        && authorizer
            .authorize(
                &source.scopes,
                &AuthorizationPolicy::require(FORWARDED_OPERATOR_ATTESTATION_SCOPE),
            )
            .is_err()
    {
        return Err(refused(
            ScheduledAuthorityRefusalKind::ForwardingAttestationNotCovered,
            "scheduled forwarding-node grant lost forwarded-operator attestation authority",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::execution_contract::{ExecutionPolicy, ExecutionResponse};
    use ryeos_engine::principal_contract::AuthenticatedGrantAuthority;

    const SITE: &str = "site:schedule-authority-test";
    const CAPABILITY: &str = "ryeos.execute.tool.test/job";

    struct Fixture {
        _root: tempfile::TempDir,
        auth_dir: std::path::PathBuf,
        node: NodeIdentity,
        principal_fp: String,
        principal_key_b64: String,
    }

    impl Fixture {
        fn new() -> Self {
            use base64::Engine as _;
            let root = tempfile::tempdir().unwrap();
            let node = NodeIdentity::create(&root.path().join("node.pem")).unwrap();
            let principal = lillux::crypto::SigningKey::from_bytes(&[0x42; 32]);
            let verifying_key = principal.verifying_key();
            Self {
                auth_dir: root.path().join("authorized_keys"),
                _root: root,
                node,
                principal_fp: lillux::signature::compute_fingerprint(&verifying_key),
                principal_key_b64: base64::engine::general_purpose::STANDARD
                    .encode(verifying_key.to_bytes()),
            }
        }

        fn grant(&self, scopes: &[&str], created_at: &str) {
            let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
            ryeos_node_runtime::identity::create_authorized_key_toml(
                &self.auth_dir,
                &self.principal_fp,
                &self.principal_key_b64,
                &scopes,
                "scheduled principal",
                &format!("fp:{}", "99".repeat(32)),
                created_at,
                &ryeos_node_runtime::identity::GrantTerms::for_tests(),
                self.node.signing_key(),
            )
            .unwrap();
        }

        fn revoke(&self) {
            std::fs::remove_file(self.auth_dir.join(format!("{}.toml", self.principal_fp)))
                .unwrap();
        }

        /// A schedule pinned to the grant generation present now.
        fn pinned_execution(&self) -> ScheduleExecution {
            let grant =
                load_verified_authorized_key(&self.principal_fp, &self.auth_dir, &self.node)
                    .unwrap()
                    .expect("grant to pin");
            ScheduleExecution {
                authority: ScheduleExecutionAuthority::Authenticated {
                    principal_id: format!("fp:{}", self.principal_fp),
                    principal_class: AuthorizedKeyPrincipalClass::LocalClient,
                    effective_origin_site_id: SITE.to_owned(),
                    grant_authority: AuthenticatedGrantAuthority {
                        principal_grant_hash: grant.source_file_hash,
                        forwarding: None,
                    },
                    registration_request_hash: "88".repeat(32),
                },
                capabilities: vec![CAPABILITY.to_owned()],
                policy: ExecutionPolicy::projectless(ExecutionResponse::Accepted),
                limits: None,
                max_fires: None,
                expires_at: None,
            }
        }

        fn check_on(&self, execution: &ScheduleExecution, site: &str) -> anyhow::Result<()> {
            check_authenticated_grants(
                execution,
                &self.auth_dir,
                &self.node,
                site,
                &Authorizer::new(),
            )
        }
    }

    fn refusal_kind(result: anyhow::Result<()>) -> ScheduledAuthorityRefusalKind {
        let error = result.expect_err("the check must refuse");
        ScheduledAuthorityRefusal::find(&error)
            .unwrap_or_else(|| panic!("untyped refusal: {error:#}"))
            .kind
    }

    #[test]
    fn current_pinned_grant_is_admitted() {
        let fixture = Fixture::new();
        fixture.grant(&[CAPABILITY], "2026-11-13T00:00:00Z");
        let execution = fixture.pinned_execution();
        fixture.check_on(&execution, SITE).unwrap();
    }

    #[test]
    fn revoked_grant_is_typed_grant_revoked() {
        let fixture = Fixture::new();
        fixture.grant(&[CAPABILITY], "2026-11-13T00:00:00Z");
        let execution = fixture.pinned_execution();
        fixture.revoke();
        let kind = refusal_kind(fixture.check_on(&execution, SITE));
        assert_eq!(kind, ScheduledAuthorityRefusalKind::GrantRevoked);
        assert_eq!(kind.fire_outcome(), "authority_revoked");
    }

    #[test]
    fn rewritten_grant_same_scopes_is_generation_changed() {
        let fixture = Fixture::new();
        fixture.grant(&[CAPABILITY], "2026-11-13T00:00:00Z");
        let execution = fixture.pinned_execution();
        // Same key, same scopes, a later grant: a new generation.
        fixture.revoke();
        fixture.grant(&[CAPABILITY], "2026-11-13T00:05:00Z");
        let kind = refusal_kind(fixture.check_on(&execution, SITE));
        assert_eq!(kind, ScheduledAuthorityRefusalKind::GrantGenerationChanged);
        assert_eq!(kind.fire_outcome(), "authority_changed");
    }

    #[test]
    fn capability_outside_the_pinned_grant_is_typed_narrowed() {
        let fixture = Fixture::new();
        fixture.grant(&[CAPABILITY], "2026-11-13T00:00:00Z");
        let mut execution = fixture.pinned_execution();
        execution.capabilities = vec!["ryeos.execute.tool.other/job".to_owned()];
        let kind = refusal_kind(fixture.check_on(&execution, SITE));
        assert_eq!(
            kind,
            ScheduledAuthorityRefusalKind::CapabilityNoLongerCovered
        );
        assert_eq!(kind.fire_outcome(), "authority_narrowed");
    }

    #[test]
    fn changed_origin_is_typed_class_or_origin_changed() {
        let fixture = Fixture::new();
        fixture.grant(&[CAPABILITY], "2026-11-13T00:00:00Z");
        let execution = fixture.pinned_execution();
        assert_eq!(
            refusal_kind(fixture.check_on(&execution, "site:another-node")),
            ScheduledAuthorityRefusalKind::ClassOrOriginChanged
        );
    }

    #[test]
    fn node_owned_schedule_pins_no_grant() {
        let fixture = Fixture::new();
        fixture.grant(&[CAPABILITY], "2026-11-13T00:00:00Z");
        let mut execution = fixture.pinned_execution();
        execution.authority = ScheduleExecutionAuthority::Node {
            principal_id: format!("fp:{}", fixture.node.fingerprint()),
            effective_origin_site_id: SITE.to_owned(),
        };
        assert!(
            pinned_schedule_grants(&execution, &fixture.auth_dir, &fixture.node)
                .unwrap()
                .is_none()
        );
    }
}
