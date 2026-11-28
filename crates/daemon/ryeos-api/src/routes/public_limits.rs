//! The node's limits on the keyless public-page routes, held outside the route
//! table and keyed by route id.
//!
//! Each route the node's `public_routes` policy names has a rolling 24-hour
//! byte ledger, kept in a file so a restart does not reset the day, and a
//! concurrency limit. The limiter is built once at start from the loaded
//! policy and never rebuilt while the node runs, so neither resets.
//!
//! A public request first takes a permit, held until its response body has
//! been sent or dropped; with none free it is answered 503
//! `public_route_busy`. Before the first byte is sent the whole body length is
//! reserved against the route's day, and never refunded, a client that
//! disconnects included; over the cap it is answered 429
//! `public_route_quota_exhausted` with `Retry-After`. A HEAD or a 304 reserves
//! no bytes but takes a permit. Both refusals carry only their code: no node,
//! operator, version or principal, and no cookie.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use ryeos_node_runtime::node_policy::sections::public_routes::{
    PublicRouteLimits, PublicRoutesPolicy,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::published::ByteLedger;

pub const PUBLIC_ROUTE_BUSY: &str = "public_route_busy";
pub const PUBLIC_ROUTE_QUOTA_EXHAUSTED: &str = "public_route_quota_exhausted";
/// Seconds a client is told to wait after the day's cap is spent.
const RETRY_AFTER_SECONDS: &str = "3600";

struct RouteLimit {
    limits: PublicRouteLimits,
    ledger: Mutex<ByteLedger>,
    permits: Arc<Semaphore>,
}

/// Per-route limits for every public route the node serves.
pub struct PublicRouteLimiter {
    routes: HashMap<String, RouteLimit>,
}

/// The ledger file for one public route, in the node's state directory.
pub fn ledger_path(state_dir: &Path, route_id: &str) -> PathBuf {
    state_dir.join(format!(
        "public-route-{}.ledger",
        lillux::cas::sha256_hex(route_id.as_bytes())
    ))
}

fn code_only(status: StatusCode, code: &'static str) -> Response {
    (status, axum::Json(serde_json::json!({ "code": code }))).into_response()
}

impl PublicRouteLimiter {
    /// A limiter for a node that serves no public route.
    pub fn empty() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    /// Build the limiter from the node's policy, loading each named route's
    /// ledger from `state_dir`.
    pub fn build(state_dir: &Path, policy: &PublicRoutesPolicy, now_ms: i64) -> Self {
        let routes = policy
            .routes
            .iter()
            .map(|(id, limits)| {
                (
                    id.clone(),
                    RouteLimit {
                        limits: *limits,
                        ledger: Mutex::new(ByteLedger::load(ledger_path(state_dir, id), now_ms)),
                        permits: Arc::new(Semaphore::new(limits.concurrent_max as usize)),
                    },
                )
            })
            .collect();
        Self { routes }
    }

    /// Take a permit for a request to `route_id`, or the 503 refusal. A route
    /// the policy does not name was never compiled, so it is never asked for;
    /// if it is, it is refused the same way.
    pub fn admit(&self, route_id: &str) -> Result<OwnedSemaphorePermit, Response> {
        let Some(route) = self.routes.get(route_id) else {
            return Err(code_only(
                StatusCode::SERVICE_UNAVAILABLE,
                PUBLIC_ROUTE_BUSY,
            ));
        };
        Arc::clone(&route.permits)
            .try_acquire_owned()
            .map_err(|_| code_only(StatusCode::SERVICE_UNAVAILABLE, PUBLIC_ROUTE_BUSY))
    }

