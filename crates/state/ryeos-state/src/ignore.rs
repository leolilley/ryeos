//! Shared ingest ignore matcher.
//!
//! Used by:
//! - `ryeosd` (ingest_walk, walk_and_diff, push-head validation)
//! - `ryeos-cli` (remote push manifest building)
//!
//! Patterns are loaded from `.ai/node/ingest/ignore.yaml`. If the file is
//! missing the daemon is misconfigured — no silent default fallback.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Ignore rule configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IgnoreConfig {
    /// Glob / directory patterns to exclude.
    pub patterns: Vec<String>,
}

/// Compiled ignore matcher.
///
/// Three pattern forms, chosen per entry in [`IgnoreMatcher::from_config`]:
/// - **component** (`name/`, no internal `/`): matches any path segment named
///   `name`, anywhere in the tree (e.g. `target/`).
/// - **glob** (no trailing `/`): matches the filename component (e.g. `*.pyc`).
/// - **anchored path prefix** (leading `/`, or an internal `/` before the
///   trailing `/`): matches a path prefix rooted at the repo, on segment
///   boundaries (e.g. `/.ai/config/remotes/` matches `.ai/config/remotes` and
///   `.ai/config/remotes/foo`, but not `.ai/config/remotes2`).
#[derive(Debug, Clone)]
pub struct IgnoreMatcher {
    dir_patterns: Vec<String>,
    file_patterns: Vec<glob::Pattern>,
    /// Normalized anchored prefixes (no leading/trailing `/`).
    anchored_patterns: Vec<String>,
}

/// Decide which pattern form an entry is.
fn is_anchored_pattern(pattern: &str) -> bool {
    // A leading `/` is the explicit "anchored to repo root" sigil. An internal
    // `/` (one that survives trimming the trailing slash) also implies a path,
    // not a bare component name.
    pattern.starts_with('/')
        || pattern
            .trim_start_matches('/')
            .trim_end_matches('/')
            .contains('/')
}

/// Validate + normalize an anchored prefix: strip the leading/trailing `/`,
/// then reject anything that isn't a clean repo-relative path.
fn normalize_anchored(pattern: &str) -> Result<String> {
    let core = pattern.trim_start_matches('/').trim_end_matches('/');
    anyhow::ensure!(
        !core.is_empty(),
        "empty anchored ignore pattern: {pattern:?}"
    );
    anyhow::ensure!(
        !core.contains('\\'),
        "anchored ignore pattern must use '/': {pattern:?}"
    );
    for seg in core.split('/') {
        anyhow::ensure!(
            !seg.is_empty(),
            "anchored ignore pattern has an empty segment (duplicate '/'): {pattern:?}"
        );
        anyhow::ensure!(
            seg != "." && seg != "..",
            "anchored ignore pattern must not contain '.' or '..': {pattern:?}"
        );
    }
    Ok(core.to_string())
}

impl IgnoreMatcher {
    /// Load from a YAML file. Returns an error if the file doesn't exist
    /// or contains invalid patterns.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("ingest ignore config not found: {}", path.display()))?;
        let config: IgnoreConfig = serde_yaml::from_str(&content)
            .with_context(|| format!("invalid ingest ignore config: {}", path.display()))?;
        Self::from_config(&config)
    }

    /// Build from a config struct. Validates all patterns up front.
    pub fn from_config(config: &IgnoreConfig) -> Result<Self> {
        let mut dir_patterns = Vec::new();
        let mut file_patterns = Vec::new();
        let mut anchored_patterns = Vec::new();

        for pattern in &config.patterns {
            if is_anchored_pattern(pattern) {
                // Anchored path prefix (e.g. `/.ai/config/remotes/`).
                anchored_patterns.push(normalize_anchored(pattern)?);
            } else if pattern.ends_with('/') {
                // Directory pattern: match against any path component.
                let dir_name = &pattern[..pattern.len() - 1];
                anyhow::ensure!(
                    !dir_name.is_empty(),
                    "empty directory pattern in ignore config"
                );
                dir_patterns.push(dir_name.to_string());
            } else {
                // File/glob pattern: match against filename component.
                let compiled = glob::Pattern::new(pattern)
                    .with_context(|| format!("invalid glob pattern: {}", pattern))?;
                file_patterns.push(compiled);
            }
        }

        Ok(Self {
            dir_patterns,
            file_patterns,
            anchored_patterns,
        })
    }

    /// Returns true if the relative path should be ignored.
    pub fn is_ignored(&self, rel_path: &str) -> bool {
        // Normalize a stray leading slash so anchored matching is repo-relative.
        let rel = rel_path.trim_start_matches('/');

        // Anchored prefix patterns match on segment boundaries.
        for prefix in &self.anchored_patterns {
            if rel == prefix || rel.starts_with(&format!("{prefix}/")) {
                return true;
            }
        }

        // Check directory patterns against every path component.
        for component in rel.split(['/', '\\']) {
            if self.dir_patterns.contains(&component.to_string()) {
                return true;
            }
        }

        // Check file glob patterns against the filename.
        if let Some(filename) = rel.rsplit(['/', '\\']).next() {
            for pattern in &self.file_patterns {
                if pattern.matches(filename) {
                    return true;
                }
            }
        }

        false
    }
}

