use ryeos_runtime::envelope::{RuntimeCost, RuntimeCostError, UsdNanos};

pub struct BudgetTracker {
    total_input: u64,
    total_output: u64,
    total_usd: UsdNanos,
    max_usd: UsdNanos,
}

impl BudgetTracker {
    pub fn new(max_usd: UsdNanos) -> Self {
        Self {
            total_input: 0,
            total_output: 0,
            total_usd: UsdNanos::ZERO,
            max_usd,
        }
    }

    pub fn report(
        &mut self,
        input: u64,
        output: u64,
        usd: UsdNanos,
    ) -> Result<(), RuntimeCostError> {
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

    pub fn reseed(
        &mut self,
        input: u64,
        output: u64,
        usd: UsdNanos,
    ) -> Result<(), RuntimeCostError> {
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
        !self.max_usd.is_zero() && self.total_usd >= self.max_usd
    }

    pub fn remaining_spend_usd(&self) -> Option<UsdNanos> {
        if self.max_usd.is_zero() {
            None
        } else {
            Some(
                self.max_usd
                    .checked_sub(self.total_usd)
                    .unwrap_or(UsdNanos::ZERO),
            )
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

    fn usd(canonical: &str) -> UsdNanos {
        UsdNanos::parse_canonical(canonical).unwrap()
    }

    fn make_tracker(max_usd: &str) -> BudgetTracker {
        BudgetTracker::new(usd(max_usd))
    }

    #[tokio::test]
    async fn reserve_and_release() {
        let mut tracker = make_tracker("1");
        tracker.report(0, 0, UsdNanos::ZERO).unwrap();
    }

    #[test]
    fn report_accumulates_exactly() {
        let mut tracker = make_tracker("10");
        tracker.report(100, 50, usd("0.01")).unwrap();
        tracker.report(200, 100, usd("0.02")).unwrap();
        let cost = tracker.cost();
        assert_eq!(cost.input_tokens, 300);
        assert_eq!(cost.output_tokens, 150);
        // Integer-nano accumulation is exact — no epsilon needed.
        assert_eq!(cost.total_usd, usd("0.03"));
    }

    #[test]
    fn is_exhausted() {
        let mut tracker = make_tracker("1");
        assert!(!tracker.is_exhausted());
        tracker.report(0, 0, usd("1")).unwrap();
        assert!(tracker.is_exhausted());
    }

    #[test]
    fn no_max_means_never_exhausted() {
        let mut tracker = make_tracker("0");
        tracker.report(0, 0, usd("99999")).unwrap();
        assert!(!tracker.is_exhausted());
    }

    #[test]
    fn report_is_transactional_on_token_overflow() {
        let mut tracker = make_tracker("0");
        tracker.report(i64::MAX as u64, 0, usd("1")).unwrap();
        assert!(tracker.report(1, 0, usd("1")).is_err());
        let cost = tracker.cost();
        assert_eq!(cost.input_tokens, i64::MAX as u64);
        assert_eq!(cost.total_usd, usd("1"));
    }

    #[test]
    fn report_is_transactional_on_money_overflow() {
        let mut tracker = make_tracker("0");
        tracker.report(1, 0, UsdNanos::MAX).unwrap();
        assert!(tracker.report(1, 0, usd("0.000000001")).is_err());
        let cost = tracker.cost();
        assert_eq!(cost.input_tokens, 1);
        assert_eq!(cost.total_usd, UsdNanos::MAX);
    }

    #[test]
    fn remaining_spend_floors_at_zero() {
        let mut tracker = make_tracker("1");
        assert_eq!(tracker.remaining_spend_usd(), Some(usd("1")));
        tracker.report(0, 0, usd("0.25")).unwrap();
        assert_eq!(tracker.remaining_spend_usd(), Some(usd("0.75")));
        tracker.report(0, 0, usd("2")).unwrap();
        assert_eq!(tracker.remaining_spend_usd(), Some(UsdNanos::ZERO));
    }

    #[test]
    fn accumulate_includes_child_runtime_cost() {
        let mut tracker = make_tracker("10");
        tracker.report(100, 50, usd("0.01")).unwrap();
        tracker
            .accumulate(&RuntimeCost {
                input_tokens: 20,
                output_tokens: 10,
                total_usd: usd("0.005"),
                basis: Some("rollup".to_string()),
            })
            .unwrap();

        let cost = tracker.cost();
        assert_eq!(cost.input_tokens, 120);
        assert_eq!(cost.output_tokens, 60);
        assert_eq!(cost.total_usd, usd("0.015"));
    }
}
