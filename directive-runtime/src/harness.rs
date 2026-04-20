use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::launch_envelope::{EnvelopePolicy, HardLimits};

pub struct Harness {
    limits: HardLimits,
    effective_caps: Vec<String>,
    cancelled: Arc<AtomicBool>,
    start: Instant,
    turns_used: u32,
    tokens_used: u64,
    spend_used: f64,
    spawns_used: u32,
    depth: u32,
}

impl Harness {
    pub fn new(policy: &EnvelopePolicy, depth: u32) -> Self {
        Self {
            limits: policy.hard_limits.clone(),
            effective_caps: policy.effective_caps.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
            start: Instant::now(),
            turns_used: 0,
            tokens_used: 0,
            spend_used: 0.0,
            spawns_used: 0,
            depth,
        }
    }

    pub fn cancelled_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
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

    pub fn check_permission(&self, required: &str) -> bool {
        if self.effective_caps.is_empty() {
            return false;
        }
        self.effective_caps
            .iter()
            .any(|cap| rye_runtime::cap_matches(cap, required))
    }

    pub fn record_turn(&mut self) {
        self.turns_used += 1;
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

    pub fn turns_used(&self) -> u32 {
        self.turns_used
    }

    pub fn tokens_used(&self) -> u64 {
        self.tokens_used
    }

    pub fn spend_used(&self) -> f64 {
        self.spend_used
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
    pub fn from_value(action: &Value) -> Self {
        if let Some(primary) = action.get("primary").and_then(|v| v.as_str()) {
            match primary {
                "retry" => HookAction::Retry,
                "fail" | "abort" => HookAction::Abort,
                "suspend" => HookAction::Suspend,
                "escalate" => HookAction::Escalate,
                _ => HookAction::Continue,
            }
        } else {
            HookAction::Continue
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
    pub level: String,
    pub requires_ack: bool,
}

impl RiskPolicy {
    pub fn assess(&self, cap: &str) -> RiskAssessment {
        let mut best: Option<&RiskPattern> = None;
        let mut best_specificity = 0;

        for p in &self.patterns {
            if rye_runtime::cap_matches(&p.pattern, cap) {
                let specificity = p.pattern.matches('*').count();
                if specificity >= best_specificity {
                    best_specificity = specificity;
                    best = Some(p);
                }
            }
        }

        match best {
            Some(p) => RiskAssessment {
                level: p.level.clone(),
                requires_ack: p.requires_ack,
                blocked: p.level == "high" && p.requires_ack,
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
        Harness::new(&policy, 0)
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
            vec!["rye.execute.tool.*".to_string()],
        );
        assert!(harness.check_permission("rye.execute.tool.read_file"));
    }

    #[test]
    fn check_permission_denied_empty_caps() {
        let harness = make_harness(HardLimits::default(), vec![]);
        assert!(!harness.check_permission("rye.execute.tool.read_file"));
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
            HookAction::from_value(&serde_json::json!({"primary": "retry"})),
            HookAction::Retry
        );
        assert_eq!(
            HookAction::from_value(&serde_json::json!({"primary": "fail"})),
            HookAction::Abort
        );
        assert_eq!(
            HookAction::from_value(&serde_json::json!({"primary": "unknown"})),
            HookAction::Continue
        );
    }

    #[test]
    fn risk_assessment_blocked_high() {
        let policy = RiskPolicy {
            patterns: vec![RiskPattern {
                pattern: "rye.execute.tool.*".to_string(),
                level: "high".to_string(),
                requires_ack: true,
            }],
        };
        let assessment = policy.assess("rye.execute.tool.dangerous");
        assert!(assessment.blocked);
    }

    #[test]
    fn risk_assessment_allowed_low() {
        let policy = RiskPolicy {
            patterns: vec![RiskPattern {
                pattern: "rye.execute.tool.safe".to_string(),
                level: "low".to_string(),
                requires_ack: false,
            }],
        };
        let assessment = policy.assess("rye.execute.tool.safe");
        assert!(!assessment.blocked);
    }
}
