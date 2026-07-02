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

    /// Rough token estimate of the live message window: message content,
    /// replayed `reasoning_content`, and tool-call names/arguments (tool-result
    /// content is counted via the tool message's `content`). Character-count /
    /// 4 — deliberately provider-agnostic; it only needs to track the context
    /// window, not bill.
    pub fn estimate_live_context_tokens(&self, messages: &[ProviderMessage]) -> u64 {
        let char_count: usize = messages
            .iter()
            .map(|m| {
                let mut chars = 0;
                if let Some(ref content) = m.content {
                    chars += content.to_string().len();
                }
                if let Some(ref reasoning) = m.reasoning_content {
                    chars += reasoning.len();
                }
                for tc in m.tool_calls.iter().flatten() {
                    chars += tc.arguments.to_string().len();
                    chars += tc.name.len();
                }
                chars
            })
            .sum();

        (char_count as u64) / 4
    }

    pub fn threshold(&self) -> u64 {
        (self.context_window as f64 * self.threshold_ratio) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_message(role: &str, content: &str) -> ProviderMessage {
        ProviderMessage {
            role: role.to_string(),
            content: Some(json!(content)),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
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
        let mut with_reasoning = make_message("assistant", "");
        with_reasoning.reasoning_content = Some("r".repeat(4000));
        let with = check.estimate_live_context_tokens(std::slice::from_ref(&with_reasoning));
        let without = check.estimate_live_context_tokens(&[make_message("assistant", "")]);
        assert!(with > without);
        assert_eq!(with, 1000); // (2 quotes for empty content + 4000 reasoning) / 4
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
        // content is JSON-stringified, so each 400-char string is 402 chars
        // (surrounding quotes): (402 + 402) / 4 = 201.
        let tokens = check.estimate_live_context_tokens(&messages);
        assert_eq!(tokens, 201);
    }
}
