use crate::resolution::types::{AliasHop, ResolutionError};
use std::collections::HashSet;
use std::collections::HashMap;

/// Resolves @ aliases to their canonical refs recursively.
/// Detects cycles and enforces depth limits.
pub struct AliasResolver {
    aliases: HashMap<String, String>,
    max_depth: usize,
}

impl AliasResolver {
    pub fn new(aliases: HashMap<String, String>, max_depth: usize) -> Self {
        AliasResolver { aliases, max_depth }
    }

    /// Resolve an ID (possibly @alias) recursively.
    /// Returns (canonical_ref, AliasHop if alias was used).
    /// Errors on cycle, max_depth, or unknown alias.
    pub fn resolve(
        &self,
        id: &str,
        kind_name: &str,
    ) -> Result<(String, Option<AliasHop>), ResolutionError> {
        if !id.starts_with('@') {
            // Not an alias, return as-is.
            return Ok((id.to_string(), None));
        }

        let mut expansion = Vec::new();
        let mut seen = HashSet::new();
        let mut current = id.to_string();
        let mut depth = 0;

        loop {
            if !seen.insert(current.clone()) {
                // Cycle detected — include the offending hop in the report
                // so the chain reads like ["@a", "@b", "@a"].
                expansion.push(current.clone());
                return Err(ResolutionError::AliasCycle {
                    expansion,
                });
            }
            expansion.push(current.clone());

            if !current.starts_with('@') {
                // Resolved to a non-alias; we're done.
                let alias_hop = AliasHop {
                    expansion,
                    depth,
                };
                return Ok((current, Some(alias_hop)));
            }

            // Check depth before next step.
            if depth >= self.max_depth {
                return Err(ResolutionError::AliasMaxDepthExceeded {
                    alias: id.to_string(),
                    expansion: expansion.clone(),
                });
            }

            // Look up the next alias.
            match self.aliases.get(&current) {
                Some(next) => {
                    current = next.clone();
                    depth += 1;
                }
                None => {
                    return Err(ResolutionError::UnknownAlias {
                        alias: current,
                        kind: kind_name.to_string(),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_non_alias() {
        let resolver = AliasResolver::new(HashMap::new(), 8);
        let (ref_str, hop) = resolver.resolve("tool:rye/core/foo", "directive").unwrap();
        assert_eq!(ref_str, "tool:rye/core/foo");
        assert!(hop.is_none());
    }

    #[test]
    fn resolve_simple_alias() {
        let mut aliases = HashMap::new();
        aliases.insert("@foo".to_string(), "tool:bar".to_string());
        let resolver = AliasResolver::new(aliases, 8);

        let (ref_str, hop) = resolver.resolve("@foo", "directive").unwrap();
        assert_eq!(ref_str, "tool:bar");
        assert!(hop.is_some());
        let hop = hop.unwrap();
        assert_eq!(hop.depth, 1);
        assert_eq!(hop.expansion, vec!["@foo", "tool:bar"]);
    }

    #[test]
    fn resolve_chained_alias() {
        let mut aliases = HashMap::new();
        aliases.insert("@core".to_string(), "@base".to_string());
        aliases.insert("@base".to_string(), "directive:rye/agent/core/base".to_string());
        let resolver = AliasResolver::new(aliases, 8);

        let (ref_str, hop) = resolver.resolve("@core", "directive").unwrap();
        assert_eq!(ref_str, "directive:rye/agent/core/base");
        assert!(hop.is_some());
        let hop = hop.unwrap();
        assert_eq!(hop.depth, 2);
    }

    #[test]
    fn detect_cycle() {
        let mut aliases = HashMap::new();
        aliases.insert("@a".to_string(), "@b".to_string());
        aliases.insert("@b".to_string(), "@a".to_string());
        let resolver = AliasResolver::new(aliases, 8);

        let err = resolver.resolve("@a", "directive").unwrap_err();
        assert!(matches!(err, ResolutionError::AliasCycle { .. }));
    }

    #[test]
    fn enforce_max_depth() {
        let mut aliases = HashMap::new();
        aliases.insert("@a".to_string(), "@b".to_string());
        aliases.insert("@b".to_string(), "@c".to_string());
        let resolver = AliasResolver::new(aliases, 1); // Max depth 1

        let err = resolver.resolve("@a", "directive").unwrap_err();
        assert!(matches!(err, ResolutionError::AliasMaxDepthExceeded { .. }));
    }

    #[test]
    fn unknown_alias() {
        let resolver = AliasResolver::new(HashMap::new(), 8);
        let err = resolver.resolve("@unknown", "directive").unwrap_err();
        assert!(matches!(err, ResolutionError::UnknownAlias { .. }));
    }
}
