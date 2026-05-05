//! Shared command resolution — resolves alias tokens into a typed command.
//!
//! Used by both the daemon (execute handler) and could be used by the CLI
//! for help/completion. The core operation is:
//!
//!   tokens + AliasRegistry + VerbRegistry → ResolvedCommand
//!
//! This is the single source of truth for token → verb → execute ref resolution.

use serde::Serialize;

/// The result of resolving a token sequence through the alias and verb registries.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedCommand {
    /// The verb name the alias resolved to.
    pub verb: String,
    /// The canonical ref to execute (from the verb's `execute` field).
    pub execute_ref: String,
    /// How many tokens were consumed by the alias match.
    pub consumed: usize,
    /// Remaining tokens after the alias match (the "tail").
    pub tail: Vec<String>,
    /// Parameters parsed from the tail (--key value, --flag, positional).
    pub parameters: serde_json::Value,
    /// Whether the matched alias is deprecated.
    pub deprecated: bool,
    /// If deprecated, the suggested replacement token sequence.
    pub replacement_tokens: Option<Vec<String>>,
    /// If deprecated, the version in which this alias will be removed.
    pub removed_in: Option<String>,
}

/// Errors from command resolution.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no alias matches tokens {tokens:?}")]
    NoMatch { tokens: Vec<String> },
    #[error("alias resolved to verb '{verb}' but verb not found in registry")]
    VerbNotFound { verb: String },
    #[error("verb '{verb}' has no execute ref (abstract verb)")]
    VerbNotExecutable { verb: String },
}

