use ryeos_engine::execution_policy::ResolvedExecutionPolicy;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::launch_envelope::HardLimits;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub defaults: LimitValues,
    #[serde(default)]
    pub caps: LimitCaps,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            category: None,
            defaults: LimitValues::default(),
            caps: LimitCaps::default(),
        }
    }
}

impl LimitsConfig {
    fn validate(&self) -> anyhow::Result<()> {
        self.defaults.validate("limits.defaults")?;
        self.caps.validate("limits.caps")?;
        Ok(())
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

impl LimitValues {
    fn validate(&self, source: &str) -> anyhow::Result<()> {
        validate_spend_usd(self.spend_usd, &format!("{source}.spend_usd"))
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
    fn validate(&self, source: &str) -> anyhow::Result<()> {
        if let Some(spend_usd) = self.spend_usd {
            validate_spend_usd(spend_usd, &format!("{source}.spend_usd"))?;
        }
        Ok(())
    }
}

fn validate_spend_usd(value: f64, source: &str) -> anyhow::Result<()> {
    if !value.is_finite() || value < 0.0 {
        return Err(anyhow::anyhow!(
            "{source} must be finite and non-negative, got {value}"
        ));
    }
    Ok(())
}

fn default_turns() -> u32 {
    0
}
fn default_tokens() -> u64 {
    0
}
fn default_spend() -> f64 {
    0.0
}
fn default_spawns() -> u32 {
    0
}
fn default_depth() -> u32 {
    0
}
fn default_duration() -> u64 {
    0
}

pub fn compute_effective_limits(
    item_requested: Option<&LimitValues>,
    defaults: &LimitValues,
    caps: &LimitCaps,
    parent_limits: Option<&HardLimits>,
) -> HardLimits {
    let requested = item_requested.unwrap_or(defaults);

    // Ops caps clamp. A cap is an upper bound; "no cap" (`None`) is the 0
    // sentinel = unbounded. `sentinel_clamp` also makes a present cap win over a
    // 0 (= unlimited) request, so a request can never bypass a configured cap.
    // `depth` here is the depth LIMIT (from requested.depth), NOT the current
    // launch depth — current depth travels separately in EnvelopeRequest.depth.
    let mut hard = HardLimits {
        turns: sentinel_clamp(requested.turns, caps.turns.unwrap_or(0)),
        tokens: sentinel_clamp(requested.tokens, caps.tokens.unwrap_or(0)),
        spend_usd: sentinel_clamp(requested.spend_usd, caps.spend_usd.unwrap_or(0.0)),
        spawns: sentinel_clamp(requested.spawns, caps.spawns.unwrap_or(0)),
        depth: sentinel_clamp(requested.depth, caps.depth.unwrap_or(0)),
        duration_seconds: sentinel_clamp(
            requested.duration_seconds,
            caps.duration_seconds.unwrap_or(0),
        ),
    };

    if let Some(parent) = parent_limits {
        // A child can never exceed the parent that spawned it.
        hard.turns = sentinel_clamp(hard.turns, parent.turns);
        hard.tokens = sentinel_clamp(hard.tokens, parent.tokens);
        hard.spend_usd = sentinel_clamp(hard.spend_usd, parent.spend_usd);
        hard.spawns = sentinel_clamp(hard.spawns, parent.spawns);
        hard.depth = sentinel_clamp(hard.depth, parent.depth);
        hard.duration_seconds = sentinel_clamp(hard.duration_seconds, parent.duration_seconds);
    }

    hard
}

/// Clamp a `value` to an upper `bound`, honouring the "0 means unlimited"
/// sentinel used at enforcement time — on BOTH sides. Used for ops caps (no cap
/// => bound 0) and parent clamps alike. A plain `min` would let a zero bound
/// erase a bounded value into unlimited, or let a zero (unlimited) value ignore
/// a real bound.
#[allow(clippy::float_cmp)] // 0.0 is an exact sentinel here, not an approximation
fn sentinel_clamp<T: Copy + PartialOrd + Default>(value: T, bound: T) -> T {
    let unlimited = T::default();
    if bound == unlimited {
        value // no bound
    } else if value == unlimited {
        bound // value unbounded → take the bound
    } else if value < bound {
        value
    } else {
        bound
    }
}

/// Overlay a partial header `limits:` JSON object onto base defaults, returning
/// the merged [`LimitValues`]. Present header keys win; omitted keys inherit
/// `base` (the project's directive-runtime `limits.yaml` defaults, not built-in
/// constants).
/// Merges at the JSON level so no limit field is named here — the field set
/// lives only in `LimitValues`.
pub fn merge_header_limits(base: &LimitValues, header: &Value) -> anyhow::Result<LimitValues> {
    let overlay = header
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("`limits` must be an object, got {header}"))?;
    let mut merged = serde_json::to_value(base)?;
    let m = merged
        .as_object_mut()
        .expect("LimitValues always serializes to a JSON object");
    for (k, v) in overlay {
        m.insert(k.clone(), v.clone());
    }
    let result: LimitValues = serde_json::from_value(merged)?;
    // A non-finite or negative spend value silently disables enforcement (the
    // harness only acts when `spend_usd > 0.0`), so reject it at the source.
    result.validate("limits")?;
    Ok(result)
}

/// Load limits config from project/bundle config roots.
///
/// Returns `Ok(None)` if the file doesn't exist (limits config is optional).
/// Returns `Err` if the file exists but is malformed — no silent fallback.
pub fn load_limits_config_from_loader(
    loader: &ryeos_runtime::verified_loader::VerifiedLoader,
) -> anyhow::Result<Option<LimitsConfig>> {
    let config = loader
        .load_config_strict::<LimitsConfig>("directive-runtime/limits")
        .map_err(|e| anyhow::anyhow!("limits config: {e}"))?;
    if let Some(config) = &config {
        config.validate()?;
    }
    Ok(config)
}

pub fn apply_execution_policy_overrides(
    defaults: &LimitValues,
    policy: &ResolvedExecutionPolicy,
) -> LimitValues {
    let mut requested = defaults.clone();
    if let Some(timeout) = &policy.timeout {
        requested.duration_seconds = timeout.value;
    }
    if let Some(max_steps) = &policy.max_steps {
        requested.turns = max_steps.value;
    }
    requested
}

pub fn apply_caller_limit_overrides(
    mut requested: LimitValues,
    parameters: &Value,
) -> anyhow::Result<LimitValues> {
    if let Some(raw) = parameters.get("timeout") {
        requested.duration_seconds = raw.as_u64().ok_or_else(|| {
            anyhow::anyhow!("invalid caller timeout: expected unsigned integer seconds, got {raw}")
        })?;
    }
    if let Some(raw) = parameters.get("max_steps") {
        let value = raw.as_u64().ok_or_else(|| {
            anyhow::anyhow!("invalid caller max_steps: expected unsigned integer, got {raw}")
        })?;
        requested.turns = u32::try_from(value)
            .map_err(|_| anyhow::anyhow!("invalid caller max_steps: exceeds u32"))?;
    }
    Ok(requested)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_overlay_preserves_project_defaults_for_omitted() {
        // A partial header overrides only the fields it names; omitted fields
        // inherit the PROJECT defaults (limits.yaml), not built-in constants.
        let base = LimitValues {
            turns: 40,
            tokens: 111_111,
            ..LimitValues::default()
        };
        let header = serde_json::json!({ "tokens": 250_000 });
        let merged = merge_header_limits(&base, &header).unwrap();
        assert_eq!(merged.tokens, 250_000); // present header field wins
        assert_eq!(merged.turns, 40); // omitted inherits project default
    }

    #[test]
    fn header_overlay_empty_is_identity() {
        let base = LimitValues::default();
        let merged = merge_header_limits(&base, &serde_json::json!({})).unwrap();
        assert_eq!(merged.turns, base.turns);
        assert_eq!(merged.tokens, base.tokens);
    }

    #[test]
    fn merge_header_limits_rejects_non_object() {
        let base = LimitValues::default();
        let err = merge_header_limits(&base, &serde_json::json!("nope")).unwrap_err();
        assert!(err.to_string().contains("must be an object"), "got {err}");
    }

    #[test]
    fn merge_header_limits_rejects_negative_spend() {
        let base = LimitValues::default();
        let err = merge_header_limits(&base, &serde_json::json!({ "spend_usd": -1.0 }))
            .unwrap_err();
        assert!(
            err.to_string().contains("must be finite and non-negative"),
            "got {err}"
        );
    }

    #[test]
    fn caps_apply_across_all_dimensions() {
        // Zero (= unlimited) requests on every dimension must all be bounded by
        // their caps, not just turns/duration.
        let requested = LimitValues {
            tokens: 0,
            spend_usd: 0.0,
            spawns: 0,
            ..Default::default()
        };
        let caps = LimitCaps {
            tokens: Some(1000),
            spend_usd: Some(1.5),
            spawns: Some(3),
            ..Default::default()
        };
        let hard = compute_effective_limits(Some(&requested), &LimitValues::default(), &caps, None);
        assert_eq!(hard.tokens, 1000);
        assert_eq!(hard.spend_usd, 1.5);
        assert_eq!(hard.spawns, 3);
    }

    #[test]
    fn sentinel_clamp_zero_is_unlimited_on_both_sides() {
        // Bound 0 (= no limit) must NOT erase a bounded value to 0.
        assert_eq!(sentinel_clamp(100u32, 0u32), 100);
        // Value 0 (= unlimited) takes a bounded cap/parent.
        assert_eq!(sentinel_clamp(0u32, 50u32), 50);
        // Both bounded → the smaller wins.
        assert_eq!(sentinel_clamp(100u32, 50u32), 50);
        assert_eq!(sentinel_clamp(30u32, 50u32), 30);
        // Both unlimited → unlimited.
        assert_eq!(sentinel_clamp(0u64, 0u64), 0);
    }

    #[test]
    fn zero_request_cannot_bypass_a_configured_cap() {
        // A "0 means unlimited" request must not slip past an explicit cap.
        let requested = LimitValues {
            turns: 0,
            ..Default::default()
        };
        let caps = LimitCaps {
            turns: Some(30),
            ..Default::default()
        };
        let hard = compute_effective_limits(Some(&requested), &LimitValues::default(), &caps, None);
        assert_eq!(hard.turns, 30, "cap must win over an unlimited request");
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
        let hard = compute_effective_limits(None, &defaults, &caps, None);
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
        let hard = compute_effective_limits(Some(&requested), &LimitValues::default(), &caps, None);
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
        let hard = compute_effective_limits(Some(&requested), &LimitValues::default(), &caps, None);
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
        );
        assert_eq!(hard.turns, 10);
    }

    #[test]
    fn parent_zero_field_does_not_unlimit_child() {
        // A parent with a zero (unlimited) field must not erase the child's
        // bounded limit — regression for the zero-erase hazard.
        let parent = HardLimits {
            turns: 0,
            ..HardLimits::default()
        };
        let requested = LimitValues {
            turns: 15,
            ..Default::default()
        };
        let hard = compute_effective_limits(
            Some(&requested),
            &LimitValues::default(),
            &LimitCaps::default(),
            Some(&parent),
        );
        assert_eq!(hard.turns, 15, "zero parent field must not unlimit child");
    }

    #[test]
    fn depth_limit_comes_from_requested_not_current_depth() {
        // HardLimits.depth is the depth LIMIT (from requested.depth, default 5),
        // NOT the current launch depth — current depth now travels separately in
        // EnvelopeRequest.depth.
        let hard = compute_effective_limits(
            None,
            &LimitValues::default(),
            &LimitCaps::default(),
            None,
        );
        assert_eq!(hard.depth, default_depth());
    }

    #[test]
    fn load_limits_config_missing_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let loader = ryeos_runtime::verified_loader::VerifiedLoader::new(
            tmp.path().join("project"),
            vec![],
            &tmp.path().join("operator/.ai/config/keys/trusted"),
        );
        let config = load_limits_config_from_loader(&loader).unwrap();
        assert!(config.is_none(), "missing file should return Ok(None)");
    }

