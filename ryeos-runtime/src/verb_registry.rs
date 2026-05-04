//! Verb registry for capability implication expansion.
//!
//! A verb is the second component of a `rye.<verb>.<kind>.<subject>`
//! capability string (e.g. `execute`, `fetch`, `sign`). Verbs can
//! *imply* other verbs: `execute` implies `fetch`, so holding
//! `rye.execute.service.x` also satisfies `rye.fetch.service.x`.
//!
//! The registry is built once at startup with `with_builtins()` and
//! stored in `AppState`. No global singleton, no config file parsing.

use std::collections::{BTreeMap, BTreeSet};

/// Definition of a single verb and what it implies.
#[derive(Debug, Clone)]
pub struct VerbDef {
    pub name: String,
    /// Other verb names that this verb implies.
    /// `execute` implies `fetch` → holding `rye.execute.*` satisfies `rye.fetch.*`.
    pub implies: Vec<String>,
}

/// Registry of known verbs with cached implication expansion.
#[derive(Debug, Clone)]
pub struct VerbRegistry {
    verbs: BTreeMap<String, VerbDef>,
    /// Cached transitive closure: verb → set of all verbs it implies
    /// (including transitive implications). Built once by `rebuild_cache`.
    implications_cache: BTreeMap<String, BTreeSet<String>>,
}

impl VerbRegistry {
    /// Build the registry with the three built-in verbs.
    ///
    /// | Verb     | Implies |
    /// |----------|---------|
    /// | execute  | fetch   |
    /// | fetch    | —       |
    /// | sign     | fetch   |
    ///
    /// New verbs are added to this function and shipped in a release.
    /// Operator-defined verbs via YAML config is a future feature.
    pub fn with_builtins() -> Self {
        let mut reg = Self {
            verbs: BTreeMap::new(),
            implications_cache: BTreeMap::new(),
        };
        reg.register(VerbDef {
            name: "execute".into(),
            implies: vec!["fetch".into()],
        });
        reg.register(VerbDef {
            name: "fetch".into(),
            implies: vec![],
        });
        reg.register(VerbDef {
            name: "sign".into(),
            implies: vec!["fetch".into()],
        });
        reg.rebuild_cache();
        reg
    }

    /// Register a single verb. Rejects duplicate names.
    pub fn register(&mut self, def: VerbDef) {
        if self.verbs.contains_key(&def.name) {
            panic!(
                "VerbRegistry: duplicate verb '{}' — each verb must be registered exactly once",
                def.name
            );
        }
        self.verbs.insert(def.name.clone(), def);
    }

    /// Rebuild the implications cache after all verbs are registered.
    /// Performs DFS cycle detection.
    pub fn rebuild_cache(&mut self) {
        let mut cache: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        for name in self.verbs.keys() {
            let mut visited = BTreeSet::new();
            let mut stack = vec![];
            Self::collect_implied(&self.verbs, name, &mut visited, &mut stack);

            cache.insert(name.clone(), visited);
        }

        self.implications_cache = cache;
    }

    /// DFS collector for transitive implications. Detects cycles.
    fn collect_implied(
        verbs: &BTreeMap<String, VerbDef>,
        name: &str,
        visited: &mut BTreeSet<String>,
        path: &mut Vec<String>,
    ) {
        let def = match verbs.get(name) {
            Some(d) => d,
            None => return, // Unknown verb in implies list — skip
        };

        for implied in &def.implies {
            if path.contains(&implied.clone()) {
                panic!(
                    "VerbRegistry: cycle detected in implication graph: {} → {} (path: [{}])",
                    name,
                    implied,
                    path.join(" → ")
                );
            }
            if visited.insert(implied.clone()) {
                path.push(implied.clone());
                Self::collect_implied(verbs, implied, visited, path);
                path.pop();
            }
        }
    }

    /// Return the set of verbs implied by `verb` (transitively).
    /// Returns an empty set for unknown verbs.
    pub fn implied_verbs(&self, verb: &str) -> &BTreeSet<String> {
        self.implications_cache
            .get(verb)
            .unwrap_or_else(|| {
                static EMPTY: BTreeSet<String> = BTreeSet::new();
                &EMPTY
            })
    }

    /// Return the set of all known verb names.
    pub fn verb_names(&self) -> impl Iterator<Item = &str> {
        self.verbs.keys().map(|s| s.as_str())
    }

    /// Check if a verb is registered.
    pub fn has_verb(&self, name: &str) -> bool {
        self.verbs.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_execute_implies_fetch() {
        let reg = VerbRegistry::with_builtins();
        let implied = reg.implied_verbs("execute");
        assert!(implied.contains("fetch"), "execute must imply fetch");
        assert!(!implied.contains("execute"), "execute must NOT imply itself");
        assert!(!implied.contains("sign"), "execute must NOT imply sign");
    }

    #[test]
    fn builtins_fetch_implies_nothing() {
        let reg = VerbRegistry::with_builtins();
        let implied = reg.implied_verbs("fetch");
        assert!(implied.is_empty(), "fetch must imply nothing");
    }

    #[test]
    fn builtins_sign_implies_fetch() {
        let reg = VerbRegistry::with_builtins();
        let implied = reg.implied_verbs("sign");
        assert!(implied.contains("fetch"), "sign must imply fetch");
        assert!(!implied.contains("sign"), "sign must NOT imply itself");
    }

    #[test]
    fn unknown_verb_returns_empty() {
        let reg = VerbRegistry::with_builtins();
        assert!(reg.implied_verbs("nonexistent").is_empty());
    }

    #[test]
    #[should_panic(expected = "duplicate verb")]
    fn duplicate_registration_panics() {
        let mut reg = VerbRegistry::with_builtins();
        // Can't re-register "execute"
        reg.register(VerbDef {
            name: "execute".into(),
            implies: vec![],
        });
    }

    #[test]
    #[should_panic(expected = "cycle detected")]
    fn cyclic_implication_panics() {
        let mut reg = VerbRegistry {
            verbs: BTreeMap::new(),
            implications_cache: BTreeMap::new(),
        };
        reg.register(VerbDef {
            name: "a".into(),
            implies: vec!["b".into()],
        });
        reg.register(VerbDef {
            name: "b".into(),
            implies: vec!["a".into()],
        });
        reg.rebuild_cache();
    }

    #[test]
    fn transitive_implication() {
        // a → b → c
        let mut reg = VerbRegistry {
            verbs: BTreeMap::new(),
            implications_cache: BTreeMap::new(),
        };
        reg.register(VerbDef {
            name: "a".into(),
            implies: vec!["b".into()],
        });
        reg.register(VerbDef {
            name: "b".into(),
            implies: vec!["c".into()],
        });
        reg.register(VerbDef {
            name: "c".into(),
            implies: vec![],
        });
        reg.rebuild_cache();

        let implied = reg.implied_verbs("a");
        assert!(implied.contains("b"), "a must imply b");
        assert!(implied.contains("c"), "a must imply c (transitively)");
    }

    #[test]
    fn verb_names_returns_all() {
        let reg = VerbRegistry::with_builtins();
        let mut names: Vec<&str> = reg.verb_names().collect();
        names.sort();
        assert_eq!(names, vec!["execute", "fetch", "sign"]);
    }

    #[test]
    fn has_verb_works() {
        let reg = VerbRegistry::with_builtins();
        assert!(reg.has_verb("execute"));
        assert!(reg.has_verb("fetch"));
        assert!(reg.has_verb("sign"));
        assert!(!reg.has_verb("unknown"));
    }
}
