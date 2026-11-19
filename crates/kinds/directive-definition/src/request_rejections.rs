//! Request rejections, declared as provider data.
//!
//! A provider refuses some requests before streaming starts, and the status
//! alone is too coarse to name why (a bare 400 is also a malformed request).
//! An entry lists rows that pair statuses with phrases of the response body
//! and name a failure code for the match. The runtime reads the rows and
//! carries no provider name and no meaning for any code: a code is an opaque
//! token a parent graph can branch on.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Most rows an entry can declare.
pub const REQUEST_REJECTIONS_MAX_ROWS: usize = 16;
/// Most body phrases one row can declare.
pub const REQUEST_REJECTION_MAX_PHRASES: usize = 16;
/// Required prefix of a rejection code, so a provider-declared code can never
/// collide with a code the runtime itself reports.
pub const REQUEST_REJECTION_CODE_PREFIX: &str = "provider_";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRejectionRow {
    /// The failure code a match reports. Lowercase letters, digits and
    /// underscores, at most 64 bytes, starting with `provider_`.
    pub code: String,
    /// The HTTP statuses the row applies to.
    pub statuses: Vec<u16>,
    /// The row matches when the response body contains any of these phrases,
    /// compared without regard to case.
    pub body_contains_any: Vec<String>,
}

impl RequestRejectionRow {
    fn matches(&self, status: u16, lowered_body: &str) -> bool {
        self.statuses.contains(&status)
            && self
                .body_contains_any
                .iter()
                .any(|phrase| lowered_body.contains(&phrase.to_lowercase()))
    }
}

/// The code of the first row that matches a refused request, or `None` when
/// no row does (the failure then stays the generic provider failure).
pub fn classify_request_rejection<'a>(
    rows: &'a [RequestRejectionRow],
    status: u16,
    body: &str,
) -> Option<&'a str> {
    let lowered = body.to_lowercase();
    rows.iter()
        .find(|row| row.matches(status, &lowered))
        .map(|row| row.code.as_str())
}

fn valid_code(code: &str) -> bool {
    code.len() <= 64
        && code.starts_with(REQUEST_REJECTION_CODE_PREFIX)
        && code.len() > REQUEST_REJECTION_CODE_PREFIX.len()
        && code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Validates the rows of one entry or profile.
pub fn validate_request_rejections(rows: &[RequestRejectionRow], context: &str) -> Result<()> {
    if rows.is_empty() || rows.len() > REQUEST_REJECTIONS_MAX_ROWS {
        bail!(
            "provider config{context}: request_rejections must hold 1-{REQUEST_REJECTIONS_MAX_ROWS} rows"
        );
    }
    for row in rows {
        if !valid_code(&row.code) {
            bail!(
                "provider config{context}: request_rejections code {:?} must be lowercase letters, digits and underscores, at most 64 bytes, and start with {REQUEST_REJECTION_CODE_PREFIX}",
                row.code
            );
        }
        if row.statuses.is_empty() || row.statuses.iter().any(|s| !(400..=599).contains(s)) {
            bail!(
                "provider config{context}: request_rejections row {} must list statuses between 400 and 599",
                row.code
            );
        }
        if row.body_contains_any.is_empty()
            || row.body_contains_any.len() > REQUEST_REJECTION_MAX_PHRASES
        {
            bail!(
                "provider config{context}: request_rejections row {} must hold 1-{REQUEST_REJECTION_MAX_PHRASES} body phrases",
                row.code
            );
        }
        for phrase in &row.body_contains_any {
            if phrase.is_empty() || phrase.len() > 256 || phrase.chars().any(char::is_control) {
                bail!(
                    "provider config{context}: request_rejections row {} has a body phrase that is empty, over 256 bytes or holds control characters",
                    row.code
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(code: &str, statuses: &[u16], phrases: &[&str]) -> RequestRejectionRow {
        RequestRejectionRow {
            code: code.to_string(),
            statuses: statuses.to_vec(),
            body_contains_any: phrases.iter().map(|p| p.to_string()).collect(),
        }
    }

    fn rows() -> Vec<RequestRejectionRow> {
        vec![row(
            "provider_context_length_exceeded",
            &[400, 413],
            &["context length", "too long"],
        )]
    }

    #[test]
    fn a_status_and_a_body_phrase_name_the_code() {
        let rows = rows();
        let code =
            classify_request_rejection(&rows, 400, "This model's Maximum CONTEXT LENGTH is 8");
        assert_eq!(code, Some("provider_context_length_exceeded"));
        assert_eq!(
            classify_request_rejection(&rows, 413, "prompt is too long"),
            Some("provider_context_length_exceeded")
        );
    }

    #[test]
    fn no_match_leaves_the_failure_generic() {
        let rows = rows();
        assert_eq!(
            classify_request_rejection(&rows, 400, "malformed json"),
            None
        );
        assert_eq!(
            classify_request_rejection(&rows, 500, "context length"),
            None
        );
        assert_eq!(classify_request_rejection(&rows, 400, ""), None);
        assert_eq!(classify_request_rejection(&[], 400, "context length"), None);
    }

    #[test]
    fn the_first_matching_row_wins() {
        let rows = vec![
            row("provider_first", &[400], &["limit"]),
            row("provider_second", &[400], &["limit"]),
        ];
        assert_eq!(
            classify_request_rejection(&rows, 400, "over the limit"),
            Some("provider_first")
        );
    }

    #[test]
    fn rows_validate_their_code_statuses_and_phrases() {
        assert!(validate_request_rejections(&rows(), "").is_ok());
        for bad in [
            row("context_length", &[400], &["x"]),
            row("provider_", &[400], &["x"]),
            row("Provider_x", &[400], &["x"]),
            row(&format!("provider_{}", "a".repeat(60)), &[400], &["x"]),
            row("provider_x", &[], &["x"]),
            row("provider_x", &[200], &["x"]),
            row("provider_x", &[400], &[]),
            row("provider_x", &[400], &[""]),
            row("provider_x", &[400], &["a\nb"]),
        ] {
            assert!(
                validate_request_rejections(std::slice::from_ref(&bad), "").is_err(),
                "{bad:?}"
            );
        }
        assert!(validate_request_rejections(&[], "").is_err());
    }
}
