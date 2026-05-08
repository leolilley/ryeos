//! Verb registry — security-canonical capability gates.
//!
//! A verb is the second component of a `ryeos.<verb>.<kind>.<subject>`
//! capability string (e.g. `execute`, `fetch`, `sign`). Each verb
//! optionally has a canonical ref (what it executes).
//!
//! Token routing (CLI dispatch) lives in the separate `AliasRegistry`.
//! This registry is purely about authorization: verb names map to
//! capability strings.
//!
//! The registry is built once at startup from node-config verb YAMLs
//! via `from_records()` and stored in `AppState`. All validation
//! returns `Result` — operator config never panics the daemon.

use std::collections::BTreeMap;

/// Definition of a single verb with optional execution target.
#[derive(Debug, Clone)]
pub struct VerbDef {
    pub name: String,
    /// Canonical ref to execute when this verb is dispatched.
    /// `None` for abstract verbs like `execute` (generic dispatcher).
    pub execute: Option<String>,
}

/// Registry of known verbs (security-canonical).
#[derive(Debug, Clone)]
pub struct VerbRegistry {
    verbs: BTreeMap<String, VerbDef>,
}

/// Errors from verb registry construction.
#[derive(Debug, thiserror::Error)]
pub enum VerbRegistryError {
    #[error("duplicate verb '{name}'")]
    Duplicate { name: String },
}

impl VerbRegistry {
    /// Build the registry from verb definitions.
    ///
    /// Validates that verb names are unique. Returns structured errors —
    /// operator config never panics.
    ///
    /// This is the production constructor — the daemon calls this at
    /// startup with records loaded from `.ai/node/verbs/*.yaml`.
    pub fn from_records(records: &[VerbDef]) -> Result<Self, VerbRegistryError> {
        let mut verbs: BTreeMap<String, VerbDef> = BTreeMap::new();

        for def in records {
            if verbs.contains_key(&def.name) {
                return Err(VerbRegistryError::Duplicate {
                    name: def.name.clone(),
                });
            }
            verbs.insert(def.name.clone(), def.clone());
        }

        Ok(Self { verbs })
    }

    /// Get a verb definition by name.
    pub fn get_verb(&self, name: &str) -> Option<&VerbDef> {
        self.verbs.get(name)
    }

    /// Return the set of all known verb names.
    pub fn verb_names(&self) -> impl Iterator<Item = &str> {
        self.verbs.keys().map(|s| s.as_str())
    }

    /// Check if a verb is registered.
    pub fn has_verb(&self, name: &str) -> bool {
        self.verbs.contains_key(name)
    }

    /// Validate that the verb component of a capability string refers to a
    /// known verb.
    ///
    /// Accepts full caps like `ryeos.sign.directive.*` or bare patterns like
    /// `ryeos.sign.*`. Wildcards (`*`) in the verb position pass — the check
    /// is about typos and drift, not about policy semantics.
    ///
    /// Returns `Ok(())` if the verb is known or wildcarded. Returns a
    /// structured error otherwise.
    pub fn validate_cap_verb(&self, cap: &str) -> Result<(), UnknownVerbInCap> {
        let parts: Vec<&str> = cap.split('.').collect();
        if parts.len() < 2 || parts[0] != "ryeos" {
            // Not a `ryeos.*` cap — not our concern.
            return Ok(());
        }
        let verb = parts[1];
        if verb == "*" {
            return Ok(());
        }
        if !self.has_verb(verb) {
            return Err(UnknownVerbInCap {
                cap: cap.to_string(),
                verb: verb.to_string(),
            });
        }
        Ok(())
    }
}

/// Error from `VerbRegistry::validate_cap_verb`.
#[derive(Debug, thiserror::Error)]
#[error("capability '{cap}' references unknown verb '{verb}'")]
pub struct UnknownVerbInCap {
    pub cap: String,
    pub verb: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Standard verb definitions used across the test suite.
    fn test_verb_defs() -> [VerbDef; 3] {
        [
            VerbDef {
                name: "execute".into(),
                execute: None,
            },
            VerbDef {
                name: "fetch".into(),
                execute: None,
            },
            VerbDef {
                name: "sign".into(),
                execute: Some("tool:ryeos/core/sign".into()),
            },
        ]
    }

    fn test_registry() -> VerbRegistry {
        VerbRegistry::from_records(&test_verb_defs()).unwrap()
    }

    #[test]
    fn from_records_three_verbs() {
        let reg = test_registry();
        assert!(reg.has_verb("execute"));
        assert!(reg.has_verb("fetch"));
        assert!(reg.has_verb("sign"));
    }

    #[test]
    fn get_verb_returns_def() {
        let reg = test_registry();
        let sign = reg.get_verb("sign").unwrap();
        assert_eq!(sign.name, "sign");
        assert_eq!(sign.execute.as_deref(), Some("tool:ryeos/core/sign"));
    }

    #[test]
    fn get_verb_abstract_no_ref() {
        let reg = test_registry();
        let execute = reg.get_verb("execute").unwrap();
        assert_eq!(execute.name, "execute");
        assert!(execute.execute.is_none());
    }

    #[test]
    fn unknown_verb_returns_none() {
        let reg = test_registry();
        assert!(reg.get_verb("nonexistent").is_none());
        assert!(!reg.has_verb("nonexistent"));
    }

    #[test]
    fn duplicate_registration_returns_error() {
        let result = VerbRegistry::from_records(&[
            VerbDef {
                name: "execute".into(),
                execute: None,
            },
            VerbDef {
                name: "execute".into(),
                execute: Some("tool:x".into()),
            },
        ]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, VerbRegistryError::Duplicate { .. }));
        let msg = format!("{err}");
        assert!(msg.contains("duplicate verb 'execute'"), "got: {msg}");
    }

    #[test]
    fn verb_names_returns_all() {
        let reg = test_registry();
        let mut names: Vec<&str> = reg.verb_names().collect();
        names.sort();
        assert_eq!(names, vec!["execute", "fetch", "sign"]);
    }

    #[test]
    fn empty_records_ok() {
        let reg = VerbRegistry::from_records(&[]).unwrap();
        assert!(reg.verb_names().collect::<Vec<_>>().is_empty());
    }

    // ── validate_cap_verb ────────────────────────────────────────

    #[test]
    fn validate_cap_known_verb() {
        let reg = test_registry();
        assert!(reg.validate_cap_verb("ryeos.sign.directive.*").is_ok());
        assert!(reg.validate_cap_verb("ryeos.execute.service.bundle/install").is_ok());
    }

    #[test]
    fn validate_cap_wildcard_verb_passes() {
        let reg = test_registry();
        assert!(reg.validate_cap_verb("ryeos.*").is_ok());
    }

    #[test]
    fn validate_cap_unknown_verb_rejected() {
        let reg = test_registry();
        let result = reg.validate_cap_verb("ryeos.nonexistent.service.*");
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("unknown verb"), "got: {msg}");
        assert!(msg.contains("nonexistent"), "got: {msg}");
    }

    #[test]
    fn validate_cap_non_rye_passes() {
        let reg = test_registry();
        assert!(reg.validate_cap_verb("node.maintenance").is_ok());
        assert!(reg.validate_cap_verb("*").is_ok());
    }
}
