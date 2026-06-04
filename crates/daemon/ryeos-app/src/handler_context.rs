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
//! - Admin bypass: `["*"]` is the only admin scope in v1.
//! - Owner check: fingerprint matches the resource owner.
//! - Not-found-never-forbidden: `require_owner()` returns
//!   `HandlerError::NotFound` (not 403) to avoid leaking resource
//!   existence to non-owners.

use crate::handler_error::HandlerError;

/// Typed caller context, passed to every service handler.
#[derive(Debug, Clone, Default)]
pub struct HandlerContext {
    /// Fingerprint of the authenticated caller. Empty when unauthenticated.
    pub fingerprint: String,
    /// Capability scopes granted to the caller. Empty when unauthenticated.
    pub scopes: Vec<String>,
    /// Whether the caller identity was cryptographically verified.
    /// `true` only for signed-request auth (ryeos_signed, hmac).
    /// `false` for anonymous routes and synthetic principals.
    pub verified: bool,
}

impl HandlerContext {
    /// Construct from a principal's identity fields.
    pub fn new(fingerprint: String, scopes: Vec<String>, verified: bool) -> Self {
        Self {
            fingerprint,
            scopes,
            verified,
        }
    }

    /// Anonymous context — no identity, no scopes, not verified.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// Returns true when the caller holds the admin wildcard scope `["*"]`.
    pub fn is_admin(&self) -> bool {
        self.scopes.iter().any(|s| s == "*")
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

    /// Returns true when the caller is verified and is an admin or
    /// matches the resource owner.
    pub fn is_owner_or_admin(&self, owner: Option<&str>) -> bool {
        if !self.verified {
            return false;
        }
        if self.is_admin() {
            return true;
        }
        match owner {
            Some(fp) => fp == self.fingerprint,
            None => false,
        }
    }

    /// Returns `Ok(())` when the caller is verified and is the owner
    /// or an admin.
    ///
    /// Returns `HandlerError::NotFound` on miss (never 403) to avoid
    /// leaking resource existence to non-owners.
    pub fn require_owner(&self, owner: Option<&str>) -> Result<(), HandlerError> {
        if self.is_owner_or_admin(owner) {
            Ok(())
        } else {
            Err(HandlerError::NotFound)
        }
    }
}
