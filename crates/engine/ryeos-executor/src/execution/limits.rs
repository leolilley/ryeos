use std::path::Path;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::launch_envelope::HardLimits;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    #[serde(default)]
    pub defaults: LimitValues,
    #[serde(default)]
    pub caps: LimitCaps,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            defaults: LimitValues::default(),
            caps: LimitCaps::default(),
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LimitCaps {
    pub turns: Option<u32>,
    pub tokens: Option<u64>,
    pub spend_usd: Option<f64>,
    pub spawns: Option<u32>,
    pub depth: Option<u32>,
    pub duration_seconds: Option<u64>,
}

impl LimitCaps {
    fn turns(&self) -> u32 {
        self.turns.unwrap_or(u32::MAX)
    }

    fn tokens(&self) -> u64 {
        self.tokens.unwrap_or(u64::MAX)
    }

    fn spend_usd(&self) -> f64 {
        self.spend_usd.unwrap_or(f64::MAX)
    }

    fn spawns(&self) -> u32 {
        self.spawns.unwrap_or(u32::MAX)
    }

    fn depth(&self) -> u32 {
        self.depth.unwrap_or(u32::MAX)
    }

    fn duration_seconds(&self) -> u64 {
        self.duration_seconds.unwrap_or(u64::MAX)
    }
}

fn default_turns() -> u32 {
    25
}
fn default_tokens() -> u64 {
    200_000
}
fn default_spend() -> f64 {
    2.0
}
fn default_spawns() -> u32 {
    10
}
fn default_depth() -> u32 {
    5
}
fn default_duration() -> u64 {
    300
}

pub fn compute_effective_limits(
    item_requested: Option<&LimitValues>,
    defaults: &LimitValues,
    caps: &LimitCaps,
    parent_limits: Option<&HardLimits>,
    depth: u32,
) -> HardLimits {
    let requested = item_requested.unwrap_or(defaults);

    let turns = clamp(requested.turns, caps.turns());
    let tokens = clamp(requested.tokens, caps.tokens());
    let spend_usd = clamp_f64(requested.spend_usd, caps.spend_usd());
    let spawns = clamp(requested.spawns, caps.spawns());
    let effective_depth = clamp(depth.max(1), caps.depth());
    let duration_seconds = clamp(requested.duration_seconds, caps.duration_seconds());

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
    if value > cap {
        cap
    } else {
        value
    }
}

