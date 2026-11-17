use crate::directive::ProviderMessage;

pub struct ContinuationCheck {
    context_window: u64,
    threshold_ratio: f64,
}

impl ContinuationCheck {
    pub fn new(context_window: u64, threshold_ratio: f64) -> Self {
        Self {
            context_window,
            threshold_ratio,
        }
    }

    /// Continue when the LIVE context that will be sent on the next provider
    /// call approaches the model's context window.
    ///
    /// The threshold is a per-call quantity (a fraction of the context
    /// window), so it MUST be compared against the current message window —
    /// never cumulative chain spend. Comparing it to lifetime `budget.cost()`
    /// (which is reseeded forward across continuations) latches the check true
    /// after the first crossing: every continuation successor is reseeded over
    /// the line and re-forks after one turn until a hard limit ends the chain.
    /// This entry point takes only the live messages so that footgun cannot be
    /// reintroduced by passing a usage total.
    pub fn should_continue_live_context(&self, messages: &[ProviderMessage]) -> bool {
        self.estimate_live_context_tokens(messages) >= self.threshold()
    }

    /// Rough token estimate of the live message window, by the one estimator
    /// the recovery trim also uses.
    pub fn estimate_live_context_tokens(&self, messages: &[ProviderMessage]) -> u64 {
        crate::estimate::estimate_tokens(messages)
    }

    pub fn threshold(&self) -> u64 {
        (self.context_window as f64 * self.threshold_ratio) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::ProviderMessageExt;

    fn make_message(role: &str, content: &str) -> ProviderMessage {
        match role {
            "system" => ProviderMessage::system(content),
            "assistant" => ProviderMessage::assistant_text(content),
            "tool" => ProviderMessage::tool_result("c0", content),
            _ => ProviderMessage::user(content),
        }
    }

    #[test]
    fn under_threshold_no_continue() {
        let check = ContinuationCheck::new(200_000, 0.9);
        let messages = vec![
            make_message("user", "hello"),
            make_message("assistant", "hi there"),
        ];
        assert!(!check.should_continue_live_context(&messages));
    }

    #[test]
    fn over_threshold_continues() {
        let check = ContinuationCheck::new(100, 0.9);
        let long = (0..200)
            .map(|i| format!("long message {i} with padding content here"))
            .collect::<Vec<_>>();
        let messages: Vec<_> = long.iter().map(|m| make_message("user", m)).collect();
        assert!(check.should_continue_live_context(&messages));
    }

    #[test]
    fn small_live_context_does_not_continue() {
        // The latch regression: the decision reads ONLY the live message
        // window, so a large lifetime budget can never force a fork when the
        // trimmed context is small. There is no usage argument to pass.
        let check = ContinuationCheck::new(200_000, 0.9);
        let messages = vec![make_message("user", "short")];
        assert!(!check.should_continue_live_context(&messages));
    }

    #[test]
    fn reasoning_content_counts_toward_context() {
        let check = ContinuationCheck::new(200_000, 0.9);
        let with_reasoning =
            ProviderMessage::assistant(Some(String::new()), Vec::new(), Some("r".repeat(4000)));
        let with = check.estimate_live_context_tokens(std::slice::from_ref(&with_reasoning));
        let without = check.estimate_live_context_tokens(&[make_message("assistant", "")]);
        assert!(with > without);
        assert_eq!(with, 1000); // 4000 reasoning characters / 4; an empty text adds none
    }

    #[test]
    fn threshold_calculation() {
        let check = ContinuationCheck::new(200_000, 0.9);
        assert_eq!(check.threshold(), 180_000);
    }

    #[test]
    fn estimate_live_context_char_count() {
        let check = ContinuationCheck::new(100_000, 0.9);
        let a = "a".repeat(400);
        let b = "b".repeat(400);
        let messages = vec![make_message("user", &a), make_message("assistant", &b)];
        // Text counts by its length, not its JSON encoding: (400 + 400) / 4.
        let tokens = check.estimate_live_context_tokens(&messages);
        assert_eq!(tokens, 200);
    }
}
