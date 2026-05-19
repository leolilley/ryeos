//! Shared argument binder — unified argv → JSON parameters.
//!
//! Used by the resolver (daemon token mode) and the CLI (execute escape hatch).
//! Single source of truth for heuristic flag/value binding.
//!
//! ## Rules
//!
//! - `--key=value` → `{ "key": "value" }`  (hyphens normalised to underscores)
//! - `--key value` → `{ "key": "value" }`  (next token consumed as value)
//! - `--key` (alone or before `--next`) → `{ "key": true }`
//! - `--key -1` → `{ "key": "-1" }` (single-dash values are values, not flags)
//! - `--foo-bar val` → `{ "foo_bar": "val" }` (hyphens in keys → underscores)
//! - Positional tokens → `{ "_args": [...] }`
//! - Repeated keys: last one wins
//! - Empty input → `{}`

/// Normalise a flag key: strip the `--` prefix (already done by caller)
/// and replace hyphens with underscores so `--public-key` becomes `public_key`.
/// This matches the snake_case field names used by handlers with
/// `#[serde(deny_unknown_fields)]`.
fn normalise_key(key: &str) -> String {
    key.replace('-', "_")
}

/// Bind raw argv tokens into a flat JSON parameters object.
///
/// This is schema-free heuristic binding — no POSIX short flags, no `--`
/// terminator, no type coercion. The daemon validates params against the
/// item's schema downstream.
pub fn bind_argv(argv: &[String]) -> serde_json::Value {
    let mut params = serde_json::Map::new();
    let mut positional = Vec::new();
    let mut i = 0;

    while i < argv.len() {
        let token = &argv[i];
        if let Some(key) = token.strip_prefix("--") {
            if key.is_empty() {
                // bare "--" — skip (not currently used, but safe)
                i += 1;
                continue;
            }
            if key.contains('=') {
                // --key=value
                let (k, v) = key.split_once('=').unwrap();
                params.insert(normalise_key(k), serde_json::Value::String(v.to_string()));
            } else if i + 1 < argv.len() && !argv[i + 1].starts_with("--") {
                // --key value (value may start with single dash, e.g. "-1")
                i += 1;
                params.insert(normalise_key(key), serde_json::Value::String(argv[i].clone()));
            } else {
                // --key (bare flag)
                params.insert(normalise_key(key), serde_json::Value::Bool(true));
            }
        } else {
            positional.push(serde_json::Value::String(token.clone()));
        }
        i += 1;
    }

    if !positional.is_empty() {
        params.insert("_args".to_string(), serde_json::Value::Array(positional));
    }

    serde_json::Value::Object(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        let result = bind_argv(&[]);
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn flag_with_value() {
        let result = bind_argv(&["--name".into(), "alice".into()]);
        assert_eq!(result["name"], "alice");
    }

    #[test]
    fn flag_with_equals() {
        let result = bind_argv(&["--seed=119".into()]);
        assert_eq!(result["seed"], "119");
    }

    #[test]
    fn bare_flag_is_true() {
        let result = bind_argv(&["--verbose".into()]);
        assert_eq!(result["verbose"], true);
    }

    #[test]
    fn bare_flag_before_next_flag() {
        let result = bind_argv(&["--verbose".into(), "--count".into(), "5".into()]);
        assert_eq!(result["verbose"], true);
        assert_eq!(result["count"], "5");
    }

    #[test]
    fn negative_number_as_value() {
        // --seed -1: -1 is NOT a flag (only -- starts a flag)
        let result = bind_argv(&["--seed".into(), "-1".into()]);
        assert_eq!(result["seed"], "-1");
    }

    #[test]
    fn positional_args() {
        let result = bind_argv(&["cmd".into(), "--verbose".into(), "arg".into()]);
        // "arg" follows --verbose and doesn't start with "--", so it's consumed as value
        assert_eq!(result["verbose"], "arg");
        let args = result["_args"].as_array().unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "cmd");
    }

    #[test]
    fn positional_only() {
        let result = bind_argv(&["./bundles/standard".into()]);
        let args = result["_args"].as_array().unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "./bundles/standard");
    }

    #[test]
    fn repeated_flag_last_wins() {
        let result = bind_argv(&[
            "--name".into(),
            "alice".into(),
            "--name".into(),
            "bob".into(),
        ]);
        assert_eq!(result["name"], "bob");
    }

    #[test]
    fn mixed_flags_positionals_equals() {
        let result = bind_argv(&[
            "--seed".into(),
            "119".into(),
            "./bundles/standard".into(),
            "--force".into(),
        ]);
        assert_eq!(result["seed"], "119");
        assert_eq!(result["force"], true);
        let args = result["_args"].as_array().unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "./bundles/standard");
    }

    #[test]
    fn flag_consumes_non_dash_value() {
        // --force followed by a non-dash token: consumed as value
        let result = bind_argv(&["--force".into(), "some-arg".into()]);
        assert_eq!(result["force"], "some-arg");
        assert!(result.get("_args").is_none());
    }

    #[test]
    fn bare_double_dash_skipped() {
        let result = bind_argv(&["--".into(), "positional".into()]);
        let args = result["_args"].as_array().unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], "positional");
    }

    // ── Hyphen-to-underscore normalisation ──

    #[test]
    fn hyphenated_key_normalised_to_underscore() {
        let result = bind_argv(&["--public-key".into(), "abc123".into()]);
        assert_eq!(result["public_key"], "abc123");
        assert!(result.get("public-key").is_none());
    }

    #[test]
    fn hyphenated_equals_key_normalised() {
        let result = bind_argv(&["--some-key=value".into()]);
        assert_eq!(result["some_key"], "value");
        assert!(result.get("some-key").is_none());
    }

    #[test]
    fn hyphenated_bare_flag_normalised() {
        let result = bind_argv(&["--dry-run".into()]);
        assert_eq!(result["dry_run"], true);
        assert!(result.get("dry-run").is_none());
    }

    #[test]
    fn mixed_hyphenated_and_plain_keys() {
        let result = bind_argv(&[
            "--public-key".into(),
            "ed25519:abc".into(),
            "--label".into(),
            "test-key".into(),
            "--dry-run".into(),
        ]);
        assert_eq!(result["public_key"], "ed25519:abc");
        assert_eq!(result["label"], "test-key");
        assert_eq!(result["dry_run"], true);
    }
}
