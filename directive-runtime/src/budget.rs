use crate::launch_envelope::RuntimeCost;

pub struct BudgetTracker {
    total_input: u64,
    total_output: u64,
    total_usd: f64,
    max_usd: f64,
}

impl BudgetTracker {
    pub fn new(_max_usd: f64) -> Self {
        Self {
            total_input: 0,
            total_output: 0,
            total_usd: 0.0,
            max_usd: _max_usd,
        }
    }

    pub fn report(&mut self, input: u64, output: u64, usd: f64) {
        self.total_input += input;
        self.total_output += output;
        self.total_usd += usd;
    }

    pub fn is_exhausted(&self) -> bool {
        self.max_usd > 0.0 && self.total_usd >= self.max_usd
    }

    pub fn cost(&self) -> RuntimeCost {
        RuntimeCost {
            input_tokens: self.total_input,
            output_tokens: self.total_output,
            total_usd: self.total_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_tracker(max_usd: f64) -> BudgetTracker {
        BudgetTracker::new(max_usd)
    }

    #[tokio::test]
    async fn reserve_and_release() {
        let mut tracker = make_tracker(1.0);
        tracker.report(0, 0, 0.0);
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
    }
}