    /// Charge `response` to `route_id`'s day and return it with `permit` held
    /// until its body has been sent or dropped, or the 429 refusal.
    pub fn charge(
        &self,
        route_id: &str,
        method: &Method,
        response: Response,
        permit: OwnedSemaphorePermit,
        now_ms: i64,
    ) -> Response {
        let Some(route) = self.routes.get(route_id) else {
            return code_only(StatusCode::SERVICE_UNAVAILABLE, PUBLIC_ROUTE_BUSY);
        };
        let (mut parts, body) = response.into_parts();
        let carries_body = *method != Method::HEAD && parts.status != StatusCode::NOT_MODIFIED;
        let length = axum::body::HttpBody::size_hint(&body).exact();
        if carries_body {
            // A public route is static: its body is in memory and its length
            // exact. Anything else is not charged blind.
            let Some(length) = length else {
                return code_only(StatusCode::SERVICE_UNAVAILABLE, PUBLIC_ROUTE_BUSY);
            };
            let reserved = route
                .ledger
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .reserve(now_ms, length, route.limits.daily_bytes);
            match reserved {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    let mut refused =
                        code_only(StatusCode::TOO_MANY_REQUESTS, PUBLIC_ROUTE_QUOTA_EXHAUSTED);
                    refused.headers_mut().insert(
                        header::RETRY_AFTER,
                        HeaderValue::from_static(RETRY_AFTER_SECONDS),
                    );
                    return refused;
                }
                Err(_) => return code_only(StatusCode::SERVICE_UNAVAILABLE, PUBLIC_ROUTE_BUSY),
            }
        }
        if let Some(length) = length {
            parts.headers.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from_str(&length.to_string()).expect("a length is a header value"),
            );
        }
        let held = tokio_stream::StreamExt::map(body.into_data_stream(), move |chunk| {
            let _held = &permit;
            chunk
        });
        Response::from_parts(parts, Body::from_stream(held))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(daily_bytes: u64, concurrent_max: u32) -> PublicRoutesPolicy {
        let mut policy = PublicRoutesPolicy::as_shipped();
        policy.routes.insert(
            "public/front".into(),
            PublicRouteLimits {
                daily_bytes,
                concurrent_max,
            },
        );
        policy
    }

    fn page(bytes: usize) -> Response {
        (StatusCode::OK, vec![b'x'; bytes]).into_response()
    }

    async fn drain(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    async fn code_of(response: Response) -> (StatusCode, serde_json::Value, Option<String>) {
        let status = response.status();
        let retry = response
            .headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_owned());
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        let body: serde_json::Value = serde_json::from_slice(&drain(response).await).unwrap();
        (status, body, retry)
    }

    #[tokio::test]
    async fn bytes_are_reserved_before_sending_and_the_day_runs_out() {
        let dir = tempfile::tempdir().unwrap();
        let limiter = PublicRouteLimiter::build(dir.path(), &policy(10, 4), 1_000);

        let permit = limiter.admit("public/front").unwrap();
        let sent = limiter.charge("public/front", &Method::GET, page(6), permit, 1_000);
        assert_eq!(sent.status(), StatusCode::OK);
        assert_eq!(sent.headers()[header::CONTENT_LENGTH], "6");
        assert_eq!(drain(sent).await.len(), 6);

        // 6 of 10 spent: another 6 does not fit, and the refusal carries only
        // its code and a Retry-After.
        let permit = limiter.admit("public/front").unwrap();
        let refused = limiter.charge("public/front", &Method::GET, page(6), permit, 1_001);
        let (status, body, retry) = code_of(refused).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            body,
            serde_json::json!({ "code": PUBLIC_ROUTE_QUOTA_EXHAUSTED })
        );
        assert_eq!(retry.as_deref(), Some(RETRY_AFTER_SECONDS));

        // A restart reads the day back from the ledger file.
        let restarted = PublicRouteLimiter::build(dir.path(), &policy(10, 4), 1_002);
        let permit = restarted.admit("public/front").unwrap();
        let refused = restarted.charge("public/front", &Method::GET, page(6), permit, 1_002);
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn a_dropped_body_is_not_refunded() {
        let dir = tempfile::tempdir().unwrap();
        let limiter = PublicRouteLimiter::build(dir.path(), &policy(10, 4), 1_000);
        let permit = limiter.admit("public/front").unwrap();
        let sent = limiter.charge("public/front", &Method::GET, page(8), permit, 1_000);
        drop(sent);
        let permit = limiter.admit("public/front").unwrap();
        let refused = limiter.charge("public/front", &Method::GET, page(3), permit, 1_001);
        assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn head_and_not_modified_reserve_nothing_but_take_a_permit() {
        let dir = tempfile::tempdir().unwrap();
        let limiter = PublicRouteLimiter::build(dir.path(), &policy(1, 1), 1_000);

        let permit = limiter.admit("public/front").unwrap();
        // The only permit is held while that response is unsent.
        let (status, body, _) = code_of(limiter.admit("public/front").unwrap_err()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, serde_json::json!({ "code": PUBLIC_ROUTE_BUSY }));
        let head = limiter.charge("public/front", &Method::HEAD, page(100), permit, 1_000);
        assert_eq!(head.status(), StatusCode::OK);
        drop(head);

        let permit = limiter.admit("public/front").expect("the permit came back");
        let not_modified = limiter.charge(
            "public/front",
            &Method::GET,
            StatusCode::NOT_MODIFIED.into_response(),
            permit,
            1_000,
        );
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
        drop(not_modified);

        // Neither spent the one-byte day.
        let permit = limiter.admit("public/front").unwrap();
        let sent = limiter.charge("public/front", &Method::GET, page(1), permit, 1_000);
        assert_eq!(sent.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn the_permit_is_held_until_the_body_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let limiter = PublicRouteLimiter::build(dir.path(), &policy(1_000, 1), 1_000);
        let permit = limiter.admit("public/front").unwrap();
        let sent = limiter.charge("public/front", &Method::GET, page(4), permit, 1_000);
        // The handler has returned, but the body is unsent: still busy.
        assert!(limiter.admit("public/front").is_err());
        assert_eq!(drain(sent).await.len(), 4);
        assert!(limiter.admit("public/front").is_ok());
    }

    #[test]
    fn a_route_the_policy_does_not_name_is_refused() {
        let limiter = PublicRouteLimiter::empty();
        assert!(limiter.admit("public/front").is_err());
    }
}
