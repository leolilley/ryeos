//! A bounded, secret-free account of why an adapter reported `uncertain`.
//!
//! The reason is diagnostic only: it never changes the uncertain outcome or
//! any recovery decision. The class is closed; the detail is free text that is
//! limited to printable ASCII and a fixed byte bound so no header, token, URL
//! or response body can ride along.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// The longest accepted `detail`, in bytes.
pub const MAX_UNCERTAINTY_DETAIL_BYTES: usize = 512;

/// Which step of the provider contact failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UncertaintyClass {
    TokenMint,
    UploadStatus,
    RunStatus,
    CreateStatus,
    ContentType,
    StreamBound,
    Parse,
    Validation,
    Timeout,
    Transport,
}

impl UncertaintyClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TokenMint => "token_mint",
            Self::UploadStatus => "upload_status",
            Self::RunStatus => "run_status",
            Self::CreateStatus => "create_status",
            Self::ContentType => "content_type",
            Self::StreamBound => "stream_bound",
            Self::Parse => "parse",
            Self::Validation => "validation",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UncertaintyReason {
    pub class: UncertaintyClass,
    pub detail: String,
}

impl UncertaintyReason {
    /// Build a reason from arbitrary text: non-printable bytes become `?` and
    /// the result is cut at the bound on a character boundary.
    pub fn new(class: UncertaintyClass, detail: &str) -> Self {
        let mut detail: String = detail
            .chars()
            .map(|c| {
                if c == ' ' || c.is_ascii_graphic() {
                    c
                } else {
                    '?'
                }
            })
            .collect();
        detail.truncate(MAX_UNCERTAINTY_DETAIL_BYTES);
        Self { class, detail }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.detail.len() <= MAX_UNCERTAINTY_DETAIL_BYTES
                && self
                    .detail
                    .chars()
                    .all(|c| c == ' ' || c.is_ascii_graphic()),
            "uncertainty reason detail exceeds its bound or is not printable ASCII"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_is_bounded_and_printable() {
        let reason = UncertaintyReason::new(UncertaintyClass::Parse, &"é\n".repeat(600));
        assert!(reason.detail.len() <= MAX_UNCERTAINTY_DETAIL_BYTES);
        reason.validate().unwrap();
        let bad = UncertaintyReason {
            class: UncertaintyClass::Parse,
            detail: "x".repeat(MAX_UNCERTAINTY_DETAIL_BYTES + 1),
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn unknown_fields_and_classes_are_refused() {
        assert!(
            serde_json::from_str::<UncertaintyReason>(
                r#"{"class":"parse","detail":"x","extra":1}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<UncertaintyReason>(r#"{"class":"nope","detail":"x"}"#).is_err()
        );
    }
}
