use ryeos_runtime::envelope::{RuntimeCost, RuntimeCostError};

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

    pub fn report(&mut self, input: u64, output: u64, usd: f64) -> Result<(), RuntimeCostError> {
        self.accumulate(&RuntimeCost {
            input_tokens: input,
            output_tokens: output,
            total_usd: usd,
            basis: None,
        })
    }

    /// Account for already-validated child execution cost as one atomic update.
    /// Hook children use this path so both successful and failed hook dispatches
    /// remain part of the directive's terminal cost.
    pub fn accumulate(&mut self, cost: &RuntimeCost) -> Result<(), RuntimeCostError> {
        let mut accumulated = self.cost();
        accumulated.checked_accumulate(cost)?;
        self.total_input = accumulated.input_tokens;
        self.total_output = accumulated.output_tokens;
        self.total_usd = accumulated.total_usd;
        Ok(())
    }

    pub fn reseed(&mut self, input: u64, output: u64, usd: f64) -> Result<(), RuntimeCostError> {
        let cost = RuntimeCost {
            input_tokens: input,
            output_tokens: output,
            total_usd: usd,
            basis: None,
        };
        cost.validate()?;
        self.total_input = input;
        self.total_output = output;
        self.total_usd = usd;
        Ok(())
    }

    pub fn is_exhausted(&self) -> bool {
        self.max_usd > 0.0 && self.total_usd >= self.max_usd
    }

    pub fn remaining_spend_usd(&self) -> Option<f64> {
        if self.max_usd <= 0.0 {
            None
        } else {
            Some((self.max_usd - self.total_usd).max(0.0))
        }
    }

    pub fn cost(&self) -> RuntimeCost {
        RuntimeCost {
            input_tokens: self.total_input,
            output_tokens: self.total_output,
            total_usd: self.total_usd,
            basis: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tracker(max_usd: f64) -> BudgetTracker {
        BudgetTracker::new(max_usd)
    }

    #[tokio::test]
    async fn reserve_and_release() {
        let mut tracker = make_tracker(1.0);
        tracker.report(0, 0, 0.0).unwrap();
    }

    #[test]
    fn report_accumulates() {
        let mut tracker = make_tracker(10.0);
        tracker.report(100, 50, 0.01).unwrap();
        tracker.report(200, 100, 0.02).unwrap();
        let cost = tracker.cost();
        assert_eq!(cost.input_tokens, 300);
        assert_eq!(cost.output_tokens, 150);
        assert!((cost.total_usd - 0.03).abs() < f64::EPSILON);
    }

    #[test]
    fn is_exhausted() {
        let mut tracker = make_tracker(1.0);
        assert!(!tracker.is_exhausted());
        tracker.report(0, 0, 1.0).unwrap();
        assert!(tracker.is_exhausted());
    }

    #[test]
    fn no_max_means_never_exhausted() {
        let mut tracker = make_tracker(0.0);
        tracker.report(0, 0, 99999.0).unwrap();
        assert!(!tracker.is_exhausted());
    }

    #[test]
    fn report_is_transactional_on_token_overflow() {
        let mut tracker = make_tracker(0.0);
        tracker.report(i64::MAX as u64, 0, 1.0).unwrap();
        assert!(tracker.report(1, 0, 1.0).is_err());
        let cost = tracker.cost();
        assert_eq!(cost.input_tokens, i64::MAX as u64);
        assert_eq!(cost.total_usd, 1.0);
    }

    #[test]
    fn accumulate_includes_child_runtime_cost() {
        let mut tracker = make_tracker(10.0);
        tracker.report(100, 50, 0.01).unwrap();
        tracker
            .accumulate(&RuntimeCost {
                input_tokens: 20,
                output_tokens: 10,
                total_usd: 0.005,
                basis: Some("rollup".to_string()),
            })
            .unwrap();

        let cost = tracker.cost();
        assert_eq!(cost.input_tokens, 120);
        assert_eq!(cost.output_tokens, 60);
        assert!((cost.total_usd - 0.015).abs() < f64::EPSILON);
    }
}
