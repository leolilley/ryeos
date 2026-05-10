//! Alias registry — token-based routing sugar, decoupled from security.
//!
//! Maps `tokens` to verb names. Pure routing convenience: aliases can be
//! deprecated, renamed, or extended freely without touching authorization.
//!
//! The alias registry is validated at startup against the verb registry:
//! every alias must reference a known verb. This catches configuration
//! drift early (fail-closed).

use std::collections::{BTreeMap, BTreeSet};

/// A single alias definition: a token sequence that routes to a verb.
#[derive(Debug, Clone)]
pub struct AliasDef {
    /// Token sequence that triggers this alias (e.g. `["sign"]` or `["bundle", "install"]`).
    pub tokens: Vec<String>,
    /// Verb name this alias routes to (must exist in `VerbRegistry`).
    pub verb: String,
    /// Whether this alias is deprecated. Deprecated aliases still resolve
    /// but callers should warn.
    pub deprecated: bool,
    /// If deprecated, the suggested replacement token sequence.
    pub replacement_tokens: Option<Vec<String>>,
    /// If deprecated, the version in which this alias will be removed.
    pub removed_in: Option<String>,
}

/// Registry of aliases (routing convenience).
///
/// Keyed by `tokens` so each token sequence maps to exactly one verb.
#[derive(Debug, Clone)]
pub struct AliasRegistry {
    /// `tokens` → alias definition.
    alias_index: BTreeMap<Vec<String>, AliasDef>,
    /// Set of token sequences that are deprecated.
    deprecated: BTreeSet<Vec<String>>,
}

/// Errors from alias registry operations.
#[derive(Debug, thiserror::Error)]
pub enum AliasRegistryError {
    #[error("duplicate alias tokens {tokens:?}: first routes to '{first}', second routes to '{second}'")]
    DuplicateAlias {
        tokens: Vec<String>,
        first: String,
        second: String,
    },
    #[error("alias tokens {tokens:?} references unknown verb '{verb}'")]
    UnknownVerb {
        tokens: Vec<String>,
        verb: String,
    },
}

impl AliasRegistry {
    /// Build the alias registry from alias definitions.
    ///
    /// Validates that token sequences are unique. Verb existence
    /// validation is done separately by the daemon startup (which
    /// has access to both registries).
    pub fn from_records(records: &[AliasDef]) -> Result<Self, AliasRegistryError> {
        let mut alias_index: BTreeMap<Vec<String>, AliasDef> = BTreeMap::new();
        let mut deprecated: BTreeSet<Vec<String>> = BTreeSet::new();

        for def in records {
            if let Some(existing) = alias_index.get(&def.tokens) {
                return Err(AliasRegistryError::DuplicateAlias {
                    tokens: def.tokens.clone(),
                    first: existing.verb.clone(),
                    second: def.verb.clone(),
                });
            }

            if def.deprecated {
                deprecated.insert(def.tokens.clone());
            }

            alias_index.insert(def.tokens.clone(), def.clone());
        }

        Ok(Self {
            alias_index,
            deprecated,
        })
    }

    /// Resolve a token sequence to a verb name.
    ///
    /// Returns `None` if no alias matches.
    pub fn resolve_tokens(&self, tokens: &[String]) -> Option<&str> {
        self.alias_index.get(tokens).map(|def| def.verb.as_str())
    }

    /// Resolve a token sequence to an alias definition.
    pub fn get_alias(&self, tokens: &[String]) -> Option<&AliasDef> {
        self.alias_index.get(tokens)
    }

    /// Match an argv against aliases using longest-prefix matching.
    ///
    /// Tries from longest to shortest prefix. Returns `(verb_name, tokens_consumed)`.
    /// E.g. argv `["bundle", "install", "extra"]` matches
    /// `["bundle", "install"]` → `("bundle-install", 2)`.
    pub fn match_argv(&self, argv: &[String]) -> Option<(String, usize)> {
        for len in (1..=argv.len()).rev() {
            let prefix = &argv[0..len];
            if let Some(verb) = self.resolve_tokens(prefix) {
                return Some((verb.to_string(), len));
            }
        }
        None
    }

    /// Check if a token sequence is deprecated.
    pub fn is_deprecated(&self, tokens: &[String]) -> bool {
        self.deprecated.contains(tokens)
    }

    /// Get all aliases for a given verb.
    pub fn aliases_for_verb(&self, verb: &str) -> Vec<&AliasDef> {
        self.alias_index
            .values()
            .filter(|def| def.verb == verb)
            .collect()
    }

    /// Return all alias definitions.
    pub fn all_aliases(&self) -> Vec<&AliasDef> {
        self.alias_index.values().collect()
    }

    /// Return all non-deprecated alias definitions.
    pub fn active_aliases(&self) -> Vec<&AliasDef> {
        self.alias_index
            .values()
            .filter(|def| !def.deprecated)
            .collect()
    }

