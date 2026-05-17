//! Shared ingest ignore matcher.
//!
//! Used by:
//! - `ryeosd` (ingest_walk, walk_and_diff, push-head validation)
//! - `ryeos-cli` (remote push manifest building)
//!
//! Patterns are loaded from `node/ingest/ignore.yaml`. If the file is
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
#[derive(Debug, Clone)]
pub struct IgnoreMatcher {
    dir_patterns: Vec<String>,
    file_patterns: Vec<glob::Pattern>,
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

        for pattern in &config.patterns {
            if pattern.ends_with('/') {
                // Directory pattern: match against any path component
                let dir_name = &pattern[..pattern.len() - 1];
                anyhow::ensure!(
                    !dir_name.is_empty(),
                    "empty directory pattern in ignore config"
                );
                dir_patterns.push(dir_name.to_string());
            } else {
                // File/glob pattern: match against filename component
                let compiled = glob::Pattern::new(pattern)
                    .with_context(|| format!("invalid glob pattern: {}", pattern))?;
                file_patterns.push(compiled);
            }
        }

        Ok(Self {
            dir_patterns,
            file_patterns,
        })
    }

    /// Returns true if the relative path should be ignored.
    pub fn is_ignored(&self, rel_path: &str) -> bool {
        // Check directory patterns against every path component
        for component in rel_path.split(|c: char| c == '/' || c == '\\') {
            if self.dir_patterns.contains(&component.to_string()) {
                return true;
            }
        }

        // Check file glob patterns against the filename
        if let Some(filename) = rel_path
            .rsplit(|c: char| c == '/' || c == '\\')
            .next()
        {
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
        "__pycache__/",
        ".DS_Store",
        "*.pyc",
        ".env",
    ]
}

/// Create an ignore matcher from the built-in patterns.
pub fn matcher_from_builtins() -> IgnoreMatcher {
    let config = IgnoreConfig {
        patterns: builtin_patterns().into_iter().map(|s| s.to_string()).collect(),
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
