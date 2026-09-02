//! Operator admission shared by node-local and remotely hosted workflows.
//!
//! The target node's local operator and an origin-bound forwarded operator are
//! distinct authorities. Only the former may cause this node to load and use
//! its local operator private key; the latter is admitted solely by verified
//! node-signed public-key grants and source-node forwarding proof.

use anyhow::{Context as _, bail};

use crate::handler_context::HandlerContext;
use crate::identity::{AuthorizedKeyPrincipalClass, NodeIdentity};
use crate::state::AppState;

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
    Ok(grant.source_file_hash)
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
}
