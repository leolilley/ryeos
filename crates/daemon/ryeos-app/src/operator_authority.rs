//! Operator admission shared by node-local and remotely hosted workflows.
//!
//! The target node's local operator and an origin-bound forwarded operator are
//! distinct authorities. Only the former may cause this node to load and use
//! its local operator private key; the latter is admitted solely by verified
//! node-signed public-key grants and source-node forwarding proof.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

use crate::handler_context::HandlerContext;
use crate::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
use crate::state::AppState;

/// Exact target-node operator grant retained by a remotely adopted execution.
/// The node-signed grant remains revocable; this value binds admission and the
/// launch capsule to the exact grant generation and target-derived scopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedOperatorAuthority {
    pub owner_principal: String,
    pub origin_site_id: String,
    pub principal_class: AuthorizedKeyPrincipalClass,
    pub grant_digest: String,
    pub scopes: Vec<String>,
}

impl AdmittedOperatorAuthority {
    pub fn validate(&self) -> anyhow::Result<()> {
        let fingerprint = self
            .owner_principal
            .strip_prefix("fp:")
            .ok_or_else(|| anyhow::anyhow!("admitted operator principal is not canonical"))?;
        if !lillux::valid_hash(fingerprint)
            || !lillux::valid_hash(&self.grant_digest)
            || self.origin_site_id.is_empty()
            || self.principal_class == AuthorizedKeyPrincipalClass::RemoteNode
        {
            bail!("admitted operator authority is not canonical");
        }
        crate::identity::validate_canonical_site_id(&self.origin_site_id)?;
        let mut scopes = self.scopes.clone();
        for scope in &scopes {
            ryeos_runtime::authorizer::validate_scope_pattern(scope)
                .map_err(|error| anyhow::anyhow!("admitted operator scope is invalid: {error}"))?;
        }
        scopes.sort();
        scopes.dedup();
        if scopes != self.scopes {
            bail!("admitted operator scopes are not canonical");
        }
        Ok(())
    }

    pub fn handler_context(&self) -> HandlerContext {
        HandlerContext::new_with_authority(
            self.owner_principal.clone(),
            self.scopes.clone(),
            true,
            Some(self.principal_class),
            (self.principal_class == AuthorizedKeyPrincipalClass::RemoteOperator)
                .then(|| self.origin_site_id.clone()),
        )
    }

    /// Require the retained launch capability ceiling to be a subset of this
    /// target grant. Pattern-to-pattern comparison is intentionally
    /// conservative: each retained grant must itself be covered by one
    /// current target grant pattern.
    pub fn require_covers(&self, retained: &[String]) -> anyhow::Result<()> {
        self.validate()?;
        for scope in retained {
            ryeos_runtime::authorizer::validate_scope_pattern(scope)
                .map_err(|error| anyhow::anyhow!("retained execution scope is invalid: {error}"))?;
            if !self
                .scopes
                .iter()
                .any(|granted| ryeos_state::capability::pattern_covers(granted, scope))
            {
                bail!("target operator grant does not cover retained execution scope `{scope}`");
            }
        }
        Ok(())
    }
}

/// Require the app root's exact local configured operator.
pub fn require_local_configured_operator(
    state: &AppState,
    context: &HandlerContext,
) -> anyhow::Result<String> {
    context
        .require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    if context.authorized_key_class != Some(AuthorizedKeyPrincipalClass::LocalClient)
        || context.authenticated_origin_site_id.is_some()
    {
        bail!("action requires a local_client configured operator");
    }
    let operator = NodeIdentity::load(&state.config.operator_signing_key_path)
        .context("load configured local operator identity")?;
    if context.fingerprint != operator.principal_id() {
        bail!("local action requires the configured local operator");
    }
    Ok(operator.fingerprint().to_owned())
}

/// Require either the exact local operator or a verified, origin-bound
/// `remote_operator`. Remote admission never reads the target-local private
/// key.
pub fn require_admitted_operator(
    state: &AppState,
    context: &HandlerContext,
) -> anyhow::Result<String> {
    context
        .require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    match (
        context.authorized_key_class,
        context.authenticated_origin_site_id.as_deref(),
    ) {
        (Some(AuthorizedKeyPrincipalClass::LocalClient), None) => {
            require_local_configured_operator(state, context)
        }
        (Some(AuthorizedKeyPrincipalClass::RemoteOperator), Some(_)) => {
            remote_operator_fingerprint(context)
        }
        (Some(AuthorizedKeyPrincipalClass::RemoteNode), _) => {
            bail!("operator actions reject remote_node grants")
        }
        (Some(AuthorizedKeyPrincipalClass::RemoteOperator), None) => {
            bail!("remote_operator request has no authenticated source-node forwarding proof")
        }
        _ => bail!("action requires an authenticated admitted-operator class"),
    }
}

fn remote_operator_fingerprint(context: &HandlerContext) -> anyhow::Result<String> {
    let fingerprint = context
        .fingerprint
        .strip_prefix("fp:")
        .ok_or_else(|| anyhow::anyhow!("remote operator principal is not canonical"))?;
    if !lillux::valid_hash(fingerprint) {
        bail!("remote operator fingerprint is not canonical");
    }
    Ok(fingerprint.to_owned())
}

