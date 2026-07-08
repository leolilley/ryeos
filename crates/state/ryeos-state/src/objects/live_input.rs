//! Live-input types — an operator input delivered to a *running* thread.
//!
//! A running directive thread folds operator input as a new `cognition_in`
//! event. Two intents differ only in *when and how* the input lands; both
//! produce the same honest, foldable braid event:
//!
//! - [`LiveInputIntent::Steer`] — cooperative. The input is folded at the
//!   next turn boundary if the loop runs another cognition; a thread that has
//!   genuinely finalized is past steering (the caller falls back to a
//!   chained-resume).
//! - [`LiveInputIntent::Interrupt`] — forceful. The in-flight cognition is cut,
//!   its partial sealed as `cognition_out{interrupted:true}`, and the input
//!   then folds into a fresh cognition.
//!
//! These describe the *delivery mechanism*; the durable braid event itself
//! carries only the input `content` (the intent is not persisted).

use serde::{Deserialize, Serialize};

/// How an operator input should be delivered to a running thread.
///
/// Serializes as snake_case (`"steer"` / `"interrupt"`). Deserialization
/// defaults to [`LiveInputIntent::Steer`] so an older client that omits the
/// field gets the cooperative path, never an unexpected interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveInputIntent {
    /// Fold at the next turn boundary if the loop runs another cognition.
    #[default]
    Steer,
    /// Cut the in-flight cognition now, seal the partial, then fold.
    Interrupt,
}

impl LiveInputIntent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Interrupt => "interrupt",
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s {
            "steer" => Some(Self::Steer),
            "interrupt" => Some(Self::Interrupt),
            _ => None,
        }
    }

    /// Whether this intent forcibly cuts the in-flight cognition.
    pub fn is_interrupt(&self) -> bool {
        matches!(self, Self::Interrupt)
    }
}

/// An operator input queued for delivery to a running thread.
///
/// `content` becomes the `cognition_in` payload verbatim (bound whole, never
/// tokenized). `intent` selects the delivery mechanism and is *not* part of the
/// persisted braid event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveInput {
    pub content: String,
    #[serde(default)]
    pub intent: LiveInputIntent,
}

impl LiveInput {
    pub fn new(content: impl Into<String>, intent: LiveInputIntent) -> Self {
        Self {
            content: content.into(),
            intent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_defaults_to_steer() {
        assert_eq!(LiveInputIntent::default(), LiveInputIntent::Steer);
    }

    #[test]
    fn intent_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&LiveInputIntent::Steer).unwrap(),
            "\"steer\""
        );
        assert_eq!(
            serde_json::to_string(&LiveInputIntent::Interrupt).unwrap(),
            "\"interrupt\""
        );
    }

    #[test]
    fn input_missing_intent_defaults_to_steer() {
        let s: LiveInput = serde_json::from_str(r#"{"content":"hello"}"#).unwrap();
        assert_eq!(s.intent, LiveInputIntent::Steer);
        assert_eq!(s.content, "hello");
    }

    #[test]
    fn input_round_trips_interrupt() {
        let s = LiveInput::new("redirect now", LiveInputIntent::Interrupt);
        let json = serde_json::to_string(&s).unwrap();
        let back: LiveInput = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        assert!(back.intent.is_interrupt());
    }

    #[test]
    fn str_round_trip() {
        for intent in [LiveInputIntent::Steer, LiveInputIntent::Interrupt] {
            assert_eq!(
                LiveInputIntent::from_str_lossy(intent.as_str()),
                Some(intent)
            );
        }
        assert_eq!(LiveInputIntent::from_str_lossy("nope"), None);
    }
}