/// Built-in patterns for when no config file exists (e.g. tests,
/// standalone mode). NOT a silent fallback — production startup
/// uses `load()` which fails if the file is missing.
pub fn builtin_patterns() -> Vec<&'static str> {
    vec![
        ".git/",
        ".hg/",
        ".svn/",
        "node_modules/",
        "target/",
        ".venv/",
        "__pycache__/",
        ".DS_Store",
        "*.pyc",
        ".env",
        // Environment-specific: a project's remotes config points at *this*
        // machine's nodes (e.g. the prod node a dev pushes to). The remote has
        // no use for a copy, so never ship it. Anchored so it only matches the
        // project's own `.ai/config/remotes/`, not any dir named `remotes`.
        "/.ai/config/remotes/",
        // Runtime-owned and rebuildable project data must never be folded
        // into a durable source snapshot. Besides wasting hundreds of MB,
        // ingesting `.ai/state` recursively snapshots the very thread state
        // being created and can prevent admission from ever converging.
        "/.ai/state/",
        "/.ai/cache/",
    ]
}

/// Create an ignore matcher from the built-in patterns.
pub fn matcher_from_builtins() -> IgnoreMatcher {
    let config = IgnoreConfig {
        patterns: builtin_patterns()
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
    };
    IgnoreMatcher::from_config(&config).expect("built-in patterns must be valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher_from_patterns(patterns: &[&str]) -> IgnoreMatcher {
        let config = IgnoreConfig {
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
        };
        IgnoreMatcher::from_config(&config).unwrap()
    }

    #[test]
    fn ignores_git_directory() {
        let m = matcher_from_patterns(&[".git/"]);
        assert!(m.is_ignored(".git/config"));
        assert!(m.is_ignored("src/.git/refs"));
        assert!(m.is_ignored(".git"));
    }

    #[test]
    fn ignores_node_modules() {
        let m = matcher_from_patterns(&["node_modules/"]);
        assert!(m.is_ignored("node_modules/react/index.js"));
        assert!(m.is_ignored("app/node_modules/react/index.js"));
    }

    #[test]
    fn ignores_glob_patterns() {
        let m = matcher_from_patterns(&["*.pyc", ".DS_Store"]);
        assert!(m.is_ignored("foo.pyc"));
        assert!(m.is_ignored("src/bar.pyc"));
        assert!(m.is_ignored(".DS_Store"));
        assert!(!m.is_ignored("foo.py"));
    }

    #[test]
    fn ignores_target_directory() {
        let m = matcher_from_patterns(&["target/"]);
        assert!(m.is_ignored("target/debug/ryeosd"));
        assert!(m.is_ignored("target"));
        assert!(!m.is_ignored("my-target/file.txt"));
    }

    #[test]
    fn builtins_exclude_runtime_state_and_virtualenvs() {
        let matcher = matcher_from_builtins();
        assert!(matcher.is_ignored(".venv/bin/python"));
        assert!(matcher.is_ignored(".ai/state/cas/objects/aa/hash"));
        assert!(matcher.is_ignored(".ai/cache/generated"));
        assert!(!matcher.is_ignored(".ai/graphs/arc/hash_probe.yaml"));
    }

    #[test]
    fn ignores_env_file() {
        let m = matcher_from_patterns(&[".env"]);
        assert!(m.is_ignored(".env"));
        assert!(!m.is_ignored(".env.local"));
        assert!(m.is_ignored("src/.env"));
        assert!(!m.is_ignored(".env/foo"));
    }

    #[test]
    fn does_not_match_partial_dir_name() {
        let m = matcher_from_patterns(&["target/"]);
        assert!(!m.is_ignored("my-target/debug/app"));
    }

    #[test]
    fn rejects_invalid_glob() {
        let config = IgnoreConfig {
            patterns: vec!["[invalid".to_string()],
        };
        assert!(IgnoreMatcher::from_config(&config).is_err());
    }

    #[test]
    fn rejects_empty_dir_pattern() {
        let config = IgnoreConfig {
            patterns: vec!["/".to_string()],
        };
        assert!(IgnoreMatcher::from_config(&config).is_err());
    }

    #[test]
    fn anchored_prefix_matches_on_segment_boundary() {
        let m = matcher_from_patterns(&["/.ai/config/remotes/"]);
        // exact prefix and anything below it
        assert!(m.is_ignored(".ai/config/remotes"));
        assert!(m.is_ignored(".ai/config/remotes/remotes.yaml"));
        assert!(m.is_ignored(".ai/config/remotes/sub/dir/x"));
        // a stray leading slash on the candidate is normalized
        assert!(m.is_ignored("/.ai/config/remotes/remotes.yaml"));
        // sibling with a shared prefix must NOT match
        assert!(!m.is_ignored(".ai/config/remotes2"));
        assert!(!m.is_ignored(".ai/config/remotes2/x"));
        // a same-named dir elsewhere must NOT match (anchored, not component)
        assert!(!m.is_ignored("src/remotes/x"));
        assert!(!m.is_ignored(".ai/other/config/remotes/x"));
    }

    #[test]
    fn anchored_without_leading_slash_is_still_rooted() {
        // An internal '/' makes it a path prefix even without the leading '/'.
        let m = matcher_from_patterns(&[".ai/config/remotes/"]);
        assert!(m.is_ignored(".ai/config/remotes/x"));
        assert!(!m.is_ignored("src/.ai/config/remotes/x"));
    }

    #[test]
    fn anchored_rejects_unsafe_patterns() {
        for bad in ["/.ai/../secrets/", "/.ai//remotes/", "/a/b\\c/", "/"] {
            let config = IgnoreConfig {
                patterns: vec![bad.to_string()],
            };
            assert!(
                IgnoreMatcher::from_config(&config).is_err(),
                "pattern {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn component_pattern_still_matches_anywhere() {
        // Single-component dir patterns keep their match-anywhere behavior.
        let m = matcher_from_patterns(&["node_modules/"]);
        assert!(m.is_ignored("a/b/node_modules/c"));
    }

    #[test]
    fn full_config_roundtrip() {
        let config = IgnoreConfig {
            patterns: vec![
                ".git/".into(),
                "node_modules/".into(),
                "target/".into(),
                "__pycache__/".into(),
                ".DS_Store".into(),
                "*.pyc".into(),
                ".env".into(),
            ],
        };
        let m = IgnoreMatcher::from_config(&config).unwrap();
        assert!(m.is_ignored(".git/HEAD"));
        assert!(m.is_ignored("node_modules/foo"));
        assert!(m.is_ignored("target/debug"));
        assert!(m.is_ignored("__pycache__/foo.pyc"));
        assert!(m.is_ignored(".DS_Store"));
        assert!(m.is_ignored("foo.pyc"));
        assert!(m.is_ignored(".env"));
        assert!(!m.is_ignored("src/main.rs"));
        assert!(!m.is_ignored("README.md"));
    }
}
