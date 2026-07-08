use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use serde::{Deserialize, Serialize};

use ryeos_runtime::envelope::{EnvelopePolicy, HardLimits};

/// Typed risk level — serde rejects unknown values so typos like
/// `"HIGH"` fail at config load time instead of silently being ignored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RiskLevel {
    #[serde(rename = "low")]
    Low,
    #[serde(rename = "medium")]
    Medium,
    #[serde(rename = "high")]
    High,
}

pub struct Harness {
    limits: HardLimits,
    effective_caps: Vec<String>,
    cancelled: Arc<AtomicBool>,
    /// Live-interrupt request (SIGUSR1). Distinct from `cancelled`: an interrupt
    /// cuts the in-flight cognition but the loop CONTINUES (it seals the partial,
    /// folds the queued operator input, and runs a fresh cognition). The runner
    /// observes-and-resets it via `take_interrupt` after a cut.
    interrupted: Arc<AtomicBool>,
    start: Instant,
    turns_used: u32,
    tokens_used: u64,
    spend_used: f64,
    spawns_used: u32,
    /// Total live interrupts honored this run. Monotonic — NOT refunded — so it
    /// bounds a runaway interrupt loop (each interrupt refunds its turn, so the
    /// turn limit alone can't stop repeated SIGUSR1). See [`Self::record_interrupt`].
    interrupts_used: u32,
    depth: u32,
    risk_policy: Option<RiskPolicy>,
}

/// Backstop cap on live interrupts per run. An interrupt refunds its turn (an
/// interrupted attempt is not a completed turn), so without this an automated
/// SIGUSR1 spam could drive unbounded provider calls. Generous — a human
/// operator steering one execution will never approach it.
const MAX_LIVE_INTERRUPTS: u32 = 256;

impl Harness {
    pub fn new(policy: &EnvelopePolicy, depth: u32, risk_policy: Option<RiskPolicy>) -> Self {
        Self {
            limits: policy.hard_limits.clone(),
            effective_caps: policy.effective_caps.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
            interrupted: Arc::new(AtomicBool::new(false)),
            start: Instant::now(),
            turns_used: 0,
            tokens_used: 0,
            spend_used: 0.0,
            spawns_used: 0,
            interrupts_used: 0,
            depth,
            risk_policy,
        }
    }

    pub fn cancelled_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    /// Shared handle to the live-interrupt flag (set by the SIGUSR1 handler,
    /// observed by the provider stream loop).
    pub fn interrupted_flag(&self) -> Arc<AtomicBool> {
        self.interrupted.clone()
    }

