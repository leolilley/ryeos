//! Unified capability authorization.
//!
//! Single evaluator for all capability checks across the system.
//! Replaces the former `capability_tokens` module. One implementation
//! of matching logic (exact, `*`, `?`, prefix wildcards) with
//! verb-implication expansion via `VerbRegistry`.
//!
//! Wire format: `rye.<verb>.<kind>.<subject>`
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use ryeos_runtime::verb_registry::VerbRegistry;
//! use ryeos_runtime::authorizer::{Authorizer, AuthorizationPolicy};
//!
//! let registry = Arc::new(VerbRegistry::with_builtins());
//! let authorizer = Authorizer::new(registry);
//!
//! let policy = AuthorizationPolicy::require_all(&["rye.execute.service.bundle/install"]);
//! let scopes = vec!["rye.execute.service.*".to_string()];
//!
//! assert!(authorizer.authorize(&scopes, &policy).is_ok());
//! ```

use std::sync::Arc;

use regex::Regex;

use crate::verb_registry::VerbRegistry;

// ── Capability struct ─────────────────────────────────────────────────

/// Parsed capability string in `rye.<verb>.<kind>.<subject>` format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub verb: String,
    pub kind: String,
    pub subject: String,
}

/// Error from parsing an invalid capability string.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CapabilityParseError {
    #[error("invalid capability format: expected 'rye.<verb>.<kind>.<subject>', got '{0}'")]
    InvalidFormat(String),
}

impl Capability {
    /// Parse a capability string into structured parts.
    ///
    /// `rye.execute.service.bundle/install` →
    /// `Capability { verb: "execute", kind: "service", subject: "bundle/install" }`
    pub fn parse(s: &str) -> Result<Self, CapabilityParseError> {
        let parts: Vec<&str> = s.splitn(4, '.').collect();
        if parts.len() < 4 || parts[0] != "rye" {
            return Err(CapabilityParseError::InvalidFormat(s.to_string()));
        }
        Ok(Capability {
            verb: parts[1].to_string(),
            kind: parts[2].to_string(),
            subject: parts[3].to_string(),
        })
    }

    /// Render back to wire format.
    pub fn to_string(&self) -> String {
        format!("rye.{}.{}.{}", self.verb, self.kind, self.subject)
    }
}

/// Derive a canonical capability string from an executable ref.
///
/// `canonical_cap("service", "bundle/install", "execute")` →
/// `"rye.execute.service.bundle/install"`
///
/// Subject uses `/` separators from the ref path, not `.` — this avoids
/// ambiguity with the `.` namespace separator. `*` wildcards match `/`.
pub fn canonical_cap(kind: &str, ref_path: &str, verb: &str) -> String {
    format!("rye.{}.{}.{}", verb, kind, ref_path)
}

// ── Authorization policy ─────────────────────────────────────────────

/// Authorization policy for a protected resource.
///
/// AND-of-ORs: all clauses must pass, any grant per clause suffices.
#[derive(Debug, Clone)]
pub struct AuthorizationPolicy {
    pub public: bool,
    pub all_of: Vec<CapabilityClause>,
}

/// A single OR-clause: any of these caps satisfies the clause.
#[derive(Debug, Clone)]
pub struct CapabilityClause {
    pub any_of: Vec<String>,
}

impl AuthorizationPolicy {
    /// Public resources — no authorization required.
    pub fn public() -> Self {
        Self {
            public: true,
            all_of: vec![],
        }
    }

    /// Require ALL listed caps. Each cap becomes its own AND clause
    /// (a single-element `any_of` vector).
    pub fn require_all(caps: &[&str]) -> Self {
        Self {
            public: false,
            all_of: caps
                .iter()
                .map(|cap| CapabilityClause {
                    any_of: vec![(*cap).to_string()],
                })
                .collect(),
        }
    }
}

// ── Authorization error ──────────────────────────────────────────────

/// Authorization failure detail.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AuthorizationError {
    #[error("unauthorized: missing required capabilities")]
    Unauthorized,
}

// ── Authorizer ───────────────────────────────────────────────────────

/// Unified capability evaluator. One implementation of matching logic
/// for the entire system.
///
/// Wraps a `VerbRegistry` for implication expansion (execute → fetch,
/// sign → fetch). Matching supports: exact match, `*` (any sequence),
/// `?` (single char), and prefix wildcards (`rye.execute.service.*`).
pub struct Authorizer {
    verbs: Arc<VerbRegistry>,
}

impl Authorizer {
    pub fn new(verbs: Arc<VerbRegistry>) -> Self {
        Self { verbs }
    }