    #[test]
    fn load_limits_config_rejects_invalid_spend_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let config_dir = ryeos_engine::roots::RuntimeRoot::new(project.clone())
            .config()
            .join("directive-runtime");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("limits.yaml"),
            "defaults: {}\ncaps:\n  spend_usd: -1.0\n",
        )
        .unwrap();

        let loader = ryeos_runtime::verified_loader::VerifiedLoader::new(
            project,
            vec![],
            &tmp.path().join("operator/.ai/config/keys/trusted"),
        );
        let err = load_limits_config_from_loader(&loader).unwrap_err();
        assert!(
            err.to_string().contains("must be finite and non-negative"),
            "got {err}"
        );
    }

    #[test]
    fn execution_policy_graph_override_maps_to_duration_and_turns() {
        let item_ref =
            ryeos_engine::canonical_ref::CanonicalRef::parse("graph:snap-track/show_rescrape")
                .unwrap();
        let value = serde_json::json!({
            "defaults": { "timeout": 300, "max_steps": 5 },
            "graphs": {
                "snap-track/show_rescrape": { "timeout": 7200, "max_steps": 20 }
            }
        });
        let policy =
            ryeos_engine::execution_policy::ExecutionPolicyResolver::resolve_from_value_for_item(
                &value, &item_ref, None, None,
            )
            .unwrap();
        let limits = apply_execution_policy_overrides(&LimitValues::default(), &policy);
        assert_eq!(limits.duration_seconds, 7200);
        assert_eq!(limits.turns, 20);
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
            &ryeos_engine::execution_policy::ExecutionPolicyResolver::resolve_from_value_for_item(
                &serde_json::json!({"tools": {"x": {"timeout": 7200}}}),
                &ryeos_engine::canonical_ref::CanonicalRef::parse("tool:x").unwrap(),
                None,
                None,
            )
            .unwrap(),
        );

        assert_eq!(requested.duration_seconds, 7200);
        assert_eq!(requested.tokens, 123_456);
        assert_eq!(requested.spawns, 42);
        assert_eq!(requested.spend_usd, 9.0);
    }

    #[test]
    fn caller_limit_overrides_beat_execution_policy_before_caps() {
        let policy =
            ryeos_engine::execution_policy::ExecutionPolicyResolver::resolve_from_value_for_item(
                &serde_json::json!({"defaults": {"timeout": 300, "max_steps": 5}}),
                &ryeos_engine::canonical_ref::CanonicalRef::parse("tool:x").unwrap(),
                None,
                None,
            )
            .unwrap();
        let requested = apply_execution_policy_overrides(&LimitValues::default(), &policy);
        let requested = apply_caller_limit_overrides(
            requested,
            &serde_json::json!({"timeout": 7200, "max_steps": 20}),
        )
        .unwrap();
        let hard = compute_effective_limits(
            Some(&requested),
            &LimitValues::default(),
            &LimitCaps {
                duration_seconds: Some(600),
                ..Default::default()
            },
            None,
        );

        assert_eq!(requested.duration_seconds, 7200);
        assert_eq!(requested.turns, 20);
        assert_eq!(hard.duration_seconds, 600);
    }

    #[test]
    fn caller_max_steps_must_fit_turns_type() {
        let err = apply_caller_limit_overrides(
            LimitValues::default(),
            &serde_json::json!({"max_steps": u64::from(u32::MAX) + 1}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("exceeds u32"), "got {err}");
    }
}
