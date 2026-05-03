use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::launch_envelope::HardLimits;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    #[serde(default)]
    pub defaults: LimitValues,
    #[serde(default)]
    pub caps: LimitValues,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitValues {
    #[serde(default = "default_turns")]
    pub turns: u32,
    #[serde(default = "default_tokens")]
    pub tokens: u64,
    #[serde(default = "default_spend")]
    pub spend_usd: f64,
    #[serde(default = "default_spawns")]
    pub spawns: u32,
    #[serde(default = "default_depth")]
    pub depth: u32,
    #[serde(default = "default_duration")]
    pub duration_seconds: u64,
}

impl Default for LimitValues {
    fn default() -> Self {
        Self {
            turns: default_turns(),
            tokens: default_tokens(),
            spend_usd: default_spend(),
            spawns: default_spawns(),
            depth: default_depth(),
            duration_seconds: default_duration(),
        }
    }
}

fn default_turns() -> u32 { 25 }
fn default_tokens() -> u64 { 200_000 }
fn default_spend() -> f64 { 2.0 }
fn default_spawns() -> u32 { 10 }
fn default_depth() -> u32 { 5 }
fn default_duration() -> u64 { 300 }

pub fn compute_effective_limits(
    item_requested: Option<&LimitValues>,
    defaults: &LimitValues,
    caps: &LimitValues,
    parent_limits: Option<&HardLimits>,
    depth: u32,
) -> HardLimits {
    let requested = item_requested.unwrap_or(defaults);

    let turns = clamp(requested.turns, caps.turns);
    let tokens = clamp(requested.tokens, caps.tokens);
    let spend_usd = clamp_f64(requested.spend_usd, caps.spend_usd);
    let spawns = clamp(requested.spawns, caps.spawns);
    let effective_depth = clamp(depth.max(1), caps.depth);
    let duration_seconds = clamp(requested.duration_seconds, caps.duration_seconds);

    let mut hard = HardLimits {
        turns,
        tokens,
        spend_usd,
        spawns,
        depth: effective_depth,
        duration_seconds,
    };

    if let Some(parent) = parent_limits {
        hard.turns = hard.turns.min(parent.turns);
        hard.tokens = hard.tokens.min(parent.tokens);
        hard.spend_usd = hard.spend_usd.min(parent.spend_usd);
        hard.spawns = hard.spawns.min(parent.spawns);
        hard.depth = hard.depth.min(parent.depth);
        hard.duration_seconds = hard.duration_seconds.min(parent.duration_seconds);
    }

    hard
}

fn clamp<T: Ord>(value: T, cap: T) -> T {
    value.min(cap)
}

fn clamp_f64(value: f64, cap: f64) -> f64 {
    if value > cap { cap } else { value }
}

/// Load limits config from the project's `.ai/config/rye-runtime/limits.yaml`.
///
/// Returns `Ok(None)` if the file doesn't exist (limits config is optional).
/// Returns `Err` if the file exists but is malformed — no silent fallback.
pub fn load_limits_config(project_root: &Path) -> anyhow::Result<Option<LimitsConfig>> {
    let config_path = project_root
        .join(".ai")
        .join("config")
        .join("rye-runtime")
        .join("limits.yaml");

    if !config_path.is_file() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&config_path).with_context(|| {
        format!(
            "limits config: cannot read {}",
            config_path.display()
        )
    })?;

    let config: LimitsConfig = serde_yaml::from_str(&contents).with_context(|| {
        format!(
            "limits config: malformed YAML in {} — fix the file or \
             remove it to use built-in defaults",
            config_path.display()
        )
    })?;

    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_value_within_cap() {
        assert_eq!(clamp(10u32, 25), 10);
    }

    #[test]
    fn clamp_value_exceeds_cap() {
        assert_eq!(clamp(50u32, 25), 25);
    }

    #[test]
    fn clamp_f64_within_cap() {
        assert!((clamp_f64(1.0, 2.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_f64_exceeds_cap() {
        assert!((clamp_f64(5.0, 2.0) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_effective_uses_defaults_when_no_request() {
        let defaults = LimitValues { turns: 20, ..Default::default() };
        let caps = LimitValues { turns: 50, ..Default::default() };
        let hard = compute_effective_limits(None, &defaults, &caps, None, 0);
        assert_eq!(hard.turns, 20);
    }

    #[test]
    fn compute_effective_clamps_against_caps() {
        let requested = LimitValues { turns: 100, ..Default::default() };
        let caps = LimitValues { turns: 30, ..Default::default() };
        let hard = compute_effective_limits(Some(&requested), &LimitValues::default(), &caps, None, 0);
        assert_eq!(hard.turns, 30);
    }

    #[test]
    fn compute_effective_parent_reduces() {
        let parent = HardLimits { turns: 10, ..HardLimits::default() };
        let hard = compute_effective_limits(None, &LimitValues::default(), &LimitValues::default(), Some(&parent), 0);
        assert_eq!(hard.turns, 10);
    }

    #[test]
    fn compute_effective_depth_minimum_one() {
        let hard = compute_effective_limits(None, &LimitValues::default(), &LimitValues::default(), None, 0);
        assert_eq!(hard.depth, 1);
    }

    #[test]
    fn load_limits_config_missing_file_returns_none() {
        let config = load_limits_config(Path::new("/nonexistent")).unwrap();
        assert!(config.is_none(), "missing file should return Ok(None)");
    }
}
