use ryeos_directive_definition::ImageAcceptConfig;

use crate::directive::ProviderMessage;

/// The live context at which a run hands off: the window fraction, lowered to
/// the model's absolute cap when one is declared. The one definition every
/// consumer reads (the continuation check, the successor's carry ceiling), so
/// they cannot disagree.
pub fn hand_off_point(context_window: u64, threshold_ratio: f64, cap: Option<u64>) -> u64 {
    let by_ratio = (context_window as f64 * threshold_ratio) as u64;
    match cap {
        Some(cap) => by_ratio.min(cap),
        None => by_ratio,
    }
}

pub struct ContinuationCheck {
    context_window: u64,
    threshold_ratio: f64,
    /// The model's absolute cap on the hand-off point, when declared.
    handoff_token_cap: Option<u64>,
    /// How the model counts an image, when its entry declares image input.
    images: Option<ImageAcceptConfig>,
}

impl ContinuationCheck {
    pub fn new(context_window: u64, threshold_ratio: f64) -> Self {
        Self {
            context_window,
            threshold_ratio,
            handoff_token_cap: None,
            images: None,
        }
    }

    /// The same check with the model's absolute hand-off cap.
    pub fn with_handoff_token_cap(mut self, cap: Option<u64>) -> Self {
        self.handoff_token_cap = cap;
        self
    }

    /// The same check with the model's image cost declaration, so an image
    /// counts toward the window as the request would carry it.
    pub fn with_images(mut self, images: Option<ImageAcceptConfig>) -> Self {
        self.images = images;
        self
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
        crate::estimate::estimate_tokens(messages, self.images.as_ref())
    }

    /// The effective hand-off point.
    pub fn threshold(&self) -> u64 {
        hand_off_point(
            self.context_window,
            self.threshold_ratio,
            self.handoff_token_cap,
        )
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
    fn absent_cap_is_the_ratio_product() {
        assert_eq!(hand_off_point(200_000, 0.9, None), 180_000);
        let check = ContinuationCheck::new(200_000, 0.9).with_handoff_token_cap(None);
        assert_eq!(check.threshold(), 180_000);
    }

    #[test]
    fn a_cap_below_the_ratio_product_wins() {
        assert_eq!(hand_off_point(1_000_000, 0.9, Some(250_000)), 250_000);
        let check = ContinuationCheck::new(1_000_000, 0.9).with_handoff_token_cap(Some(250_000));
        assert_eq!(check.threshold(), 250_000);
    }

    #[test]
    fn a_cap_above_the_ratio_product_changes_nothing() {
        assert_eq!(hand_off_point(131_072, 0.9, Some(131_072)), 117_964);
    }

    #[test]
    fn the_check_threshold_equals_the_carry_ceiling_definition() {
        for (window, ratio, cap) in [
            (200_000, 0.9, None),
            (1_000_000, 0.9, Some(250_000)),
            (131_072, 0.8, Some(100_000)),
        ] {
            let check = ContinuationCheck::new(window, ratio).with_handoff_token_cap(cap);
            assert_eq!(check.threshold(), hand_off_point(window, ratio, cap));
        }
    }

    #[test]
    fn a_capped_check_continues_at_the_cap_not_the_ratio() {
        let check = ContinuationCheck::new(1_000_000, 0.9).with_handoff_token_cap(Some(100));
        let long = "x".repeat(2000);
        assert!(check.should_continue_live_context(&[make_message("user", &long)]));
        let uncapped = ContinuationCheck::new(1_000_000, 0.9);
        assert!(!uncapped.should_continue_live_context(&[make_message("user", &long)]));
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
