//! Revalidation of durable scheduler delegation testimony.
//!
//! A schedule stores grant generations, not transport signatures or bearer
//! credentials. Every fire reopens the current node-signed grants and proves
//! they are still the exact generations admitted at registration.

use anyhow::{Context as _, bail};
use ryeos_runtime::authorizer::AuthorizationPolicy;
use ryeos_scheduler::types::{ScheduleExecution, ScheduleExecutionAuthority};

use crate::handler_context::HandlerContext;
use crate::identity::{
    AuthorizedKeyPrincipalClass, FORWARDED_OPERATOR_ATTESTATION_SCOPE, load_verified_authorized_key,
};
use crate::state::AppState;

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
                bail!("node-owned schedule authority no longer identifies this node");
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
            grant_authority.validate_for_class(*principal_class)?;
            let fingerprint = principal_id
                .strip_prefix("fp:")
                .context("scheduled principal is not canonical")?;
            let grant = load_verified_authorized_key(
                fingerprint,
                &state.config.authorized_keys_dir,
                &state.identity,
            )?
            .context("scheduled principal grant was revoked")?;
            if grant.source_file_hash != grant_authority.principal_grant_hash
                || grant.principal_class != *principal_class
            {
                bail!("scheduled principal grant generation or class changed");
            }
            match principal_class {
                AuthorizedKeyPrincipalClass::LocalClient => {
                    if grant.configured_origin_site_id.is_some()
                        || effective_origin_site_id != state.threads.site_id()
                    {
                        bail!("scheduled local-client origin authority changed");
                    }
                }
                AuthorizedKeyPrincipalClass::RemoteNode
                | AuthorizedKeyPrincipalClass::RemoteOperator => {
                    if grant.configured_origin_site_id.as_deref()
                        != Some(effective_origin_site_id.as_str())
                    {
                        bail!("scheduled remote principal origin authority changed");
                    }
                }
            }
            for capability in &execution.capabilities {
                state
                    .authorizer
                    .authorize(
                        &grant.scopes,
                        &AuthorizationPolicy::require(capability),
                    )
                    .map_err(|_| anyhow::anyhow!(
                        "scheduled capability {capability:?} is no longer covered by the principal grant"
                    ))?;
            }
            if let Some(forwarding) = &grant_authority.forwarding {
                let source = load_verified_authorized_key(
                    &forwarding.source_node_fingerprint,
                    &state.config.authorized_keys_dir,
                    &state.identity,
                )?
                .context("scheduled forwarding-node grant was revoked")?;
                if source.source_file_hash != forwarding.source_node_grant_hash
                    || source.principal_class != AuthorizedKeyPrincipalClass::RemoteNode
                    || source.configured_origin_site_id.as_deref()
                        != Some(effective_origin_site_id.as_str())
                {
                    bail!("scheduled forwarding-node grant generation or origin changed");
                }
                state
                    .authorizer
                    .authorize(
                        &source.scopes,
                        &AuthorizationPolicy::require(FORWARDED_OPERATOR_ATTESTATION_SCOPE),
                    )
                    .map_err(|_| anyhow::anyhow!(
                        "scheduled forwarding-node grant lost forwarded-operator attestation authority"
                    ))?;
            }
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
