//! Classification of post-contact failures into a bounded, secret-free
//! `UncertaintyReason`. The outcome stays `uncertain`; this only records which
//! step failed. Detail text is built from fixed words, HTTP status codes and
//! the provider's own error code and (shortened) message, never from headers,
//! tokens, URLs or other response content.

use std::io::Read as _;

use ryeos_external_execution_contract::{UncertaintyClass, UncertaintyReason};

/// Provider error messages are cut to this many bytes before they are quoted.
const MAX_PROVIDER_MESSAGE_BYTES: usize = 256;
/// Only this much of a provider error body is ever inspected.
const MAX_PROVIDER_BODY_INSPECTED_BYTES: u64 = 4096;

/// A classified contact failure carried as an `anyhow` error payload.
#[derive(Debug)]
pub(crate) struct ContactFailure(pub(crate) UncertaintyReason);

impl std::fmt::Display for ContactFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.0.class.as_str(), self.0.detail)
    }
}

impl std::error::Error for ContactFailure {}

pub(crate) fn failure(class: UncertaintyClass, detail: &str) -> anyhow::Error {
    anyhow::Error::new(ContactFailure(UncertaintyReason::new(class, detail)))
}

/// `status 4xx` plus the provider's error code and message when its body is
/// JSON carrying them.
pub(crate) fn provider_detail(status: u16, body: &[u8]) -> String {
    let mut detail = format!("status {status}");
    let inspected = &body[..body.len().min(MAX_PROVIDER_BODY_INSPECTED_BYTES as usize)];
    let Ok(serde_json::Value::Object(object)) = serde_json::from_slice(inspected) else {
        return detail;
    };
    for key in ["code", "message"] {
        let Some(serde_json::Value::String(value)) = object.get(key) else {
            continue;
        };
        let mut value = value.clone();
        if value.to_ascii_lowercase().contains("bearer") {
            value = "[redacted]".to_owned();
        }
        let mut end = value.len().min(MAX_PROVIDER_MESSAGE_BYTES);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        detail.push_str(&format!("; {key}: {}", &value[..end]));
    }
    detail
}

/// `provider_detail` for a response whose body has not been read yet.
pub(crate) fn response_detail(status: u16, body: impl std::io::Read) -> String {
    let mut bytes = Vec::new();
    let _ = body
        .take(MAX_PROVIDER_BODY_INSPECTED_BYTES)
        .read_to_end(&mut bytes);
    provider_detail(status, &bytes)
}

/// The reason for an error: its own classification when a step attached one,
/// a transport or timeout class for a transport error, else the caller's
/// fallback class with a fixed detail. Error text is never copied.
pub(crate) fn reason_of(
    error: &anyhow::Error,
    fallback: UncertaintyClass,
    fallback_detail: &str,
) -> UncertaintyReason {
    for cause in error.chain() {
        if let Some(failure) = cause.downcast_ref::<ContactFailure>() {
            return failure.0.clone();
        }
        if let Some(http) = cause.downcast_ref::<ryeos_http_transport::HttpError>() {
            let class = if http.kind() == std::io::ErrorKind::TimedOut {
                UncertaintyClass::Timeout
            } else {
                UncertaintyClass::Transport
            };
            return UncertaintyReason::new(class, &format!("{http} (io {:?})", http.kind()));
        }
    }
    UncertaintyReason::new(fallback, fallback_detail)
}

/// Re-attach `reason_of` as the error's classification for `?` propagation.
pub(crate) fn classified(
    error: anyhow::Error,
    fallback: UncertaintyClass,
    fallback_detail: &str,
) -> anyhow::Error {
    anyhow::Error::new(ContactFailure(reason_of(&error, fallback, fallback_detail)))
}

/// The reason a failed contact carries, for the adapter response.
pub(crate) fn contact_reason(error: &anyhow::Error) -> UncertaintyReason {
    reason_of(
        error,
        UncertaintyClass::Validation,
        "contact failed before any step classified it",
    )
}

