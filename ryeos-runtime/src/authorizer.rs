//! Unified capability authorization.
//!
//! Single evaluator for all capability checks across the system.
//! One implementation of matching logic (exact, `*`, `?`, prefix wildcards).
//!
//! Wire format: `rye.<verb>.<kind>.<subject>`
//!
//! Subjects use `/` as path separators, matching canonical ref format.
//! Wildcards use `*` (matches any characters including `/`).
//! Path-prefix wildcards use `/*` (e.g., `rye.execute.service.bundle/*`
//! matches `bundle/install`, `bundle/remove`, but not `bundleX`).
//!
//! Required-side patterns support wildcards in verb, kind, and subject
//! positions. `require("rye.*")` means "any rye cap".
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use ryeos_runtime::verb_registry::VerbRegistry;
//! use ryeos_runtime::authorizer::{Authorizer, AuthorizationPolicy};
//!
//! let registry = Arc::new(VerbRegistry::from_records(&[
//!     ryeos_runtime::verb_registry::VerbDef {
//!         name: "execute".into(), execute: None,
//!     },
//!     ryeos_runtime::verb_registry::VerbDef {
//!         name: "fetch".into(), execute: None,
//!     },
//!     ryeos_runtime::verb_registry::VerbDef {
//!         name: "sign".into(),
//!         execute: Some("tool:rye/core/sign".into()),
//!     },
//! ]).unwrap());
//! let authorizer = Authorizer::new(registry);
//!
//! let policy = AuthorizationPolicy::require("rye.execute.service.bundle/install");
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
/// Subject preserves `/` separators from the ref path. Wildcards match `/`.
pub fn canonical_cap(kind: &str, ref_path: &str, verb: &str) -> String {
    format!("rye.{}.{}.{}", verb, kind, ref_path)
}

// ── RequiredPattern ──────────────────────────────────────────────────

/// A required-side capability pattern that may contain wildcards.
///
/// Used by `Authorizer::check_single` to match granted capabilities
/// against policy requirements. Supports wildcards in any position.
#[derive(Debug, Clone)]
enum RequiredPattern {
    /// `rye.*` — matches any verb, kind, subject.
    AllRye,
    /// `rye.<verb>.*` — matches any kind and subject for a specific verb.
    VerbWildcard { verb: String },
    /// `rye.<verb>.<kind>.*` — matches any subject for a specific verb and kind.
    SubjectWildcard { verb: String, kind: String },
    /// `rye.<verb>.<kind>.<subject>` — fully specified; subject may contain wildcards.
    Full {
        verb: String,
        kind: String,
        subject: String,
    },
    /// Bare `*` or non-rye string.
    Other(String),
}

impl RequiredPattern {
    fn parse(s: &str) -> Self {
        if s == "*" {
            return RequiredPattern::Other(s.to_string());
        }
        if !s.starts_with("rye.") {
            return RequiredPattern::Other(s.to_string());
        }

        let parts: Vec<&str> = s.splitn(4, '.').collect();

        match parts.len() {
            2 if parts[1] == "*" => RequiredPattern::AllRye,
            3 if parts[2] == "*" => RequiredPattern::VerbWildcard {
                verb: parts[1].to_string(),
            },
            3 => RequiredPattern::Full {
                // `rye.execute.service` without subject — treat as subject wildcard
                verb: parts[1].to_string(),
                kind: parts[2].to_string(),
                subject: "*".to_string(),
            },
            4 if parts[3] == "*" => RequiredPattern::SubjectWildcard {
                verb: parts[1].to_string(),
                kind: parts[2].to_string(),
            },
            4 => RequiredPattern::Full {
                verb: parts[1].to_string(),
                kind: parts[2].to_string(),
                subject: parts[3].to_string(),
            },
            _ => RequiredPattern::Other(s.to_string()),
        }
    }

