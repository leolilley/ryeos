//! Closed classes of fail-closed adapter errors.
//!
//! A class is attached where the failing step is known, as an error context,
//! and `main` turns it into one stable exit code and one compile-time word.
//! Nothing here is ever derived from error text, so provider responses, URLs,
//! settings and credentials cannot reach the process output through it.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailClass {
    RequestParse,
    ArtifactDigest,
    SpecDigest,
    SettingsDigest,
    SettingsShape,
    UploadModeOrDigest,
    NetInputs,
    CredentialFormat,
    SourceGetTransport,
    SourceGetStatusOrShape,
    SourceNotDenyAllRunning,
    TokenStatus,
    ProxyRoute,
    UploadStatus,
    CreateStatus,
    Deadline,
    Other,
}

impl FailClass {
    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            Self::Other => 2,
            Self::RequestParse => 3,
            Self::ArtifactDigest => 4,
            Self::SpecDigest => 5,
            Self::SettingsDigest => 6,
            Self::SettingsShape => 7,
            Self::UploadModeOrDigest => 8,
            Self::NetInputs => 9,
            Self::CredentialFormat => 10,
            Self::SourceGetTransport => 11,
            Self::SourceGetStatusOrShape => 12,
            Self::SourceNotDenyAllRunning => 13,
            Self::TokenStatus => 14,
            Self::ProxyRoute => 15,
            Self::UploadStatus => 16,
            Self::CreateStatus => 17,
            Self::Deadline => 18,
        }
    }

    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Other => "other",
            Self::RequestParse => "request_parse",
            Self::ArtifactDigest => "artifact_digest",
            Self::SpecDigest => "spec_digest",
            Self::SettingsDigest => "settings_digest",
            Self::SettingsShape => "settings_shape",
            Self::UploadModeOrDigest => "upload_mode_or_digest",
            Self::NetInputs => "net_inputs",
            Self::CredentialFormat => "credential_format",
            Self::SourceGetTransport => "source_get_transport",
            Self::SourceGetStatusOrShape => "source_get_status_or_shape",
            Self::SourceNotDenyAllRunning => "source_not_deny_all_running",
            Self::TokenStatus => "token_status",
            Self::ProxyRoute => "proxy_route",
            Self::UploadStatus => "upload_status",
            Self::CreateStatus => "create_status",
            Self::Deadline => "deadline",
        }
    }

    /// The class attached to an error, or `Other` when no step classified it.
    pub(crate) fn of(error: &anyhow::Error) -> Self {
        error.downcast_ref::<Self>().copied().unwrap_or(Self::Other)
    }
}

impl fmt::Display for FailClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.word())
    }
}

impl std::error::Error for FailClass {}

/// Attach a class to a failing step. An error already classified where it
/// was created keeps that class.
pub(crate) trait Classify<T> {
    fn classed(self, class: FailClass) -> anyhow::Result<T>;
}

impl<T> Classify<T> for anyhow::Result<T> {
    fn classed(self, class: FailClass) -> anyhow::Result<T> {
        self.map_err(|error| {
            if error.downcast_ref::<FailClass>().is_some() {
                error
            } else {
                error.context(class)
            }
        })
    }
}

/// `ensure!` whose failure carries a class.
macro_rules! ensure_class {
    ($class:expr, $condition:expr, $message:literal) => {
        if !$condition {
            return Err(anyhow::anyhow!($message).context($class));
        }
    };
}
pub(crate) use ensure_class;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_distinct_and_in_range() {
        let all = [
            FailClass::RequestParse,
            FailClass::ArtifactDigest,
            FailClass::SpecDigest,
            FailClass::SettingsDigest,
            FailClass::SettingsShape,
            FailClass::UploadModeOrDigest,
            FailClass::NetInputs,
            FailClass::CredentialFormat,
            FailClass::SourceGetTransport,
            FailClass::SourceGetStatusOrShape,
            FailClass::SourceNotDenyAllRunning,
            FailClass::TokenStatus,
            FailClass::ProxyRoute,
            FailClass::UploadStatus,
            FailClass::CreateStatus,
            FailClass::Deadline,
            FailClass::Other,
        ];
        let mut codes: Vec<i32> = all.iter().map(|class| class.exit_code()).collect();
        assert!(codes.iter().all(|code| (2..=20).contains(code)));
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
    }

    #[test]
    fn first_class_wins_and_unclassified_is_other() {
        let first: anyhow::Result<()> = Err(anyhow::anyhow!("provider text"));
        let error = first
            .classed(FailClass::TokenStatus)
            .classed(FailClass::Other)
            .unwrap_err();
        assert_eq!(FailClass::of(&error), FailClass::TokenStatus);
        assert_eq!(
            FailClass::of(&anyhow::anyhow!("unclassified")),
            FailClass::Other
        );
    }
}
