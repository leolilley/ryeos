use anyhow::Result;

use crate::launch_envelope::{EnvelopeCallback, RuntimeCost};

pub struct BudgetTracker {
    #[allow(dead_code)]
    socket_path: std::path::PathBuf,
    #[allow(dead_code)]
    token: String,
    reserved: bool,
    total_input: u64,
    total_output: u64,
    total_usd: f64,
    max_usd: f64,
}

impl BudgetTracker {
    pub fn new(callback: &EnvelopeCallback, max_usd: f64) -> Self {
        Self {
            socket_path: callback.socket_path.clone(),
            token: callback.token.clone(),
            reserved: false,
            total_input: 0,
            total_output: 0,
            total_usd: 0.0,
            max_usd,
        }
    }

    pub async fn reserve(&mut self) -> Result<()> {
        self.reserved = true;
        Ok(())
    }

    pub fn report(&mut self, input: u64, output: u64, usd: f64) {
        self.total_input += input;
        self.total_output += output;
        self.total_usd += usd;
    }

    pub fn remaining(&self) -> f64 {
        if self.max_usd <= 0.0 {
            return f64::MAX;
        }
        (self.max_usd - self.total_usd).max(0.0)
    }

    pub fn is_exhausted(&self) -> bool {
        self.max_usd > 0.0 && self.total_usd >= self.max_usd
    }

    pub async fn release(&self) -> Result<()> {
        Ok(())
    }

    pub fn cost(&self) -> RuntimeCost {
        RuntimeCost {
            input_tokens: self.total_input,
            output_tokens: self.total_output,
            total_usd: self.total_usd,
        }
    }

    pub fn socket_path(&self) -> &std::path::Path {
        &self.socket_path
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_tracker(max_usd: f64) -> BudgetTracker {
        let cb = EnvelopeCallback {
            socket_path: PathBuf::from("/tmp/test.sock"),
            token: "test-token".to_string(),
            allowed_primaries: vec!["execute".to_string()],
        };
        BudgetTracker::new(&cb, max_usd)
    }

    #[tokio::test]
    async fn reserve_and_release() {
        let mut tracker = make_tracker(1.0);
        tracker.reserve().await.unwrap();
        tracker.release().await.unwrap();
    }

    #[test]
    fn report_accumulates() {
        let mut tracker = make_tracker(10.0);
        tracker.report(100, 50, 0.01);
        tracker.report(200, 100, 0.02);
        let cost = tracker.cost();
        assert_eq!(cost.input_tokens, 300);
        assert_eq!(cost.output_tokens, 150);
        assert!((cost.total_usd - 0.03).abs() < f64::EPSILON);
    }

    #[test]
    fn remaining_calculates() {
        let mut tracker = make_tracker(1.0);
        tracker.report(0, 0, 0.5);
        assert!((tracker.remaining() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn is_exhausted() {
        let mut tracker = make_tracker(1.0);
        assert!(!tracker.is_exhausted());
        tracker.report(0, 0, 1.0);
        assert!(tracker.is_exhausted());
    }

    #[test]
    fn no_max_means_never_exhausted() {
        let mut tracker = make_tracker(0.0);
        tracker.report(0, 0, 99999.0);
        assert!(!tracker.is_exhausted());
        assert_eq!(tracker.remaining(), f64::MAX);
    }
}
