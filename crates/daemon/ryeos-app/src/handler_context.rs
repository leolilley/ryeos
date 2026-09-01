//! Typed handler context — the single entry point for caller identity.
//!
//! Passed as a typed parameter to every service handler. Not injected
//! into the request JSON — handlers receive it as a second argument
//! alongside `(Value, HandlerContext, Arc<AppState>)`.
//!
//! ## Fail-closed semantics
//!
//! `verified` is `true` **only** when the caller identity was
//! cryptographically verified (Ed25519 signature on the request).
//! Unauthenticated routes produce `verified: false` with a synthetic
//! fingerprint — handlers must check `verified` before trusting the
//! identity for ownership or authorization decisions.
//!
//! ## Ownership semantics
//!
//! - `require_owner()` asserts `verified == true` — unverified contexts
//!   always get `HandlerError::NotFound` (never 403).
//! - Owner check: fingerprint matches the resource owner.
//! - Not-found-never-forbidden: `require_owner()` returns
//!   `HandlerError::NotFound` (not 403) to avoid leaking resource
//!   existence to non-owners.

use crate::handler_error::HandlerError;

/// Typed caller context, passed to every service handler.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandlerContext {
    /// Fingerprint of the authenticated caller. Empty when unauthenticated.
    pub fingerprint: String,
    /// Capability scopes granted to the caller. Empty when unauthenticated.
    pub scopes: Vec<String>,
    /// Whether the caller identity was cryptographically verified.
    /// `true` only for signed-request auth (ryeos_signed, hmac).
    /// `false` for anonymous routes and synthetic principals.
    pub verified: bool,
    /// Closed class from the node-signed authorized-key grant. `None` for
    /// non-RyeOS verifiers and synthetic contexts.
    pub authorized_key_class: Option<crate::identity::AuthorizedKeyPrincipalClass>,
    /// Site identity established by a remote-node request, or by a
    /// remote-operator request plus its source-node co-signature. Request
    /// payloads never populate this field. `None` denotes a local or non-RyeOS
    /// caller.
    pub authenticated_origin_site_id: Option<String>,
}

impl HandlerContext {
    /// Construct from a principal's identity fields.
    pub fn new(fingerprint: String, scopes: Vec<String>, verified: bool) -> Self {
        Self {
            fingerprint,
            scopes,
            verified,
            authorized_key_class: None,
            authenticated_origin_site_id: None,
        }
    }

    pub fn new_with_origin(
        fingerprint: String,
        scopes: Vec<String>,
        verified: bool,
        authenticated_origin_site_id: Option<String>,
    ) -> Self {
        Self {
            fingerprint,
            scopes,
            verified,
            authorized_key_class: None,
            authenticated_origin_site_id,
        }
    }

    pub fn new_with_authority(
        fingerprint: String,
        scopes: Vec<String>,
        verified: bool,
        authorized_key_class: Option<crate::identity::AuthorizedKeyPrincipalClass>,
        authenticated_origin_site_id: Option<String>,
    ) -> Self {
        Self {
            fingerprint,
            scopes,
            verified,
            authorized_key_class,
            authenticated_origin_site_id,
        }
    }

    /// Resolve execution origin from authenticated handler authority only.
    pub fn execution_origin(&self, current_site_id: &str) -> String {
        self.authenticated_origin_site_id
            .as_ref()
            .filter(|_| self.verified)
            .cloned()
            .unwrap_or_else(|| current_site_id.to_string())
    }