    /// Check if a granted capability string satisfies this pattern.
    fn matches(&self, granted: &str) -> bool {
        // Fast path: global wildcard always matches.
        if granted == "*" {
            return true;
        }

        match self {
            RequiredPattern::AllRye => {
                // `rye.*` — any rye.* cap satisfies. Also satisfied by
                // wildcard patterns like `rye.execute.*`.
                granted.starts_with("rye.")
            }
            RequiredPattern::VerbWildcard { verb } => {
                // `rye.<verb>.*` — verb must match, kind/subject free.
                if let Ok(cap) = Capability::parse(granted) {
                    if cap.verb == *verb {
                        return true;
                    }
                }
                cap_matches(granted, &format!("rye.{}.*", verb))
            }
            RequiredPattern::SubjectWildcard { verb, kind } => {
                // `rye.<verb>.<kind>.*` — verb and kind must match.
                if let Ok(cap) = Capability::parse(granted) {
                    if cap.verb == *verb && cap.kind == *kind {
                        return true;
                    }
                }
                cap_matches(granted, &format!("rye.{}.{}.*", verb, kind))
            }
            RequiredPattern::Full {
                verb,
                kind,
                subject,
            } => {
                if let Ok(cap) = Capability::parse(granted) {
                    if cap.verb == *verb && cap.kind == *kind
                        && cap_matches(subject, &cap.subject)
                    {
                        return true;
                    }
                }
                cap_matches(granted, &format!("rye.{}.{}.{}", verb, kind, subject))
            }
            RequiredPattern::Other(req) => {
                cap_matches(granted, req)
            }
        }
    }
}

// ── Authorization policy (enum-based) ────────────────────────────────

/// Authorization policy for a protected resource.
///
/// Strongly-typed as an enum to prevent ambiguous or contradictory states.
#[derive(Debug, Clone)]
pub enum AuthorizationPolicy {
    /// No authorization required — anyone can invoke.
    Public,

    /// Require all of these clauses (AND). Each clause is an OR of
    /// equivalent capabilities.
    Protected { all_of: Vec<CapabilityClause> },
}

/// A single OR-clause: any of these caps satisfies the clause.
#[derive(Debug, Clone)]
pub struct CapabilityClause {
    pub any_of: Vec<String>,
}

impl AuthorizationPolicy {
    /// Public resource — no authz check.
    pub fn public() -> Self {
        Self::Public
    }

    /// Require a single capability.
    pub fn require(cap: &str) -> Self {
        Self::Protected {
            all_of: vec![CapabilityClause {
                any_of: vec![cap.to_string()],
            }],
        }
    }