/// Resolve a token sequence into a fully-bound command.
///
/// This is the shared resolution path:
/// 1. `AliasRegistry::match_argv(tokens)` → verb name + consumed count
/// 2. `VerbRegistry::get_verb(verb)` → execute ref
/// 3. Bind tail tokens (after consumed) into parameters
///
/// Returns a `ResolvedCommand` with everything the dispatcher needs.
pub fn resolve_command(
    tokens: &[String],
    alias_registry: &crate::alias_registry::AliasRegistry,
    verb_registry: &crate::verb_registry::VerbRegistry,
) -> Result<ResolvedCommand, ResolveError> {
    // 1. Match alias
    let (verb_name, consumed) = alias_registry
        .match_argv(tokens)
        .ok_or_else(|| ResolveError::NoMatch {
            tokens: tokens.to_vec(),
        })?;

    // 2. Look up verb
    let verb = verb_registry
        .get_verb(&verb_name)
        .ok_or_else(|| ResolveError::VerbNotFound {
            verb: verb_name.clone(),
        })?;

    let execute_ref = verb
        .execute
        .as_ref()
        .ok_or_else(|| ResolveError::VerbNotExecutable {
            verb: verb_name.clone(),
        })?
        .clone();

    // 3. Extract tail and bind parameters
    let tail: Vec<String> = tokens[consumed..].to_vec();
    let parameters = crate::arg_binder::bind_argv(&tail);

    // 4. Check deprecation
    let consumed_tokens: Vec<String> = tokens[..consumed].to_vec();
    let alias_def = alias_registry.get_alias(&consumed_tokens);
    let deprecated = alias_def.map(|d| d.deprecated).unwrap_or(false);
    let replacement_tokens = alias_def.and_then(|d| d.replacement_tokens.clone());
    let removed_in = alias_def.and_then(|d| d.removed_in.clone());

    Ok(ResolvedCommand {
        verb: verb_name,
        execute_ref,
        consumed,
        tail,
        parameters,
        deprecated,
        replacement_tokens,
        removed_in,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registries() -> (
        crate::alias_registry::AliasRegistry,
        crate::verb_registry::VerbRegistry,
    ) {
        let aliases = crate::alias_registry::AliasRegistry::from_records(&[
            crate::alias_registry::AliasDef {
                tokens: vec!["status".into()],
                verb: "status".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            crate::alias_registry::AliasDef {
                tokens: vec!["bundle".into(), "install".into()],
                verb: "bundle-install".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            crate::alias_registry::AliasDef {
                tokens: vec!["sign".into()],
                verb: "sign".into(),
                deprecated: false,
                replacement_tokens: None,
                removed_in: None,
            },
            crate::alias_registry::AliasDef {
                tokens: vec!["sig".into()],
                verb: "sign".into(),
                deprecated: true,
                replacement_tokens: Some(vec!["sign".into()]),
                removed_in: Some("0.4.0".into()),
            },
        ])
        .unwrap();

        let verbs = crate::verb_registry::VerbRegistry::from_records(&[
            crate::verb_registry::VerbDef {
                name: "status".into(),
                execute: Some("service:system/status".into()),
            },
            crate::verb_registry::VerbDef {
                name: "bundle-install".into(),
                execute: Some("service:bundle/install".into()),
            },
            crate::verb_registry::VerbDef {
                name: "sign".into(),
                execute: Some("tool:rye/core/sign".into()),
            },
        ])
        .unwrap();

        (aliases, verbs)
    }

    #[test]
    fn resolve_single_token_no_tail() {
        let (aliases, verbs) = test_registries();
        let cmd = resolve_command(&["status".to_string()], &aliases, &verbs).unwrap();
        assert_eq!(cmd.verb, "status");
        assert_eq!(cmd.execute_ref, "service:system/status");
        assert_eq!(cmd.consumed, 1);
        assert!(cmd.tail.is_empty());
        assert!(cmd.parameters.as_object().unwrap().is_empty());
        assert!(!cmd.deprecated);
    }

    #[test]
    fn resolve_multi_token_alias_with_tail() {
        let (aliases, verbs) = test_registries();
        let tokens = vec![
            "bundle".to_string(),
            "install".to_string(),
            "--name".to_string(),
            "mypackage".to_string(),
            "--force".to_string(),
        ];
        let cmd = resolve_command(&tokens, &aliases, &verbs).unwrap();
        assert_eq!(cmd.verb, "bundle-install");
        assert_eq!(cmd.execute_ref, "service:bundle/install");
        assert_eq!(cmd.consumed, 2);
        assert_eq!(cmd.tail, vec!["--name", "mypackage", "--force"]);
        assert_eq!(cmd.parameters.get("name").unwrap(), "mypackage");
        assert_eq!(cmd.parameters.get("force").unwrap(), true);
        assert!(cmd.parameters.get("_args").is_none());
    }

    #[test]
    fn resolve_no_alias_match() {
        let (aliases, verbs) = test_registries();
        let result = resolve_command(&["nonexistent".to_string()], &aliases, &verbs);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("no alias matches"), "got: {msg}");
    }

    #[test]
    fn resolve_deprecated_alias() {
        let (aliases, verbs) = test_registries();
        let cmd = resolve_command(&["sig".to_string()], &aliases, &verbs).unwrap();
        assert_eq!(cmd.verb, "sign");
        assert!(cmd.deprecated);
        assert_eq!(cmd.replacement_tokens, Some(vec!["sign".to_string()]));
        assert_eq!(cmd.removed_in.as_deref(), Some("0.4.0"));
    }

    #[test]
    fn bind_empty_tail() {
        let result = crate::arg_binder::bind_argv(&[]);
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn bind_key_value_pairs() {
        let tail = vec!["--name".to_string(), "foo".to_string(), "--verbose".to_string()];
        let result = crate::arg_binder::bind_argv(&tail);
        assert_eq!(result.get("name").unwrap(), "foo");
        assert_eq!(result.get("verbose").unwrap(), true);
    }

    #[test]
    fn bind_equals_syntax() {
        let tail = vec!["--seed=119".to_string()];
        let result = crate::arg_binder::bind_argv(&tail);
        assert_eq!(result.get("seed").unwrap(), "119");
    }

    #[test]
    fn bind_positional_args() {
        let tail = vec!["./bundles/standard".to_string()];
        let result = crate::arg_binder::bind_argv(&tail);
        let args = result.get("_args").unwrap().as_array().unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "./bundles/standard");
    }

    #[test]
    fn bind_flag_before_positional() {
        // --force followed by a non-dash token: the non-dash token is
        // consumed as the flag's value (resolver has no schema knowledge).
        let tail = vec!["--force".to_string(), "some-arg".to_string()];
        let result = crate::arg_binder::bind_argv(&tail);
        assert_eq!(result.get("force").unwrap(), "some-arg");
        // No positional _args because "some-arg" was consumed by --force
        assert!(result.get("_args").is_none());
    }

    #[test]
    fn resolve_wrong_tokens() {
        let (aliases, verbs) = test_registries();
        let result = resolve_command(&["xyz".to_string()], &aliases, &verbs);
        assert!(result.is_err());
    }

    #[test]
    fn alias_tokens_do_not_leak_into_params() {
        // This is the core bug the doc identifies:
        // rye bundle install --name x --source_path y
        // should NOT have "bundle"/"install" in parameters
        let (aliases, verbs) = test_registries();
        let tokens = vec![
            "bundle".to_string(),
            "install".to_string(),
            "--name".to_string(),
            "x".to_string(),
            "--source_path".to_string(),
            "y".to_string(),
        ];
        let cmd = resolve_command(&tokens, &aliases, &verbs).unwrap();
        assert_eq!(cmd.consumed, 2);
        assert_eq!(cmd.parameters.get("name").unwrap(), "x");
        assert_eq!(cmd.parameters.get("source_path").unwrap(), "y");
        // NO alias tokens in _args
        assert!(cmd.parameters.get("_args").is_none());
    }
}
