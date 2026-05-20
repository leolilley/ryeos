use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcRequest {
    pub request_id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: Value,
}

#[derive(Debug, Serialize)]
pub struct RpcResponse {
    pub request_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(request_id: u64, result: Value) -> Self {
        Self {
            request_id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(request_id: u64, code: &str, message: impl Into<String>) -> Self {
        Self {
            request_id,
            result: None,
            error: Some(RpcError {
                code: code.to_string(),
                message: message.into(),
                retryable: false,
                details: Value::Object(Default::default()),
            }),
        }
    }
}