    /// Require all caps (each becomes a single-element OR clause).
    pub fn require_all(caps: &[&str]) -> Self {
        Self::Protected {
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
/// Holds a `VerbRegistry` for verb lookup and token routing. The registry
/// is shared via `Arc` so the same instance is used across `AppState` and
/// this authorizer.
pub struct Authorizer {
    verbs: Arc<VerbRegistry>,
}

impl Authorizer {
    pub fn new(verbs: Arc<VerbRegistry>) -> Self {
        Self { verbs }
    }

    /// Access the underlying `VerbRegistry`.
    pub fn verb_registry(&self) -> &VerbRegistry {
        &*self.verbs
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
        match policy {
            AuthorizationPolicy::Public => Ok(()),
            AuthorizationPolicy::Protected { all_of } => {
                for clause in all_of {
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
        }
    }

    /// Check a single required capability against granted scopes.
    ///
    /// Two matching mechanisms:
    /// 1. **Wildcard granted**: `granted = "rye.execute.service.*"` satisfies
    ///    `required = "rye.execute.service.bundle/install"`.
    /// 2. **Wildcard required**: `required = "rye.*"` is satisfied by any
    ///    granted rye cap. `required = "rye.execute.service.*"` is satisfied
    ///    by any granted cap with verb=execute, kind=service.
    fn check_single(&self, scopes: &[String], required: &str) -> bool {
        let pattern = RequiredPattern::parse(required);

        for granted in scopes {
            if pattern.matches(granted) {
                return true;
            }
        }

        false
    }
}

// ── Pattern matching ─────────────────────────────────────────────────

/// Match a granted capability pattern against a required capability string.
///
/// Supports:
/// - Exact match (`rye.execute.service.x` matches itself)
/// - Global wildcard (`*` matches everything)
/// - Prefix wildcard (`rye.execute.service.*` matches `rye.execute.service.bundle/install`)
/// - Path-prefix wildcard (`rye.execute.service.bundle/*` matches `rye.execute.service.bundle/install`)
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

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verb_registry::VerbDef;

    fn test_authorizer() -> Authorizer {
        Authorizer::new(Arc::new(VerbRegistry::from_records(&[
            VerbDef { name: "execute".into(), execute: None },
            VerbDef { name: "fetch".into(), execute: None },
            VerbDef { name: "sign".into(), execute: Some("tool:rye/core/sign".into()) },
        ]).unwrap()))
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

    // ── RequiredPattern parsing ───────────────────────────────────

    #[test]
    fn required_pattern_rye_star() {
        let p = RequiredPattern::parse("rye.*");
        assert!(matches!(p, RequiredPattern::AllRye));
    }

    #[test]
    fn required_pattern_verb_wildcard() {
        let p = RequiredPattern::parse("rye.execute.*");
        match p {
            RequiredPattern::VerbWildcard { verb } => assert_eq!(verb, "execute"),
            _ => panic!("expected VerbWildcard, got {:?}", p),
        }
    }

    #[test]
    fn required_pattern_subject_wildcard() {
        let p = RequiredPattern::parse("rye.execute.service.*");
        match p {
            RequiredPattern::SubjectWildcard { verb, kind } => {
                assert_eq!(verb, "execute");
                assert_eq!(kind, "service");
            }
            _ => panic!("expected SubjectWildcard, got {:?}", p),
        }
    }

    #[test]
    fn required_pattern_full() {
        let p = RequiredPattern::parse("rye.execute.service.bundle/install");
        match p {
            RequiredPattern::Full { verb, kind, subject } => {
                assert_eq!(verb, "execute");
                assert_eq!(kind, "service");
                assert_eq!(subject, "bundle/install");
            }
            _ => panic!("expected Full, got {:?}", p),
        }
    }

    #[test]
    fn required_pattern_other() {
        assert!(matches!(RequiredPattern::parse("*"), RequiredPattern::Other(_)));
        assert!(matches!(
            RequiredPattern::parse("node.maintenance"),
            RequiredPattern::Other(_)
        ));
    }

    // ── RequiredPattern matching ─────────────────────────────────

    #[test]
    fn all_rye_matches_any_rye_cap() {
        let p = RequiredPattern::parse("rye.*");
        assert!(p.matches("rye.execute.service.x"));
        assert!(p.matches("rye.fetch.tool.y/z"));
        assert!(p.matches("rye.sign.directive.a"));
        assert!(!p.matches("node.maintenance"));
    }

    #[test]
    fn verb_wildcard_matches_any_kind() {
        let p = RequiredPattern::parse("rye.execute.*");
        assert!(p.matches("rye.execute.service.x"));
        assert!(p.matches("rye.execute.tool.y"));
        assert!(!p.matches("rye.fetch.service.x"));
    }

    #[test]
    fn subject_wildcard_matches_any_subject() {
        let p = RequiredPattern::parse("rye.execute.service.*");
        assert!(p.matches("rye.execute.service.bundle/install"));
        assert!(p.matches("rye.execute.service.threads/get"));
        assert!(!p.matches("rye.execute.tool.x"));
    }

    #[test]
    fn full_pattern_exact_subject() {
        let p = RequiredPattern::parse("rye.execute.service.bundle/install");
        assert!(p.matches("rye.execute.service.bundle/install"));
        assert!(!p.matches("rye.execute.service.bundle/remove"));
    }

    #[test]
    fn full_pattern_wildcard_subject() {
        let p = RequiredPattern::parse("rye.execute.service.bundle/*");
        assert!(p.matches("rye.execute.service.bundle/install"));
        assert!(p.matches("rye.execute.service.bundle/remove"));
        assert!(!p.matches("rye.execute.service.bundleX"));
    }

    #[test]
    fn global_wildcard_always_matches() {
        let p = RequiredPattern::parse("rye.*");
        assert!(p.matches("*"));
    }

    // ── cap_matches (pattern matching) ────────────────────────────

    #[test]
    fn exact_match() {
        assert!(cap_matches(
            "rye.execute.service.threads/get",
            "rye.execute.service.threads/get"
        ));
    }

    #[test]
    fn no_match_denied() {
        assert!(!cap_matches(
            "rye.execute.service.threads/get",
            "rye.execute.service.threads/list"
        ));
    }

    #[test]
    fn global_wildcard() {
        assert!(cap_matches("*", "rye.execute.service.threads/get"));
        assert!(cap_matches("*", "anything.at.all"));
    }

    #[test]
    fn prefix_wildcard() {
        assert!(cap_matches(
            "rye.execute.service.*",
            "rye.execute.service.bundle/install"
        ));
    }

    #[test]
    fn path_prefix_wildcard() {
        assert!(cap_matches(
            "rye.execute.service.bundle/*",
            "rye.execute.service.bundle/install"
        ));
        assert!(!cap_matches(
            "rye.execute.service.bundle/*",
            "rye.execute.service.bundleX"
        ));
    }

    #[test]
    fn prefix_respects_kind() {
        assert!(!cap_matches(
            "rye.execute.service.*",
            "rye.execute.tool.rye/file-system/read"
        ));
    }

    #[test]
    fn wildcard_does_not_cross_boundaries_without_star() {
        assert!(!cap_matches("rye.execute", "rye.execute.tool.foo"));
    }

    #[test]
    fn single_char_wildcard() {
        assert!(cap_matches(
            "rye.execute.tool.rye/?/fs_read",
            "rye.execute.tool.rye/x/fs_read"
        ));
        assert!(!cap_matches(
            "rye.execute.tool.rye/?/fs_read",
            "rye.execute.tool.rye/xx/fs_read"
        ));
    }

    #[test]
    fn full_wildcard() {
        assert!(cap_matches("rye.*", "rye.execute.tool.anything"));
        assert!(cap_matches("rye.*", "rye.fetch.directive.anything"));
    }

    #[test]
    fn slash_in_subject_matches_wildcard() {
        assert!(cap_matches(
            "rye.execute.service.*",
            "rye.execute.service.bundle/install"
        ));
    }

    // ── Authorizer: no implication — verbs are independent ────────

    #[test]
    fn execute_does_not_imply_fetch() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require("rye.fetch.service.x");
        let scopes = vec!["rye.execute.service.x".to_string()];
        assert!(auth.authorize(&scopes, &policy).is_err());
    }

    #[test]
    fn fetch_does_not_imply_execute() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require("rye.execute.service.x");
        let scopes = vec!["rye.fetch.service.x".to_string()];
        assert!(auth.authorize(&scopes, &policy).is_err());
    }

    #[test]
    fn sign_does_not_imply_fetch() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require("rye.fetch.tool.x");
        let scopes = vec!["rye.sign.tool.x".to_string()];
        assert!(auth.authorize(&scopes, &policy).is_err());
    }

    #[test]
    fn wildcard_grant_satisfies_all() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require("rye.execute.service.bundle/install");
        assert!(auth.authorize(&["*".to_string()], &policy).is_ok());
    }

    #[test]
    fn prefix_wildcard_grant() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require("rye.execute.service.bundle/install");
        assert!(auth
            .authorize(&["rye.execute.service.*".to_string()], &policy)
            .is_ok());
    }