    /// Authorize a principal's scopes against a policy.
    ///
    /// Returns `Ok(())` if all clauses are satisfied, `Err(Unauthorized)` otherwise.
    /// Public policies always pass. Empty scopes with non-public policies always fail.
    pub fn authorize(
        &self,
        principal_scopes: &[String],
        policy: &AuthorizationPolicy,
    ) -> Result<(), AuthorizationError> {
        if policy.public {
            return Ok(());
        }
        for clause in &policy.all_of {
            let satisfied = clause
                .any_of
                .iter()
                .any(|req| self.check_single(principal_scopes, req));
            if !satisfied {
                return Err(AuthorizationError::Unauthorized);
            }
        }
        Ok(())
    }

    /// Check a single required capability against granted scopes.
    fn check_single(&self, scopes: &[String], required: &str) -> bool {
        let expanded = self.expand_required(required);
        expanded
            .iter()
            .any(|req| scopes.iter().any(|g| cap_matches(g, req)))
    }

    /// Expand a required capability using verb implications.
    ///
    /// If required is `rye.fetch.service.x`, and the registry says
    /// `execute` implies `fetch`, then also check `rye.execute.service.x`.
    /// This means holding `rye.execute.service.x` satisfies `rye.fetch.service.x`.
    ///
    /// For wildcard globals like `rye.*`, expand to all known verb variants
    /// (`rye.execute.*`, `rye.fetch.*`, `rye.sign.*`).
    fn expand_required(&self, required: &str) -> Vec<String> {
        // Fast path: bare `*` or non-rye cap — no expansion possible.
        if required == "*" || !required.starts_with("rye.") {
            return vec![required.to_string()];
        }

        // Try to parse as a structured cap.
        let Ok(cap) = Capability::parse(required) else {
            return vec![required.to_string()];
        };

        // Handle `rye.*` → expand to `rye.<each verb>.*`
        if cap.verb == "*" {
            let mut result = vec![required.to_string()];
            for verb in self.verbs.verb_names() {
                result.push(format!("rye.{}.{}.{}", verb, cap.kind, cap.subject));
            }
            return result;
        }

        // Standard expansion: find all verbs that imply the required verb.
        // If required is `rye.fetch.service.x`, we want to also accept
        // `rye.execute.service.x` (because execute implies fetch).
        let mut result = vec![required.to_string()];

        for verb_name in self.verbs.verb_names() {
            let implied = self.verbs.implied_verbs(verb_name);
            if implied.contains(&cap.verb) {
                result.push(format!(
                    "rye.{}.{}.{}",
                    verb_name, cap.kind, cap.subject
                ));
            }
        }

        result
    }
}

// ── Pattern matching (extracted from capability_tokens.rs) ───────────

/// Match a granted capability pattern against a required capability string.
///
/// Supports:
/// - Exact match (`rye.execute.service.x` matches itself)
/// - Global wildcard (`*` matches everything)
/// - Prefix wildcard (`rye.execute.service.*` matches `rye.execute.service.bundle/install`)
/// - Single-char wildcard (`?` matches exactly one character)
///
/// Special regex chars in the granted pattern are escaped; only `*` and `?`
/// are treated as wildcards.
pub fn cap_matches(granted: &str, required: &str) -> bool {
    if granted == required {
        return true;
    }
    let mut regex_str = String::from("^");
    for ch in granted.chars() {
        match ch {
            '*' => regex_str.push_str(".*"),
            '?' => regex_str.push('.'),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                regex_str.push('\\');
                regex_str.push(ch);
            }
            _ => regex_str.push(ch),
        }
    }
    regex_str.push('$');
    Regex::new(&regex_str)
        .map(|re| re.is_match(required))
        .unwrap_or_else(|e| {
            tracing::warn!(
                granted = %granted,
                "capability pattern produced invalid regex: {e}"
            );
            false
        })
}

/// Expand a set of granted capabilities using verb implications.
///
/// This is the counterpart to `Authorizer::expand_required` but works on
/// the *granted* side: if you have `rye.execute.*`, you also effectively
/// have `rye.fetch.*` (because execute implies fetch).
///
/// Provided as a standalone function for callers that need the expanded
/// set directly (e.g. audit logging of effective caps).
pub fn expand_capabilities(
    caps: &[String],
    verbs: &VerbRegistry,
) -> std::collections::BTreeSet<String> {
    let mut expanded: std::collections::BTreeSet<String> = caps.iter().cloned().collect();

    let mut to_add = Vec::new();
    for cap in caps {
        if cap == "rye.*" {
            for verb in verbs.verb_names() {
                to_add.push(format!("rye.{}.*", verb));
            }
        } else if let Some(suffix) = cap.strip_prefix("rye.execute.") {
            // execute implies fetch: rye.execute.X → rye.fetch.X
            to_add.push(format!("rye.fetch.{suffix}"));
        } else if let Some(suffix) = cap.strip_prefix("rye.sign.") {
            // sign implies fetch: rye.sign.X → rye.fetch.X
            to_add.push(format!("rye.fetch.{suffix}"));
        }
    }

    for cap in to_add {
        tracing::trace!(raw = %"*", expanded = %cap, "expanded capability");
        expanded.insert(cap);
    }
    expanded
}

