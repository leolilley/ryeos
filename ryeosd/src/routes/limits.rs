use std::time::Duration;

use axum::body::Bytes;
use axum::http::StatusCode;
use axum::response::IntoResponse;

use super::compile::CompiledLimits;

pub struct RouteLimiter {
    pub body_bytes_max: u64,
    pub timeout: Duration,
}

impl RouteLimiter {
    pub fn from_limits(limits: &CompiledLimits) -> Self {
        Self {
            body_bytes_max: limits.body_bytes_max,
            timeout: Duration::from_millis(limits.timeout_ms),
        }
    }

    pub fn check_content_length(&self, headers: &axum::http::HeaderMap) -> Result<(), axum::response::Response> {
        if let Some(content_length) = headers
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        {
            if content_length > self.body_bytes_max {
                return Err(
                    (StatusCode::PAYLOAD_TOO_LARGE, axum::Json(serde_json::json!({
                        "error": format!("body too large: {} bytes (max {})", content_length, self.body_bytes_max)
                    })))
                        .into_response(),
                );
            }
        }
        Ok(())
    }

    pub async fn read_bounded_body(
        &self,
        body: axum::body::Body,
    ) -> Result<Bytes, axum::response::Response> {
        let max_bytes = self.body_bytes_max as usize;
        let bytes = axum::body::to_bytes(body, max_bytes)
            .await
            .map_err(|_| {
                (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    axum::Json(serde_json::json!({
                        "error": format!("body exceeded {} bytes", self.body_bytes_max)
                    })),
                )
                    .into_response()
            })?;

        if bytes.len() as u64 > self.body_bytes_max {
            return Err(
                (StatusCode::PAYLOAD_TOO_LARGE, axum::Json(serde_json::json!({
                    "error": format!("body too large: {} bytes (max {})", bytes.len(), self.body_bytes_max)
                })))
                    .into_response(),
            );
        }

        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_length_check_passes_within_limit() {
        let limiter = RouteLimiter {
            body_bytes_max: 1024,
            timeout: Duration::from_secs(30),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(axum::http::header::CONTENT_LENGTH, "512".parse().unwrap());
        assert!(limiter.check_content_length(&headers).is_ok());
    }

    #[test]
    fn content_length_check_rejects_over_limit() {
        let limiter = RouteLimiter {
            body_bytes_max: 1024,
            timeout: Duration::from_secs(30),
        };
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(axum::http::header::CONTENT_LENGTH, "2048".parse().unwrap());
        assert!(limiter.check_content_length(&headers).is_err());
    }

    #[test]
    fn content_length_check_passes_no_header() {
        let limiter = RouteLimiter {
            body_bytes_max: 1024,
            timeout: Duration::from_secs(30),
        };
        let headers = axum::http::HeaderMap::new();
        assert!(limiter.check_content_length(&headers).is_ok());
    }
}