    /// Prove that this transport-authenticated context is the exact authority
    /// represented by a sealed execution principal and site pair. Scope order
    /// is not authority; duplicate or reordered grants compare canonically.
    pub fn validate_execution_authority(
        &self,
        principal: &str,
        scopes: &[String],
        current_site_id: &str,
        origin_site_id: &str,
    ) -> anyhow::Result<()> {
        if self.fingerprint != principal {
            anyhow::bail!("handler fingerprint differs from sealed execution principal");
        }
        let mut expected_scopes = scopes.to_vec();
        expected_scopes.sort();
        expected_scopes.dedup();
        let mut actual_scopes = self.scopes.clone();
        actual_scopes.sort();
        actual_scopes.dedup();
        if actual_scopes != expected_scopes {
            anyhow::bail!("handler scopes differ from sealed execution scopes");
        }
        if (!self.verified)
            && (self.authorized_key_class.is_some() || self.authenticated_origin_site_id.is_some())
        {
            anyhow::bail!("unverified handler context carries authenticated execution authority");
        }
        match self.authorized_key_class {
            Some(crate::identity::AuthorizedKeyPrincipalClass::LocalClient)
                if self.authenticated_origin_site_id.is_some() =>
            {
                anyhow::bail!("local-client handler context carries a remote origin");
            }
            Some(
                crate::identity::AuthorizedKeyPrincipalClass::RemoteNode
                | crate::identity::AuthorizedKeyPrincipalClass::RemoteOperator,
            ) if self.authenticated_origin_site_id.is_none() => {
                anyhow::bail!("remote handler context has no authenticated origin");
            }
            _ => {}
        }
        if let Some(origin) = self.authenticated_origin_site_id.as_deref() {
            crate::identity::validate_canonical_site_id(origin)?;
        }
        if self.execution_origin(current_site_id) != origin_site_id {
            anyhow::bail!("handler origin differs from sealed execution origin");
        }
        Ok(())
    }

    /// Narrow only the capability vector while preserving the exact
    /// authenticated principal class and origin established at ingress.
    pub fn narrowed_for_execution(
        &self,
        scopes: Vec<String>,
        current_site_id: &str,
        origin_site_id: &str,
    ) -> anyhow::Result<Self> {
        let mut narrowed = self.clone();
        narrowed.scopes = scopes;
        narrowed.validate_execution_authority(
            &narrowed.fingerprint,
            &narrowed.scopes,
            current_site_id,
            origin_site_id,
        )?;
        Ok(narrowed)
    }

    /// Anonymous context — no identity, no scopes, not verified.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// Returns true when a verified fingerprint is present.
    ///
    /// Unlike checking `!fingerprint.is_empty()`, this requires
    /// `verified == true`, so synthetic principals from anonymous routes
    /// are correctly excluded.
    pub fn is_present(&self) -> bool {
        self.verified && !self.fingerprint.is_empty()
    }

    /// Returns `Ok(())` when the caller is cryptographically verified.
    ///
    /// Use this as a guard at the top of handlers that read
    /// `fingerprint` or `scopes` directly (not through `require_owner`).
    /// Returns `HandlerError::BadRequest` with a clear message.
    pub fn require_verified(&self) -> Result<(), HandlerError> {
        if self.verified && !self.fingerprint.is_empty() {
            Ok(())
        } else {
            Err(HandlerError::BadRequest(
                "verified caller context required".to_string(),
            ))
        }
    }

    /// Returns true when the caller is verified and matches the resource owner.
    pub fn is_owner(&self, owner: Option<&str>) -> bool {
        if !self.verified {
            return false;
        }
        match owner {
            Some(fp) => fp == self.fingerprint,
            None => false,
        }
    }

    /// Returns `Ok(())` when the caller is verified and is the owner.
    ///
    /// Returns `HandlerError::NotFound` on miss (never 403) to avoid
    /// leaking resource existence to non-owners.
    pub fn require_owner(&self, owner: Option<&str>) -> Result<(), HandlerError> {
        if self.is_owner(owner) {
            Ok(())
        } else {
            Err(HandlerError::NotFound)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_remote_origin_is_authoritative() {
        let context = HandlerContext::new_with_origin(
            "fp:remote".to_string(),
            Vec::new(),
            true,
            Some("site:remote".to_string()),
        );
        assert_eq!(context.execution_origin("site:local"), "site:remote");
    }

    #[test]
    fn unverified_origin_claim_is_ignored() {
        let context = HandlerContext::new_with_origin(
            "fp:synthetic".to_string(),
            Vec::new(),
            false,
            Some("site:spoofed".to_string()),
        );
        assert_eq!(context.execution_origin("site:local"), "site:local");
    }
}