    #[test]
    fn path_prefix_wildcard_grant() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require("rye.execute.service.bundle/install");
        assert!(auth
            .authorize(&["rye.execute.service.bundle/*".to_string()], &policy)
            .is_ok());
    }

    // ── Authorizer: required-side wildcard semantics ──────────────

    #[test]
    fn rye_wildcard_required_satisfied_by_any_rye_grant() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require("rye.*");
        assert!(auth.authorize(&["rye.execute.service.x".to_string()], &policy).is_ok());
        assert!(auth.authorize(&["rye.fetch.tool.y".to_string()], &policy).is_ok());
        assert!(auth.authorize(&["rye.*".to_string()], &policy).is_ok());
        assert!(auth.authorize(&["*".to_string()], &policy).is_ok());
    }

    #[test]
    fn verb_wildcard_required_satisfied_by_concrete_grant() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require("rye.execute.*");
        assert!(auth
            .authorize(&["rye.execute.service.x".to_string()], &policy)
            .is_ok());
        assert!(auth
            .authorize(&["rye.execute.tool.y".to_string()], &policy)
            .is_ok());
        assert!(auth
            .authorize(&["rye.fetch.service.x".to_string()], &policy)
            .is_err());
    }

    #[test]
    fn subject_wildcard_required_satisfied_by_concrete_grant() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require("rye.execute.service.*");
        assert!(auth
            .authorize(&["rye.execute.service.bundle/install".to_string()], &policy)
            .is_ok());
        assert!(auth
            .authorize(&["rye.execute.service.threads/get".to_string()], &policy)
            .is_ok());
    }

    #[test]
    fn different_verbs_are_independent() {
        let auth = test_authorizer();
        // Without implication, each verb is checked independently
        let policy = AuthorizationPolicy::require("rye.execute.service.bundle/install");
        assert!(auth
            .authorize(&["rye.fetch.service.*".to_string()], &policy)
            .is_err());
    }

    // ── Authorizer: policy semantics ──────────────────────────────

    #[test]
    fn public_passthrough() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::public();
        assert!(auth.authorize(&[], &policy).is_ok());
    }

    #[test]
    fn empty_scopes_denied() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require("rye.execute.service.x");
        assert!(auth.authorize(&[], &policy).is_err());
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
        assert!(auth
            .authorize(&["rye.execute.service.a".to_string()], &policy)
            .is_err());
    }

    #[test]
    fn require_all_empty_is_trivially_satisfied() {
        let auth = test_authorizer();
        let policy = AuthorizationPolicy::require_all(&[]);
        assert!(auth.authorize(&[], &policy).is_ok());
    }

    // ── Authorizer: verb_registry getter ──────────────────────────

    #[test]
    fn authorizer_shares_registry_instance() {
        let vr = Arc::new(VerbRegistry::from_records(&[
            VerbDef { name: "execute".into(), execute: None },
            VerbDef { name: "fetch".into(), execute: None },
            VerbDef { name: "sign".into(), execute: Some("tool:rye/core/sign".into()) },
        ]).unwrap());
        let auth = Authorizer::new(vr.clone());
        let state_ptr = &*vr as *const VerbRegistry;
        let auth_ptr = auth.verb_registry() as *const VerbRegistry;
        assert_eq!(state_ptr, auth_ptr, "Authorizer must share the same VerbRegistry instance");
    }

    // ── Subject formatting consistency ────────────────────────────

    #[test]
    fn subject_uses_slash_not_dot() {
        assert_eq!(
            canonical_cap("service", "bundle/install", "execute"),
            "rye.execute.service.bundle/install"
        );
    }

    #[test]
    fn wildcard_matches_slash_subject() {
        assert!(cap_matches(
            "rye.execute.service.*",
            "rye.execute.service.bundle/install"
        ));
    }

    #[test]
    fn slash_subject_matches_across_systems() {
        assert!(cap_matches("rye.execute.*", &canonical_cap("service", "node-sign", "execute")));
        assert!(cap_matches("rye.execute.*", &canonical_cap("directive", "rye/code/review", "execute")));
    }
}