/// Load limits config from the project's `.ai/config/ryeos-runtime/limits.yaml`.
///
/// Returns `Ok(None)` if the file doesn't exist (limits config is optional).
/// Returns `Err` if the file exists but is malformed — no silent fallback.
pub fn load_limits_config(project_root: &Path) -> anyhow::Result<Option<LimitsConfig>> {
    let config_path = project_root
        .join(".ai")
        .join("config")
        .join("ryeos-runtime")
        .join("limits.yaml");

    if !config_path.is_file() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&config_path)
        .with_context(|| format!("limits config: cannot read {}", config_path.display()))?;

    let config: LimitsConfig = serde_yaml::from_str(&contents).with_context(|| {
        format!(
            "limits config: malformed YAML in {} — fix the file or \
             remove it to use built-in defaults",
            config_path.display()
        )
    })?;

    Ok(Some(config))
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicyConfig {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub schema_version: Option<String>,
    #[serde(default)]
    pub defaults: ExecutionPolicyValues,
    #[serde(default)]
    pub tools: HashMap<String, ExecutionPolicyValues>,
    #[serde(default)]
    pub graphs: HashMap<String, ExecutionPolicyValues>,
    #[serde(default)]
    pub directives: HashMap<String, ExecutionPolicyValues>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicyValues {
    pub timeout: Option<u64>,
    pub max_steps: Option<u32>,
    pub max_concurrency: Option<u32>,
    pub cancellation_mode: Option<String>,
    pub cancellation_grace_secs: Option<u64>,
}

impl ExecutionPolicyValues {
    fn merge_from(&mut self, higher: &ExecutionPolicyValues) {
        if higher.timeout.is_some() {
            self.timeout = higher.timeout;
        }
        if higher.max_steps.is_some() {
            self.max_steps = higher.max_steps;
        }
        if higher.max_concurrency.is_some() {
            self.max_concurrency = higher.max_concurrency;
        }
        if higher.cancellation_mode.is_some() {
            self.cancellation_mode.clone_from(&higher.cancellation_mode);
        }
        if higher.cancellation_grace_secs.is_some() {
            self.cancellation_grace_secs = higher.cancellation_grace_secs;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPolicyLimitOverrides {
    pub duration_seconds: Option<u64>,
    pub duration_seconds_source: Option<String>,
    pub turns: Option<u32>,
    pub turns_source: Option<String>,
}

pub fn apply_execution_policy_overrides(
    defaults: &LimitValues,
    policy: &ExecutionPolicyLimitOverrides,
) -> LimitValues {
    let mut requested = defaults.clone();
    if let Some(duration_seconds) = policy.duration_seconds {
        requested.duration_seconds = duration_seconds;
    }
    if let Some(turns) = policy.turns {
        requested.turns = turns;
    }
    requested
}

/// Load root-item execution policy from `.ai/config/execution/execution.yaml`
/// and translate the fields enforced by the native launch envelope into
/// [`LimitValues`]. Missing config returns `Ok(None)`; malformed config is a
/// loud error so policy typos do not silently fall back to the 300s default.
pub fn load_execution_policy_limits(
    project_root: &Path,
    item_ref: &ryeos_engine::canonical_ref::CanonicalRef,
) -> anyhow::Result<Option<ExecutionPolicyLimitOverrides>> {
    let config_path = project_root
        .join(".ai")
        .join("config")
        .join("execution")
        .join("execution.yaml");

    if !config_path.is_file() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&config_path)
        .with_context(|| format!("execution policy: cannot read {}", config_path.display()))?;

    let config: ExecutionPolicyConfig = serde_yaml::from_str(&contents).with_context(|| {
        format!(
            "execution policy: malformed YAML in {} — fix the file or remove it to use defaults",
            config_path.display()
        )
    })?;

    let mut policy = config.defaults.clone();
    let mut timeout_source = policy
        .timeout
        .map(|_| format!("{}.defaults.timeout", config_path.display()));
    let mut max_steps_source = policy
        .max_steps
        .map(|_| format!("{}.defaults.max_steps", config_path.display()));
    let kind_overrides = match item_ref.kind.as_str() {
        "tool" => config.tools.get(&item_ref.bare_id).map(|v| ("tools", v)),
        "graph" => config.graphs.get(&item_ref.bare_id).map(|v| ("graphs", v)),
        "directive" => config
            .directives
            .get(&item_ref.bare_id)
            .map(|v| ("directives", v)),
        _ => None,
    };
    if let Some((section, overrides)) = kind_overrides {
        if overrides.timeout.is_some() {
            timeout_source = Some(format!(
                "{}.{}.{}/timeout",
                config_path.display(),
                section,
                item_ref.bare_id
            ));
        }
        if overrides.max_steps.is_some() {
            max_steps_source = Some(format!(
                "{}.{}.{}/max_steps",
                config_path.display(),
                section,
                item_ref.bare_id
            ));
        }
        policy.merge_from(overrides);
    }

    if policy.timeout.is_none() && policy.max_steps.is_none() {
        return Ok(None);
    }

    Ok(Some(ExecutionPolicyLimitOverrides {
        duration_seconds: policy.timeout,
        duration_seconds_source: timeout_source,
        turns: policy.max_steps,
        turns_source: max_steps_source,
    }))
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
        let defaults = LimitValues {
            turns: 20,
            ..Default::default()
        };
        let caps = LimitCaps {
            turns: Some(50),
            ..Default::default()
        };
        let hard = compute_effective_limits(None, &defaults, &caps, None, 0);
        assert_eq!(hard.turns, 20);
    }

    #[test]
    fn compute_effective_clamps_against_caps() {
        let requested = LimitValues {
            turns: 100,
            ..Default::default()
        };
        let caps = LimitCaps {
            turns: Some(30),
            ..Default::default()
        };
        let hard =
            compute_effective_limits(Some(&requested), &LimitValues::default(), &caps, None, 0);
        assert_eq!(hard.turns, 30);
    }

    #[test]
    fn missing_caps_do_not_silently_clamp_duration_to_default() {
        let requested = LimitValues {
            duration_seconds: 7200,
            ..Default::default()
        };
        let hard = compute_effective_limits(
            Some(&requested),
            &LimitValues::default(),
            &LimitCaps::default(),
            None,
            0,
        );
        assert_eq!(hard.duration_seconds, 7200);
    }

    #[test]
    fn explicit_duration_cap_still_clamps() {
        let requested = LimitValues {
            duration_seconds: 7200,
            ..Default::default()
        };
        let caps = LimitCaps {
            duration_seconds: Some(600),
            ..Default::default()
        };
        let hard =
            compute_effective_limits(Some(&requested), &LimitValues::default(), &caps, None, 0);
        assert_eq!(hard.duration_seconds, 600);
    }

    #[test]
    fn compute_effective_parent_reduces() {
        let parent = HardLimits {
            turns: 10,
            ..HardLimits::default()
        };
        let hard = compute_effective_limits(
            None,
            &LimitValues::default(),
            &LimitCaps::default(),
            Some(&parent),
            0,
        );
        assert_eq!(hard.turns, 10);
    }

    #[test]
    fn compute_effective_depth_minimum_one() {
        let hard = compute_effective_limits(
            None,
            &LimitValues::default(),
            &LimitCaps::default(),
            None,
            0,
        );
        assert_eq!(hard.depth, 1);
    }

    #[test]
    fn load_limits_config_missing_file_returns_none() {
        let config = load_limits_config(Path::new("/nonexistent")).unwrap();
        assert!(config.is_none(), "missing file should return Ok(None)");
    }

    #[test]
    fn execution_policy_graph_override_maps_to_duration_and_turns() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join(".ai").join("config").join("execution");
        std::fs::create_dir_all(&config_path).unwrap();
        std::fs::write(
            config_path.join("execution.yaml"),
            "defaults:\n  timeout: 300\n  max_steps: 5\ngraphs:\n  snap-track/show_rescrape:\n    timeout: 7200\n    max_steps: 20\n",
        )
        .unwrap();

        let item_ref =
            ryeos_engine::canonical_ref::CanonicalRef::parse("graph:snap-track/show_rescrape")
                .unwrap();
        let overrides = load_execution_policy_limits(temp.path(), &item_ref)
            .unwrap()
            .expect("policy limits");
        assert_eq!(overrides.duration_seconds, Some(7200));
        assert_eq!(overrides.turns, Some(20));
    }

    #[test]
    fn execution_policy_accepts_known_ignored_fields() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join(".ai").join("config").join("execution");
        std::fs::create_dir_all(&config_path).unwrap();
        std::fs::write(
            config_path.join("execution.yaml"),
            "category: execution\nversion: 1\nschema_version: v1\ndefaults:\n  timeout: 300\n  max_steps: 5\n  max_concurrency: 10\n  cancellation_mode: graceful\n  cancellation_grace_secs: 5\ntools:\n  snap-track/scrapers/daily-scheduler:\n    timeout: 14400\n",
        )
        .unwrap();

        let item_ref = ryeos_engine::canonical_ref::CanonicalRef::parse(
            "tool:snap-track/scrapers/daily-scheduler",
        )
        .unwrap();
        let overrides = load_execution_policy_limits(temp.path(), &item_ref)
            .unwrap()
            .expect("policy limits");
        assert_eq!(overrides.duration_seconds, Some(14400));
        assert_eq!(overrides.turns, Some(5));
    }

    #[test]
    fn execution_policy_overrides_preserve_unrelated_limit_defaults() {
        let defaults = LimitValues {
            tokens: 123_456,
            spawns: 42,
            spend_usd: 9.0,
            ..Default::default()
        };
        let requested = apply_execution_policy_overrides(
            &defaults,
            &ExecutionPolicyLimitOverrides {
                duration_seconds: Some(7200),
                duration_seconds_source: None,
                turns: None,
                turns_source: None,
            },
        );

        assert_eq!(requested.duration_seconds, 7200);
        assert_eq!(requested.tokens, 123_456);
        assert_eq!(requested.spawns, 42);
        assert_eq!(requested.spend_usd, 9.0);
    }
}
