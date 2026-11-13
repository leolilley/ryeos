//! Shared issuance rules for handlers that write or renew a node-signed
//! authorized-key grant: the lifetime comes from the node's signed
//! authorization policy, the provenance from the authenticated caller, and
//! every refusal is typed and names its cure.

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_node_runtime::identity::{
    AuthorizedBy, AuthorizedKeyRenewError, GrantExpiry, GrantLifetimeError, GrantTerms,
};
use ryeos_node_runtime::node_policy::sections::authorization::NodeAuthorizationPolicy;

use crate::handler_error::HandlerError;

/// The node's compiled authorization policy.
pub(crate) fn authorization_policy(
    state: &AppState,
) -> Result<NodeAuthorizationPolicy, HandlerError> {
    state
        .node_policy
        .require::<NodeAuthorizationPolicy>()
        .copied()
        .map_err(|error| HandlerError::Internal(format!("load authorization policy: {error:#}")))
}

/// Who asked for a grant: the authenticated caller and the signed request
/// that carried the ask.
pub(crate) fn caller_authorized_by(context: &HandlerContext) -> AuthorizedBy {
    AuthorizedBy::principal_request(&context.fingerprint, context.signed_request_digest.clone())
}

/// A lifetime request the policy refuses, as a typed 400.
pub(crate) fn lifetime_refusal(error: GrantLifetimeError) -> HandlerError {
    HandlerError::Structured {
        code: error.code().to_owned(),
        status: 400,
        body: serde_json::json!({
            "error": error.to_string(),
            "error_code": error.code(),
        }),
    }
}

/// The refusal for any online attempt to write the configured operator's
/// grant: that grant never lapses and is managed by init and the offline
/// authorization reset, so rewriting it here would silently bound it.
pub(crate) fn operator_grant_refusal() -> HandlerError {
    let message = "the configured operator's grant is managed by `ryeos init` and \
                   `ryeos node reset authorization`; it cannot be written or reclassified \
                   through authorize-key or authorize-client";
    HandlerError::Structured {
        code: "grant_is_operator".to_owned(),
        status: 409,
        body: serde_json::json!({
            "error": message,
            "error_code": "grant_is_operator",
        }),
    }
}

/// A renewal the node refuses, as a typed refusal: 404 for a missing grant,
/// 403 for a caller who may not renew it, 400 for the operator grant.
pub(crate) fn renewal_refusal(error: &AuthorizedKeyRenewError) -> HandlerError {
    let status = match error {
        AuthorizedKeyRenewError::NotFound { .. } => 404,
        AuthorizedKeyRenewError::NotGranterOrOperator { .. }
        | AuthorizedKeyRenewError::ScopesNoLongerCovered { .. } => 403,
        AuthorizedKeyRenewError::NeverExpires => 400,
    };
    HandlerError::Structured {
        code: error.code().to_owned(),
        status,
        body: serde_json::json!({
            "error": error.to_string(),
            "error_code": error.code(),
        }),
    }
}

/// Terms for a new grant of `class` asked for by `authorized_by`, for
/// `lifetime_secs` or the class maximum.
pub(crate) fn issuance_terms(
    state: &AppState,
    class: ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass,
    lifetime_secs: Option<u64>,
    authorized_by: AuthorizedBy,
) -> Result<GrantTerms, HandlerError> {
    let policy = authorization_policy(state)?;
    let expires_at = policy
        .expiry_for(class, lifetime_secs, lillux::time::unix_secs_now())
        .map_err(lifetime_refusal)?;
    Ok(GrantTerms {
        expires_at,
        authorized_by,
    })
}

/// A grant expiry as the text responses carry.
pub(crate) fn expiry_text(expiry: GrantExpiry) -> String {
    expiry.to_grant_text()
}