/// Convenience: check a single required cap against a set of granted caps.
///
/// Uses the `VerbRegistry` from `with_builtins()` for implication expansion.
/// For callers that don't have an `Authorizer` instance handy (e.g. runtime
/// subprocesses that receive `effective_caps` via the launch envelope).
pub fn check_capability(granted_caps: &[String], required_cap: &str) -> bool {
    let verbs = VerbRegistry::with_builtins();
    let expanded = expand_capabilities(granted_caps, &verbs);
    tracing::trace!(required = %required_cap, granted = ?expanded, "checking capability");
    expanded.iter().any(|g| cap_matches(g, required_cap))
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_authorizer() -> Authorizer {
        Authorizer::new(Arc::new(VerbRegistry::with_builtins()))
    }

    // ── Capability parsing ────────────────────────────────────────

    #[test]
    fn capability_parse_round_trip() {
        let cap = Capability::parse("rye.execute.service.bundle/install").unwrap();
        assert_eq!(cap.verb, "execute");
        assert_eq!(cap.kind, "service");
        assert_eq!(cap.subject, "bundle/install");
        assert_eq!(cap.to_string(), "rye.execute.service.bundle/install");
    }

    #[test]
    fn capability_parse_rejects_non_rye() {
        assert!(Capability::parse("node.maintenance").is_err());
        assert!(Capability::parse("commands.submit").is_err());
    }

    #[test]
    fn capability_parse_rejects_too_short() {
        assert!(Capability::parse("rye.execute").is_err());
        assert!(Capability::parse("rye").is_err());
    }

    #[test]
    fn canonical_cap_derivation() {
        assert_eq!(
            canonical_cap("service", "bundle/install", "execute"),
            "rye.execute.service.bundle/install"
        );
        assert_eq!(
            canonical_cap("tool", "rye/file-system/read", "execute"),
            "rye.execute.tool.rye/file-system/read"
        );
    }

    // ── cap_matches (pattern matching) ────────────────────────────

    #[test]
    fn exact_match() {
        assert!(cap_matches(
            "rye.execute.service.threads.get",
            "rye.execute.service.threads.get"
        ));
    }

    #[test]
    fn no_match_denied() {
        assert!(!cap_matches(
            "rye.execute.service.threads.get",
            "rye.execute.service.threads.list"
        ));
    }

    #[test]
    fn global_wildcard() {
        assert!(cap_matches("*", "rye.execute.service.threads.get"));
        assert!(cap_matches("*", "anything.at.all"));
    }

    #[test]
    fn prefix_wildcard() {
        assert!(cap_matches(
            "rye.execute.service.*",
            "rye.execute.service.bundle/install"
        ));
        assert!(cap_matches(
            "rye.execute.service.*",
            "rye.execute.service.threads.get"
        ));
    }

    #[test]
    fn prefix_respects_kind() {
        assert!(!cap_matches(
            "rye.execute.service.*",
            "rye.execute.tool.rye.file-system.read"
        ));
    }

    #[test]
    fn wildcard_does_not_cross_boundaries_without_star() {
        assert!(!cap_matches("rye.execute", "rye.execute.tool.foo"));
        assert!(!cap_matches("rye.fetch", "rye.fetch.tool.bar"));
    }

    #[test]
    fn different_namespace_no_match() {
        assert!(!cap_matches(
            "rye.fetch.*",
            "rye.execute.tool.rye.file-system.fs_write"
        ));
    }

    #[test]
    fn single_char_wildcard() {
        assert!(cap_matches(
            "rye.execute.tool.rye.?.fs_read",
            "rye.execute.tool.rye.x.fs_read"
        ));
        assert!(!cap_matches(
            "rye.execute.tool.rye.?.fs_read",
            "rye.execute.tool.rye.xx.fs_read"
        ));
    }

    #[test]
    fn full_wildcard() {
        assert!(cap_matches("rye.*", "rye.execute.tool.anything"));
        assert!(cap_matches("rye.*", "rye.fetch.directive.anything"));
        assert!(cap_matches("rye.*", "rye.sign.tool.anything"));
    }

    #[test]
    fn slash_in_subject_matches_wildcard() {
        // Key test: `/` in subject is matched by `*` wildcard
        assert!(cap_matches(
            "rye.execute.service.*",
            "rye.execute.service.bundle/install"
        ));
    }

    // ── Authorizer: implication expansion ─────────────────────────

    #[test]
    fn execute_implies_fetch() {
        let auth = test_authorizer();
        // Having `rye.execute.service.x` should satisfy `rye.fetch.service.x`
        let policy = AuthorizationPolicy::require_all(&["rye.fetch.service.x"]);
        let scopes = vec!["rye.execute.service.x".to_string()];
        assert!(auth.authorize(&scopes, &policy).is_ok());
    }

    #[test]
    fn fetch_does_not_imply_execute() {
        let auth = test_authorizer();
        // Having `rye.fetch.service.x` should NOT satisfy `rye.execute.service.x`
        let policy = AuthorizationPolicy::require_all(&["rye.execute.service.x"]);
        let scopes = vec!["rye.fetch.service.x".to_string()];
        assert!(auth.authorize(&scopes, &policy).is_err());
    }

    #[test]
    fn sign_implies_fetch() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require_all(&["rye.fetch.tool.x"]);
        let scopes = vec!["rye.sign.tool.x".to_string()];
        assert!(auth.authorize(&scopes, &policy).is_ok());
    }

    #[test]
    fn wildcard_grant_satisfies_all() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require_all(&["rye.execute.service.bundle/install"]);
        let scopes = vec!["*".to_string()];
        assert!(auth.authorize(&scopes, &policy).is_ok());
    }

    #[test]
    fn prefix_wildcard_grant() {
        let auth = test_authorizer();
        let policy =
            AuthorizationPolicy::require_all(&["rye.execute.service.bundle/install"]);
        let scopes = vec!["rye.execute.service.*".to_string()];
        assert!(auth.authorize(&scopes, &policy).is_ok());
    }

    // ── Authorizer: policy semantics ──────────────────────────────

    #[test]
    fn public_passthrough() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::public();
        let scopes: Vec<String> = vec![];
        assert!(auth.authorize(&scopes, &policy).is_ok());
    }

    #[test]
    fn empty_scopes_denied() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require_all(&["rye.execute.service.x"]);
        let scopes: Vec<String> = vec![];
        assert!(auth.authorize(&scopes, &policy).is_err());
    }

    #[test]
    fn multiple_required_all_must_pass() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require_all(&[
            "rye.execute.service.a",
            "rye.execute.service.b",
        ]);
        let scopes = vec![
            "rye.execute.service.a".to_string(),
            "rye.execute.service.b".to_string(),
        ];
        assert!(auth.authorize(&scopes, &policy).is_ok());
    }

    #[test]
    fn multiple_required_one_missing_denies() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require_all(&[
            "rye.execute.service.a",
            "rye.execute.service.b",
        ]);
        let scopes = vec!["rye.execute.service.a".to_string()];
        assert!(auth.authorize(&scopes, &policy).is_err());
    }

    #[test]
    fn no_capabilities_denies_all() {
        let granted: Vec<String> = vec![];
        assert!(!check_capability(&granted, "rye.execute.tool.anything"));
    }

    // ── expand_capabilities (standalone) ──────────────────────────

    #[test]
    fn expand_execute_yields_fetch() {
        let verbs = VerbRegistry::with_builtins();
        let caps = vec!["rye.execute.*".to_string()];
        let expanded = expand_capabilities(&caps, &verbs);
        assert!(expanded.contains("rye.fetch.*"));
    }

    #[test]
    fn expand_sign_yields_fetch() {
        let verbs = VerbRegistry::with_builtins();
        let caps = vec!["rye.sign.tool.foo".to_string()];
        let expanded = expand_capabilities(&caps, &verbs);
        assert!(expanded.contains("rye.fetch.tool.foo"));
    }

    #[test]
    fn expand_rye_wildcard_yields_all_verbs() {
        let verbs = VerbRegistry::with_builtins();
        let caps = vec!["rye.*".to_string()];
        let expanded = expand_capabilities(&caps, &verbs);
        assert!(expanded.contains("rye.execute.*"));
        assert!(expanded.contains("rye.fetch.*"));
        assert!(expanded.contains("rye.sign.*"));
    }

    // ── check_capability (convenience) ────────────────────────────

    #[test]
    fn check_capability_uses_expansion() {
        let granted = vec!["rye.execute.*".to_string()];
        assert!(check_capability(
            &granted,
            "rye.fetch.tool.rye.file-system.fs_read"
        ));
    }

    #[test]
    fn check_capability_exact_match() {
        let granted = vec!["rye.fetch.tool.rye.file-system.fs_read".to_string()];
        assert!(check_capability(
            &granted,
            "rye.fetch.tool.rye.file-system.fs_read"
        ));
    }

    // ── AuthorizationPolicy::require_all edge cases ───────────────

    #[test]
    fn require_all_empty_is_non_public() {
        let policy = AuthorizationPolicy::require_all(&[]);
        assert!(!policy.public);
        // Empty all_of means no clauses to fail → should pass
        let auth = test_authorizer();
        let scopes: Vec<String> = vec![];
        assert!(auth.authorize(&scopes, &policy).is_ok());
    }
}