/// Read a JSON provider response, classifying a refusal instead of
/// discarding it.
pub(crate) fn read_classified(
    response: ryeos_http_transport::HttpResponse,
) -> Result<(u16, Vec<u8>), UncertaintyReason> {
    let status = response.status;
    if !crate::has_json_content_type(&response.headers) {
        return Err(UncertaintyReason::new(
            UncertaintyClass::ContentType,
            &format!("status {status} without an exact JSON content type"),
        ));
    }
    let mut body = Vec::new();
    if let Err(error) = response
        .body
        .take(crate::MAX_API_RESPONSE_BYTES.saturating_add(1))
        .read_to_end(&mut body)
    {
        let class = if error.kind() == std::io::ErrorKind::TimedOut {
            UncertaintyClass::Timeout
        } else if ryeos_http_transport::is_response_refusal(&error) {
            UncertaintyClass::StreamBound
        } else {
            UncertaintyClass::Transport
        };
        return Err(UncertaintyReason::new(
            class,
            &format!("status {status} response body read failed"),
        ));
    }
    if body.len() as u64 > crate::MAX_API_RESPONSE_BYTES {
        return Err(UncertaintyReason::new(
            UncertaintyClass::StreamBound,
            &format!("status {status} response body exceeds its bound"),
        ));
    }
    Ok((status, body))
}

/// Why a fully read create response was not accepted.
pub(crate) fn create_rejection(status: u16, body: &[u8]) -> UncertaintyReason {
    if status != 201 {
        return UncertaintyReason::new(
            UncertaintyClass::CreateStatus,
            &format!("create {}", provider_detail(status, body)),
        );
    }
    let parsed = ryeos_external_execution_contract::from_json_slice_strict::<crate::RenderSandbox>(
        body,
        usize::try_from(crate::MAX_API_RESPONSE_BYTES).unwrap_or(usize::MAX),
    );
    match parsed {
        Err(_) => UncertaintyReason::new(
            UncertaintyClass::Parse,
            "create status 201 body did not parse as a sandbox",
        ),
        Ok(_) => UncertaintyReason::new(
            UncertaintyClass::Validation,
            "create status 201 sandbox differs from the requested projection",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::MAX_UNCERTAINTY_DETAIL_BYTES;

    const BEARER: &str = "rnd_secretbearer0123456789";

    #[test]
    fn each_step_failure_keeps_its_class() {
        for class in [
            UncertaintyClass::TokenMint,
            UncertaintyClass::UploadStatus,
            UncertaintyClass::RunStatus,
            UncertaintyClass::CreateStatus,
            UncertaintyClass::ContentType,
            UncertaintyClass::StreamBound,
            UncertaintyClass::Parse,
            UncertaintyClass::Validation,
        ] {
            let error = failure(class, "step failed").context("outer context");
            let reason = contact_reason(&error);
            assert_eq!(reason.class, class);
            assert_eq!(reason.detail, "step failed");
        }
    }

    #[test]
    fn unclassified_error_text_is_not_copied() {
        let error = anyhow::anyhow!("leaked Authorization: Bearer {BEARER}");
        let reason = classified(error, UncertaintyClass::Parse, "stream did not parse");
        let reason = contact_reason(&reason);
        assert_eq!(reason.class, UncertaintyClass::Parse);
        assert!(!reason.detail.contains(BEARER));
    }

    #[test]
    fn provider_detail_quotes_status_code_and_bounded_message_only() {
        let body = serde_json::json!({
            "code": "snapshot_not_found",
            "message": "m".repeat(1000),
            "authorization": format!("Bearer {BEARER}"),
        })
        .to_string();
        let detail = provider_detail(404, body.as_bytes());
        assert!(detail.starts_with("status 404; code: snapshot_not_found; message: m"));
        assert!(!detail.contains(BEARER));
        let reason = UncertaintyReason::new(UncertaintyClass::CreateStatus, &detail);
        assert!(reason.detail.len() <= MAX_UNCERTAINTY_DETAIL_BYTES);
        reason.validate().unwrap();
        assert!(detail.len() < 320);
    }

    #[test]
    fn provider_message_echoing_a_bearer_is_redacted() {
        let body = serde_json::json!({"message": format!("bad Bearer {BEARER}")}).to_string();
        let detail = provider_detail(401, body.as_bytes());
        assert_eq!(detail, "status 401; message: [redacted]");
        assert_eq!(provider_detail(502, b"<html>boom</html>"), "status 502");
    }
}
