use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use hmac::{Hmac, Mac};
use sha2::{Sha256, Digest};
use serde_json::Value;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    AuthVerifier, CompiledAuthVerifier, RoutePrincipal, VerifierRequestContext,
};

type HmacSha256 = Hmac<Sha256>;

const TIMESTAMP_TOLERANCE_SECS: i64 = 300;
const REPLAY_WINDOW_SECS: u64 = 600;

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub struct HmacSha256V1Verifier;

impl AuthVerifier for HmacSha256V1Verifier {
    fn key(&self) -> &'static str {
        "hmac_sha256_v1"
    }

    fn validate_route_config(
        &self,
        auth_config: Option<&Value>,
    ) -> Result<Arc<dyn CompiledAuthVerifier>, RouteConfigError> {
        let config = auth_config.ok_or_else(|| RouteConfigError::Other(
            "hmac_sha256_v1 requires auth_config with 'secret_env'".to_string(),
        ))?;

        let secret_env = config
            .get("secret_env")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                RouteConfigError::Other(
                    "hmac_sha256_v1 auth_config missing 'secret_env'".to_string(),
                )
            })?;

        let secret = std::env::var(secret_env).map_err(|_| {
            RouteConfigError::Other(format!(
                "hmac_sha256_v1: environment variable '{secret_env}' not set"
            ))
        })?;

        Ok(Arc::new(CompiledHmacSha256V1 {
            secret,
            seen: Mutex::new(HashMap::new()),
        }))
    }
}

struct CompiledHmacSha256V1 {
    secret: String,
    seen: Mutex<HashMap<String, Instant>>,
}

impl CompiledHmacSha256V1 {
    fn compute_signature(&self, method: &str, path: &str, timestamp: i64, body_sha256: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(self.secret.as_bytes())
            .expect("HMAC accepts any key length");
        let payload = format!("{method}\n{path}\n{timestamp}\n{body_sha256}");
        mac.update(payload.as_bytes());
        let result = mac.finalize();
        to_hex(&result.into_bytes())
    }

    fn is_replay(&self, signature: &str) -> bool {
        let mut guard = self.seen.lock().expect("replay guard poisoned");
        let now = Instant::now();
        guard.retain(|_, exp| now.duration_since(*exp) < Duration::from_secs(REPLAY_WINDOW_SECS));
        if guard.contains_key(signature) {
            true
        } else {
            guard.insert(signature.to_string(), now);
            false
        }
    }
}

impl CompiledAuthVerifier for CompiledHmacSha256V1 {
    fn verify(
        &self,
        route_id: &str,
        req: &VerifierRequestContext,
        _state: &crate::state::AppState,
    ) -> Result<RoutePrincipal, RouteDispatchError> {
        let timestamp_str = req
            .headers
            .get("x-rye-hmac-timestamp")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                tracing::warn!(route_id, "hmac_sha256_v1: missing X-Rye-Hmac-Timestamp");
                RouteDispatchError::Unauthorized
            })?;

        let timestamp: i64 = timestamp_str.parse().map_err(|_| {
            tracing::warn!(route_id, "hmac_sha256_v1: invalid timestamp");
            RouteDispatchError::Unauthorized
        })?;

        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        if (now_secs - timestamp).abs() > TIMESTAMP_TOLERANCE_SECS {
            tracing::warn!(
                route_id,
                %timestamp,
                %now_secs,
                "hmac_sha256_v1: timestamp outside tolerance"
            );
            return Err(RouteDispatchError::Unauthorized);
        }

        let signature = req
            .headers
            .get("x-rye-hmac-signature")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                tracing::warn!(route_id, "hmac_sha256_v1: missing X-Rye-Hmac-Signature");
                RouteDispatchError::Unauthorized
            })?;

        if self.is_replay(signature) {
            tracing::warn!(route_id, "hmac_sha256_v1: replay detected");
            return Err(RouteDispatchError::Unauthorized);
        }

        let body_sha256 = to_hex(&Sha256::digest(req.body_raw));

        let expected = self.compute_signature(req.method.as_str(), req.path, timestamp, &body_sha256);

        if signature != expected {
            tracing::warn!(route_id, "hmac_sha256_v1: signature mismatch");
            return Err(RouteDispatchError::Unauthorized);
        }

        Ok(RoutePrincipal {
            id: format!("hmac:{route_id}"),
            scopes: vec![],
            verifier_key: "hmac_sha256_v1",
            verified: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_compiled(secret: &str) -> CompiledHmacSha256V1 {
        CompiledHmacSha256V1 {
            secret: secret.to_string(),
            seen: Mutex::new(HashMap::new()),
        }
    }

    #[test]
    fn signature_is_deterministic() {
        let cv = make_compiled("test-secret");
        let body_sha256 = to_hex(&Sha256::digest(b"hello"));
        let sig1 = cv.compute_signature("POST", "/api/hook", 1700000000, &body_sha256);
        let sig2 = cv.compute_signature("POST", "/api/hook", 1700000000, &body_sha256);
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn signature_differs_for_different_inputs() {
        let cv = make_compiled("test-secret");
        let body_sha256 = to_hex(&Sha256::digest(b"hello"));
        let sig1 = cv.compute_signature("POST", "/api/hook", 1700000000, &body_sha256);
        let sig2 = cv.compute_signature("GET", "/api/hook", 1700000000, &body_sha256);
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn replay_detected() {
        let cv = make_compiled("test-secret");
        assert!(!cv.is_replay("sig-aaa"));
        assert!(cv.is_replay("sig-aaa"));
        assert!(!cv.is_replay("sig-bbb"));
    }

    #[test]
    fn config_missing_secret_env_rejected() {
        let v = HmacSha256V1Verifier;
        let config = serde_json::json!({"secret_key": "inline"});
        let result = v.validate_route_config(Some(&config));
        assert!(result.is_err());
        let msg = format!("{}", result.err().unwrap());
        assert!(msg.contains("secret_env"), "got: {msg}");
    }

    #[test]
    fn config_missing_auth_config_rejected() {
        let v = HmacSha256V1Verifier;
        let result = v.validate_route_config(None);
        assert!(result.is_err());
        let msg = format!("{}", result.err().unwrap());
        assert!(msg.contains("secret_env"), "got: {msg}");
    }

    #[test]
    fn config_with_valid_secret_env_succeeds() {
        std::env::set_var("RYEOS_TEST_HMAC_SECRET_XYZ", "my-secret-123");
        let v = HmacSha256V1Verifier;
        let config = serde_json::json!({"secret_env": "RYEOS_TEST_HMAC_SECRET_XYZ"});
        let result = v.validate_route_config(Some(&config));
        assert!(result.is_ok());
    }
}