    /// Validate that every alias references a verb that exists in the given registry.
    ///
    /// Called once at daemon startup after both registries are built.
    /// Returns the first unknown-verb alias as a structured error.
    pub fn validate_all_verbs_known(
        &self,
        verb_registry: &crate::verb_registry::VerbRegistry,
    ) -> Result<(), AliasRegistryError> {
        for def in self.alias_index.values() {
            if !verb_registry.has_verb(&def.verb) {
                return Err(AliasRegistryError::UnknownVerb {
                    tokens: def.tokens.clone(),
                    verb: def.verb.clone(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_alias_defs() -> Vec<AliasDef> {
        vec![
            AliasDef {
                tokens: vec!["sign".into()],
                verb: "sign".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                tokens: vec!["s".into()],
                verb: "sign".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                tokens: vec!["fetch".into()],
                verb: "fetch".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                tokens: vec!["f".into()],
                verb: "fetch".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                tokens: vec!["bundle".into(), "install".into()],
                verb: "bundle-install".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                tokens: vec!["sig".into()],
                verb: "sign".into(),
                deprecated: true,
                replacement_tokens: Some(vec!["sign".into()]),
                removed_in: Some("0.4.0".into()),
            },
        ]
    }

    fn test_registry() -> AliasRegistry {
        AliasRegistry::from_records(&test_alias_defs()).unwrap()
    }

    #[test]
    fn resolve_single_token() {
        let reg = test_registry();
        assert_eq!(reg.resolve_tokens(&["sign".to_string()]), Some("sign"));
        assert_eq!(reg.resolve_tokens(&["fetch".to_string()]), Some("fetch"));
    }

    #[test]
    fn resolve_short_form() {
        let reg = test_registry();
        assert_eq!(reg.resolve_tokens(&["s".to_string()]), Some("sign"));
        assert_eq!(reg.resolve_tokens(&["f".to_string()]), Some("fetch"));
    }

    #[test]
    fn resolve_multi_token() {
        let reg = test_registry();
        assert_eq!(
            reg.resolve_tokens(&["bundle".to_string(), "install".to_string()]),
            Some("bundle-install")
        );
    }

    #[test]
    fn resolve_unknown_returns_none() {
        let reg = test_registry();
        assert_eq!(reg.resolve_tokens(&["nonexistent".to_string()]), None);
    }

    #[test]
    fn match_argv_longest_prefix() {
        let reg = test_registry();
        let (verb, consumed) = reg.match_argv(
            &["bundle".to_string(), "install".to_string(), "extra".to_string()],
        ).unwrap();
        assert_eq!(verb, "bundle-install");
        assert_eq!(consumed, 2);
    }

    #[test]
    fn match_argv_single_token() {
        let reg = test_registry();
        let (verb, consumed) = reg.match_argv(
            &["sign".to_string(), "extra".to_string()],
        ).unwrap();
        assert_eq!(verb, "sign");
        assert_eq!(consumed, 1);
    }

    #[test]
    fn match_argv_no_match_returns_none() {
        let reg = test_registry();
        assert_eq!(reg.match_argv(&["xyz".to_string()]), None);
    }

    #[test]
    fn match_argv_exact_match() {
        let reg = test_registry();
        let (verb, consumed) = reg.match_argv(
            &["bundle".to_string(), "install".to_string()],
        ).unwrap();
        assert_eq!(verb, "bundle-install");
        assert_eq!(consumed, 2);
    }

    #[test]
    fn deprecated_alias_still_resolves() {
        let reg = test_registry();
        assert_eq!(reg.resolve_tokens(&["sig".to_string()]), Some("sign"));
        assert!(reg.is_deprecated(&["sig".to_string()]));
    }

    #[test]
    fn non_deprecated_not_flagged() {
        let reg = test_registry();
        assert!(!reg.is_deprecated(&["sign".to_string()]));
    }

    #[test]
    fn aliases_for_verb() {
        let reg = test_registry();
        let mut names: Vec<&str> = reg.aliases_for_verb("sign")
            .iter()
            .map(|a| a.tokens[0].as_str())
            .collect();
        names.sort();
        assert_eq!(names, vec!["s", "sig", "sign"]);
    }

    #[test]
    fn active_aliases_excludes_deprecated() {
        let reg = test_registry();
        let active = reg.active_aliases();
        let deprecated_count = active.iter().filter(|a| a.deprecated).count();
        assert_eq!(deprecated_count, 0);
        // 6 total, 1 deprecated → 5 active
        assert_eq!(active.len(), 5);
    }

    #[test]
    fn duplicate_tokens_error() {
        let result = AliasRegistry::from_records(&[
            AliasDef {
                tokens: vec!["sign".into()],
                verb: "sign".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            AliasDef {
                tokens: vec!["sign".into()],
                verb: "sign-alt".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
        ]);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("duplicate alias"), "got: {msg}");
    }

    #[test]
    fn empty_records_ok() {
        let reg = AliasRegistry::from_records(&[]).unwrap();
        assert!(reg.all_aliases().is_empty());
    }

    #[test]
    fn get_alias_returns_def() {
        let reg = test_registry();
        let alias = reg.get_alias(&["sig".to_string()]).unwrap();
        assert_eq!(alias.verb, "sign");
        assert!(alias.deprecated);
        assert_eq!(alias.replacement_tokens, Some(vec!["sign".to_string()]));
        assert_eq!(alias.removed_in.as_deref(), Some("0.4.0"));
    }

    #[test]
    fn validate_all_verbs_known_passes() {
        use crate::verb_registry::{VerbDef, VerbRegistry};
        let reg = test_registry();
        let verbs = VerbRegistry::from_records(&[
            VerbDef { name: "sign".into(), execute: None },
            VerbDef { name: "fetch".into(), execute: None },
            VerbDef { name: "bundle-install".into(), execute: None },
        ]).unwrap();
        assert!(reg.validate_all_verbs_known(&verbs).is_ok());
    }

    #[test]
    fn validate_all_verbs_known_rejects_unknown() {
        use crate::verb_registry::VerbRegistry;
        let reg = test_registry();
        let verbs = VerbRegistry::from_records(&[]).unwrap();
        let result = reg.validate_all_verbs_known(&verbs);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = format!("{}", err);
        assert!(msg.contains("unknown verb"), "got: {msg}");
    }
}
