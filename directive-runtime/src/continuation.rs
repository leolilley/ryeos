use crate::directive::ProviderMessage;
use crate::launch_envelope::RuntimeCost;

pub struct ContinuationCheck {
    context_window: u64,
    threshold_ratio: f64,
}

impl ContinuationCheck {
    pub fn new(context_window: u64) -> Self {
        Self {
            context_window,
            threshold_ratio: 0.9,
        }
    }

    pub fn should_continue(
        &self,
        messages: &[ProviderMessage],
        usage: Option<&RuntimeCost>,
    ) -> bool {
        let total_tokens = self.estimate_total_tokens(messages, usage);
        let threshold = (self.context_window as f64 * self.threshold_ratio) as u64;
        total_tokens >= threshold
    }

    pub fn estimate_total_tokens(
        &self,
        messages: &[ProviderMessage],
        usage: Option<&RuntimeCost>,
    ) -> u64 {
        if let Some(cost) = usage {
            return cost.input_tokens + cost.output_tokens;
        }

        let char_count: usize = messages
            .iter()
            .map(|m| {
                let mut chars = 0;
                if let Some(ref content) = m.content {
                    chars += content.to_string().len();
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
        }
    }

    #[test]
    fn under_threshold_no_continue() {
        let check = ContinuationCheck::new(200_000);
        let messages = vec![
            make_message("user", "hello"),
            make_message("assistant", "hi there"),
        ];
        assert!(!check.should_continue(&messages, None));
    }

    #[test]
    fn over_threshold_continues() {
        let check = ContinuationCheck::new(100);
        let mut messages = Vec::new();
        let long = (0..200).map(|i| format!("long message {} with padding content here", i)).collect::<Vec<_>>();
        for msg in &long {
            messages.push(make_message("user", msg));
        }
        assert!(check.should_continue(&messages, None));
    }

    #[test]
    fn uses_usage_when_available() {
        let check = ContinuationCheck::new(1000);
        let messages = vec![make_message("user", "short")];
        let usage = RuntimeCost {
            input_tokens: 850,
            output_tokens: 50,
            total_usd: 0.0,
        };
        assert!(check.should_continue(&messages, Some(&usage)));
    }

    #[test]
    fn usage_under_threshold() {
        let check = ContinuationCheck::new(1000);
        let messages = vec![make_message("user", "short")];
        let usage = RuntimeCost {
            input_tokens: 100,
            output_tokens: 50,
            total_usd: 0.0,
        };
        assert!(!check.should_continue(&messages, Some(&usage)));
    }

    #[test]
    fn threshold_calculation() {
        let check = ContinuationCheck::new(200_000);
        assert_eq!(check.threshold(), 180_000);
    }

    #[test]
    fn estimate_total_tokens_fallback_chars() {
        let check = ContinuationCheck::new(100_000);
        let a = "a".repeat(400);
        let b = "b".repeat(400);
        let messages = vec![
            make_message("user", &a),
            make_message("assistant", &b),
        ];
        let tokens = check.estimate_total_tokens(&messages, None);
        assert_eq!(tokens, 201);
    }
}