/// Resolve the exact current node-signed grant behind an operator-owned
/// durable operation. Local-client grants remain bound to the app root's exact
/// local key; remote-operator grants remain bound to their configured origin.
pub fn admitted_operator_authority_digest(
    state: &AppState,
    operator_fingerprint: &str,
) -> anyhow::Result<String> {
    let grant = crate::identity::load_verified_authorized_key(
        operator_fingerprint,
        &state.config.authorized_keys_dir,
        &state.identity,
    )?
    .ok_or_else(|| anyhow::anyhow!("operator grant was revoked"))?;
    match grant.principal_class {
        AuthorizedKeyPrincipalClass::LocalClient => {
            let local_operator = NodeIdentity::load(&state.config.operator_signing_key_path)
                .context("load configured local operator identity")?;
            if local_operator.fingerprint() != operator_fingerprint {
                bail!("local durable operation no longer belongs to the configured operator");
            }
        }
        AuthorizedKeyPrincipalClass::RemoteOperator => {
            if grant.configured_origin_site_id.is_none() {
                bail!("remote operator grant lost its configured origin site");
            }
        }
        AuthorizedKeyPrincipalClass::RemoteNode => bail!("operator grant changed to remote_node"),
    }
    Ok(grant.source_file_hash)
}

/// Revalidate the admitted owner of an already-created root before a private
/// worker is attached. The immutable root origin must still agree with the
/// current node-signed operator grant; a principal string alone is never
/// admission evidence.
pub fn retained_admitted_operator_authority_digest(
    state: &AppState,
    operator_principal: &str,
    origin_site_id: &str,
) -> anyhow::Result<String> {
    Ok(
        retained_admitted_operator_authority(state, operator_principal, origin_site_id)?
            .grant_digest,
    )
}

/// Resolve the complete exact current target grant for a retained execution
/// owner. This is the sole constructor for placement/capsule grant authority.
pub fn retained_admitted_operator_authority(
    state: &AppState,
    operator_principal: &str,
    origin_site_id: &str,
) -> anyhow::Result<AdmittedOperatorAuthority> {
    let operator_fingerprint = operator_principal
        .strip_prefix("fp:")
        .ok_or_else(|| anyhow::anyhow!("retained operator principal is not canonical"))?;
    if !lillux::valid_hash(operator_fingerprint) {
        bail!("retained operator fingerprint is not canonical");
    }
    let grant = crate::identity::load_verified_authorized_key(
        operator_fingerprint,
        &state.config.authorized_keys_dir,
        &state.identity,
    )?
    .ok_or_else(|| anyhow::anyhow!("retained operator grant was revoked"))?;
    match grant.principal_class {
        AuthorizedKeyPrincipalClass::LocalClient => {
            let local_operator = NodeIdentity::load(&state.config.operator_signing_key_path)
                .context("load configured local operator identity")?;
            if local_operator.fingerprint() != operator_fingerprint {
                bail!("retained local owner is not the configured local operator");
            }
            if origin_site_id != state.threads.site_id() {
                bail!("retained local operator root has a remote origin");
            }
        }
        AuthorizedKeyPrincipalClass::RemoteOperator => {
            if grant.configured_origin_site_id.as_deref() != Some(origin_site_id) {
                bail!("retained remote operator origin differs from its current grant");
            }
        }
        AuthorizedKeyPrincipalClass::RemoteNode => {
            bail!("retained operator grant changed to remote_node")
        }
    }
    let mut scopes = grant.scopes;
    scopes.sort();
    scopes.dedup();
    let authority = AdmittedOperatorAuthority {
        owner_principal: operator_principal.to_owned(),
        origin_site_id: origin_site_id.to_owned(),
        principal_class: grant.principal_class,
        grant_digest: grant.source_file_hash,
        scopes,
    };
    authority.validate()?;
    Ok(authority)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_operator_identity_is_derived_without_the_target_local_key() {
        let fingerprint = "a".repeat(64);
        let remote = HandlerContext::new_with_authority(
            format!("fp:{fingerprint}"),
            vec!["ryeos.execute.service.fixture".to_owned()],
            true,
            Some(AuthorizedKeyPrincipalClass::RemoteOperator),
            Some("site:source".to_owned()),
        );
        assert_eq!(
            remote_operator_fingerprint(&remote).unwrap(),
            fingerprint,
            "target admission must retain the source operator fingerprint"
        );
    }

    #[test]
    fn remote_operator_requires_origin_and_canonical_principal() {
        let invalid = HandlerContext::new_with_authority(
            "fp:not-a-hash".to_owned(),
            vec![],
            true,
            Some(AuthorizedKeyPrincipalClass::RemoteOperator),
            Some("site:source".to_owned()),
        );
        assert!(remote_operator_fingerprint(&invalid).is_err());
    }

    #[test]
    fn target_grant_covers_only_retained_capability_subsets() {
        let authority = AdmittedOperatorAuthority {
            owner_principal: format!("fp:{}", "a".repeat(64)),
            origin_site_id: "site:source".to_owned(),
            principal_class: AuthorizedKeyPrincipalClass::RemoteOperator,
            grant_digest: "b".repeat(64),
            scopes: vec![
                "ryeos.execute.service.worker-executions/*".to_owned(),
                "ryeos.fetch.tool.ryeos/file-system/read".to_owned(),
            ],
        };
        authority.validate().unwrap();
        authority
            .require_covers(&[
                "ryeos.execute.service.worker-executions/command".to_owned(),
                "ryeos.fetch.tool.ryeos/file-system/read".to_owned(),
            ])
            .unwrap();
        assert!(
            authority
                .require_covers(&["ryeos.execute.service.vault/read".to_owned()])
                .is_err()
        );
        assert!(
            authority
                .require_covers(&["ryeos.execute.*".to_owned()])
                .is_err(),
            "a narrower target grant cannot inherit a broader wildcard"
        );

        let narrow_pattern = AdmittedOperatorAuthority {
            scopes: vec!["ryeos.get.vault.?".to_owned()],
            ..authority
        };
        assert!(
            narrow_pattern
                .require_covers(&["ryeos.get.vault.*".to_owned()])
                .is_err(),
            "value matching must not substitute for pattern-language containment"
        );
    }
}