    #[cfg(test)]
    pub fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::Relaxed)
    }

    /// Observe-and-reset the interrupt flag. Returns whether an interrupt was
    /// pending. The runner calls this after a cut so a single SIGUSR1 cuts
    /// exactly one cognition.
    pub fn take_interrupt(&self) -> bool {
        self.interrupted.swap(false, Ordering::Relaxed)
    }

    pub fn check_limits(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err("cancelled".to_string());
        }

        if self.limits.turns > 0 && self.turns_used >= self.limits.turns {
            return Err(format!(
                "turn limit exceeded: {} >= {}",
                self.turns_used, self.limits.turns
            ));
        }

        if self.limits.tokens > 0 && self.tokens_used >= self.limits.tokens {
            return Err(format!(
                "token limit exceeded: {} >= {}",
                self.tokens_used, self.limits.tokens
            ));
        }

        if self.limits.spend_usd > 0.0 && self.spend_used >= self.limits.spend_usd {
            return Err(format!(
                "spend limit exceeded: ${:.4} >= ${:.4}",
                self.spend_used, self.limits.spend_usd
            ));
        }

        if self.limits.spawns > 0 && self.spawns_used >= self.limits.spawns {
            return Err(format!(
                "spawn limit exceeded: {} >= {}",
                self.spawns_used, self.limits.spawns
            ));
        }

        if self.limits.duration_seconds > 0 {
            let elapsed = self.start.elapsed().as_secs();
            if elapsed >= self.limits.duration_seconds {
                return Err(format!(
                    "duration limit exceeded: {}s >= {}s",
                    elapsed, self.limits.duration_seconds
                ));
            }
        }

        if self.limits.depth > 0 && self.depth >= self.limits.depth {
            return Err(format!(
                "depth limit exceeded: {} >= {}",
                self.depth, self.limits.depth
            ));
        }

        Ok(())
    }

    pub fn record_turn(&mut self) {
        self.turns_used += 1;
    }

    /// Un-count a turn whose cognition was cut by a live interrupt before it
    /// completed. DECISION 1: an interrupted attempt is NOT a completed turn, so
    /// the redirect's fresh cognition stays within the turn limit (e.g. a redirect
    /// still works under `limits.turns = 1`).
    pub fn refund_turn(&mut self) {
        self.turns_used = self.turns_used.saturating_sub(1);
    }

    /// Record an honored live interrupt. Returns `true` while under the runaway
    /// backstop, `false` once [`MAX_LIVE_INTERRUPTS`] is exceeded (the runner then
    /// stops honoring further interrupts for this run).
    pub fn record_interrupt(&mut self) -> bool {
        self.interrupts_used += 1;
        self.interrupts_used <= MAX_LIVE_INTERRUPTS
    }

    #[cfg(test)]
    pub fn interrupts_used(&self) -> u32 {
        self.interrupts_used
    }

    pub fn record_tokens(&mut self, input: u64, output: u64) {
        self.tokens_used += input + output;
    }

    pub fn record_spend(&mut self, usd: f64) {
        self.spend_used += usd;
    }

    pub fn record_spawn(&mut self) {
        self.spawns_used += 1;
    }

    pub fn reseed(&mut self, turns: u32, tokens: u64, spend: f64, spawns: u32) {
        self.turns_used = turns;
        self.tokens_used = tokens;
        self.spend_used = spend;
        self.spawns_used = spawns;
    }

    pub fn turns_used(&self) -> u32 {
        self.turns_used
    }

    pub fn tokens_used(&self) -> u64 {
        self.tokens_used
    }

    pub fn spend_used(&self) -> f64 {
        self.spend_used
    }

    pub fn spawns_used(&self) -> u32 {
        self.spawns_used
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn effective_caps(&self) -> &[String] {
        &self.effective_caps
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn assess(&self, cap: &str) -> RiskAssessment {
        self.risk_policy
            .as_ref()
            .map(|p| p.assess(cap))
            .unwrap_or(RiskAssessment {
                level: "medium".to_string(),
                requires_ack: false,
                blocked: false,
            })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HookAction {
    Retry,
    Fail,
    Abort,
    Suspend,
    Escalate,
    Continue,
}

impl HookAction {
    pub fn from_value(action: &Value) -> Result<Self, String> {
        let Some(action_value) = action.get("action") else {
            return Ok(HookAction::Continue);
        };
        let Some(action_type) = action_value.as_str() else {
            return Err("hook control action must be a string".to_string());
        };
        match action_type {
            "retry" => Ok(HookAction::Retry),
            "fail" => Ok(HookAction::Fail),
            "abort" => Ok(HookAction::Abort),
            "suspend" => Ok(HookAction::Suspend),
            "escalate" => Ok(HookAction::Escalate),
            "continue" => Ok(HookAction::Continue),
            other => Err(format!("unknown hook control action `{other}`")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RiskPolicy {
    pub patterns: Vec<RiskPattern>,
}

#[derive(Debug, Clone)]
pub struct RiskPattern {
    pub pattern: String,
    pub level: RiskLevel,
    pub requires_ack: bool,
}

impl RiskPolicy {
    pub fn assess(&self, cap: &str) -> RiskAssessment {
        let mut best: Option<&RiskPattern> = None;
        let mut best_specificity: usize = 0;

        for p in &self.patterns {
            if ryeos_runtime::cap_matches(&p.pattern, cap) {
                // More literal (non-wildcard) chars = more specific.
                // `directive:auth/*` (14 literal chars) beats `*` (0 literal chars).
                let literal_chars = p.pattern.chars().filter(|c| *c != '*').count();
                if literal_chars >= best_specificity {
                    best_specificity = literal_chars;
                    best = Some(p);
                }
            }
        }

        match best {
            Some(p) => RiskAssessment {
                level: match p.level {
                    RiskLevel::Low => "low".to_string(),
                    RiskLevel::Medium => "medium".to_string(),
                    RiskLevel::High => "high".to_string(),
                },
                requires_ack: p.requires_ack,
                blocked: p.level == RiskLevel::High && p.requires_ack,
            },
            None => RiskAssessment {
                level: "medium".to_string(),
                requires_ack: false,
                blocked: false,
            },
        }
    }
}

#[derive(Debug)]
pub struct RiskAssessment {
    pub level: String,
    pub requires_ack: bool,
    pub blocked: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_harness(limits: HardLimits, caps: Vec<String>) -> Harness {
        let policy = EnvelopePolicy {
            effective_caps: caps,
            hard_limits: limits,
        };
        Harness::new(&policy, 0, None)
    }

    #[test]
    fn check_limits_ok() {
        let harness = make_harness(
            HardLimits {
                turns: 10,
                tokens: 1000,
                spend_usd: 1.0,
                spawns: 5,
                depth: 3,
                duration_seconds: 60,
            },
            vec![],
        );
        assert!(harness.check_limits().is_ok());
    }

    #[test]
    fn check_limits_turn_exceeded() {
        let mut harness = make_harness(
            HardLimits {
                turns: 2,
                ..HardLimits::default()
            },
            vec![],
        );
        harness.record_turn();
        harness.record_turn();
        assert!(harness.check_limits().is_err());
    }

    #[test]
    fn check_limits_cancelled() {
        let harness = make_harness(HardLimits::default(), vec![]);
        harness.cancelled_flag().store(true, Ordering::Relaxed);
        assert!(harness.check_limits().is_err());
    }

    #[test]
    fn interrupt_flag_is_independent_of_cancel() {
        // An interrupt must NOT trip check_limits (the loop continues after a cut),
        // unlike cancel.
        let harness = make_harness(HardLimits::default(), vec![]);
        harness.interrupted_flag().store(true, Ordering::Relaxed);
        assert!(harness.is_interrupted());
        assert!(
            harness.check_limits().is_ok(),
            "interrupt must not stop the loop"
        );
    }

    #[test]
    fn take_interrupt_observes_and_resets() {
        let harness = make_harness(HardLimits::default(), vec![]);
        harness.interrupted_flag().store(true, Ordering::Relaxed);
        assert!(harness.take_interrupt(), "first take sees the interrupt");
        assert!(!harness.take_interrupt(), "flag was reset");
        assert!(!harness.is_interrupted());
    }

    #[test]
    fn refund_turn_lets_redirect_proceed_under_turn_limit_1() {
        // DECISION 1: an interrupted attempt is not a completed turn.
        let mut harness = make_harness(
            HardLimits {
                turns: 1,
                ..HardLimits::default()
            },
            vec![],
        );
        harness.record_turn(); // the cognition that got interrupted
        assert!(harness.check_limits().is_err(), "1 used == limit 1");
        harness.refund_turn(); // interrupted → un-count
        assert!(
            harness.check_limits().is_ok(),
            "redirect's fresh cognition fits within the turn limit"
        );
    }

    #[test]
    fn refund_turn_saturates_at_zero() {
        let mut harness = make_harness(HardLimits::default(), vec![]);
        harness.refund_turn();
        assert_eq!(harness.turns_used(), 0);
    }

    #[test]
    fn record_interrupt_bounds_runaway() {
        let mut harness = make_harness(HardLimits::default(), vec![]);
        for _ in 0..MAX_LIVE_INTERRUPTS {
            assert!(harness.record_interrupt(), "under the cap is allowed");
        }
        assert!(!harness.record_interrupt(), "past the cap is refused");
        assert_eq!(harness.interrupts_used(), MAX_LIVE_INTERRUPTS + 1);
    }

    #[test]
    fn check_limits_zero_duration_is_no_limit() {
        let limits = HardLimits {
            duration_seconds: 0,
            ..HardLimits::default()
        };
        let harness = make_harness(limits, vec![]);
        assert!(harness.check_limits().is_ok());
    }

    #[test]
    fn check_permission_granted() {
        let harness = make_harness(
            HardLimits::default(),
            vec!["ryeos.execute.tool.*".to_string()],
        );
        // effective_caps set but check_permission removed; verify caps are stored
        assert_eq!(harness.effective_caps().len(), 1);
    }

    #[test]
    fn record_accumulates() {
        let mut harness = make_harness(HardLimits::default(), vec![]);
        harness.record_turn();
        harness.record_turn();
        harness.record_tokens(100, 50);
        harness.record_spend(0.05);
        assert_eq!(harness.turns_used(), 2);
        assert_eq!(harness.tokens_used(), 150);
        assert!((harness.spend_used() - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn hook_action_from_value() {
        assert_eq!(
            HookAction::from_value(&serde_json::json!({"action": "retry"})).unwrap(),
            HookAction::Retry
        );
        assert_eq!(
            HookAction::from_value(&serde_json::json!({"action": "fail"})).unwrap(),
            HookAction::Fail
        );
        assert_eq!(
            HookAction::from_value(&serde_json::json!({"action": "abort"})).unwrap(),
            HookAction::Abort
        );
        assert_eq!(
            HookAction::from_value(&serde_json::json!("summary text")).unwrap(),
            HookAction::Continue
        );
        assert_eq!(
            HookAction::from_value(&serde_json::json!({"result": "summary text"})).unwrap(),
            HookAction::Continue
        );
        assert!(HookAction::from_value(&serde_json::json!({"action": "unknown"})).is_err());
        assert!(HookAction::from_value(&serde_json::json!({"action": 123})).is_err());
    }

    #[test]
    fn risk_assessment_blocked_high() {
        let policy = RiskPolicy {
            patterns: vec![RiskPattern {
                pattern: "ryeos.execute.tool.*".to_string(),
                level: RiskLevel::High,
                requires_ack: true,
            }],
        };
        let assessment = policy.assess("ryeos.execute.tool.dangerous");
        assert!(assessment.blocked);
    }

    #[test]
    fn risk_assessment_allowed_low() {
        let policy = RiskPolicy {
            patterns: vec![RiskPattern {
                pattern: "ryeos.execute.tool.safe".to_string(),
                level: RiskLevel::Low,
                requires_ack: false,
            }],
        };
        let assessment = policy.assess("ryeos.execute.tool.safe");
        assert!(!assessment.blocked);
    }

    #[test]
    fn risk_assessment_most_specific_wins() {
        // A wildcard `*` matches everything but is less specific than
        // `directive:auth/*` which has more literal characters.
        let policy = RiskPolicy {
            patterns: vec![
                RiskPattern {
                    pattern: "*".to_string(),
                    level: RiskLevel::High,
                    requires_ack: true,
                },
                RiskPattern {
                    pattern: "directive:auth/*".to_string(),
                    level: RiskLevel::Low,
                    requires_ack: false,
                },
            ],
        };
        // The specific pattern should win over the wildcard.
        let assessment = policy.assess("directive:auth/login");
        assert!(
            !assessment.blocked,
            "specific low-risk pattern should override wildcard high"
        );
        assert_eq!(assessment.level, "low");
    }

    #[test]
    fn risk_level_rejects_unknown() {
        let yaml = "HIGH";
        let result: Result<RiskLevel, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "typo-cased 'HIGH' should be rejected");
    }
}
