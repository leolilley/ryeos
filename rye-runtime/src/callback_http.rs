use async_trait::async_trait;
use serde_json::{json, Value};

use crate::callback::*;

pub struct HttpRuntimeClient {
    base_url: String,
    callback_token: String,
    thread_id: String,
    project_path: String,
    client: reqwest::Client,
}

impl HttpRuntimeClient {
    pub fn new(
        base_url: &str,
        callback_token: &str,
        thread_id: &str,
        project_path: &str,
    ) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            callback_token: callback_token.to_string(),
            thread_id: thread_id.to_string(),
            project_path: project_path.to_string(),
            client: reqwest::Client::new(),
        }
    }

    pub fn from_env() -> Result<Self, CallbackError> {
        let base_url = std::env::var("RYEOSD_URL")
            .map_err(|_| CallbackError::Transport(anyhow::anyhow!("RYEOSD_URL not set")))?;
        let callback_token = std::env::var("RYEOSD_CALLBACK_TOKEN")
            .map_err(|_| {
                CallbackError::Transport(anyhow::anyhow!("RYEOSD_CALLBACK_TOKEN not set"))
            })?;
        let thread_id = std::env::var("RYEOSD_THREAD_ID")
            .map_err(|_| CallbackError::Transport(anyhow::anyhow!("RYEOSD_THREAD_ID not set")))?;
        let project_path = std::env::var("RYEOSD_PROJECT_PATH")
            .map_err(|_| {
                CallbackError::Transport(anyhow::anyhow!("RYEOSD_PROJECT_PATH not set"))
            })?;
        Ok(Self::new(&base_url, &callback_token, &thread_id, &project_path))
    }

    fn map_reqwest_error(err: reqwest::Error) -> CallbackError {
        if err.status().map_or(false, |s| s == reqwest::StatusCode::UNAUTHORIZED) {
            return CallbackError::ActionFailed {
                code: "unauthorized".to_string(),
                message: "callback capability rejected".to_string(),
                retryable: false,
            };
        }
        CallbackError::Transport(err.into())
    }

    async fn call(&self, method: &str, extra: Value) -> Result<Value, CallbackError> {
        let url = format!("{}/runtime/{method}", self.base_url);
        let mut body = extra;
        if let Some(map) = body.as_object_mut() {
            map.insert("callback_token".to_string(), json!(self.callback_token));
            map.insert("thread_id".to_string(), json!(self.thread_id));
            map.insert("project_path".to_string(), json!(self.project_path));
        }

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(Self::map_reqwest_error)?;

        let status = resp.status();
        let value: Value = resp.json().await.map_err(Self::map_reqwest_error)?;

        if !status.is_success() {
            let error_msg = value["error"].as_str().unwrap_or("unknown error");
            return Err(CallbackError::ActionFailed {
                code: format!("http_{}", status.as_u16()),
                message: error_msg.to_string(),
                retryable: false,
            });
        }

        Ok(value)
    }
}

#[async_trait]
impl RuntimeCallbackAPI for HttpRuntimeClient {
    async fn dispatch_action(
        &self,
        request: DispatchActionRequest,
    ) -> Result<Value, CallbackError> {
        self.call(
            "dispatch_action",
            json!({
                "action": {
                    "item_id": request.action.item_id,
                    "kind": request.action.kind,
                    "params": request.action.params,
                    "thread": request.action.thread,
                },
            }),
        )
        .await
    }

    async fn attach_process(
        &self,
        _thread_id: &str,
        _pid: u32,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "not_available".to_string(),
            message: "attach_process not available over HTTP".to_string(),
            retryable: false,
        })
    }

    async fn mark_running(&self, _thread_id: &str) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "not_available".to_string(),
            message: "mark_running not available over HTTP".to_string(),
            retryable: false,
        })
    }

    async fn finalize_thread(
        &self,
        _thread_id: &str,
        status: &str,
    ) -> Result<Value, CallbackError> {
        self.call("finalize_thread", json!({ "status": status })).await
    }

    async fn get_thread(&self, _thread_id: &str) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "not_available".to_string(),
            message: "get_thread not available over HTTP".to_string(),
            retryable: false,
        })
    }

    async fn request_continuation(
        &self,
        _thread_id: &str,
        prompt: &str,
    ) -> Result<Value, CallbackError> {
        self.call("request_continuation", json!({ "prompt": prompt }))
            .await
    }

    async fn append_event(
        &self,
        _thread_id: &str,
        event_type: &str,
        payload: Value,
        storage_class: &str,
    ) -> Result<Value, CallbackError> {
        self.call(
            "append_event",
            json!({
                "event_type": event_type,
                "payload": payload,
                "storage_class": storage_class,
            }),
        )
        .await
    }

    async fn append_events(
        &self,
        _thread_id: &str,
        events: Vec<Value>,
    ) -> Result<Value, CallbackError> {
        self.call("append_events", json!({ "events": events })).await
    }

    async fn replay_events(&self, _thread_id: &str) -> Result<Value, CallbackError> {
        self.call("replay_events", json!({})).await
    }

    async fn claim_commands(&self, _thread_id: &str) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "not_available".to_string(),
            message: "claim_commands not available over HTTP".to_string(),
            retryable: false,
        })
    }

    async fn complete_command(
        &self,
        _thread_id: &str,
        command_id: &str,
        result: Value,
    ) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "not_available".to_string(),
            message: "complete_command not available over HTTP".to_string(),
            retryable: false,
        })
    }

    async fn reserve_budget(
        &self,
        _thread_id: &str,
        amount: f64,
    ) -> Result<Value, CallbackError> {
        self.call("reserve_budget", json!({ "amount": amount })).await
    }

    async fn report_budget(
        &self,
        _thread_id: &str,
        usage: Value,
    ) -> Result<Value, CallbackError> {
        self.call("report_budget", json!({ "usage": usage })).await
    }

    async fn release_budget(&self, _thread_id: &str) -> Result<Value, CallbackError> {
        self.call("release_budget", json!({})).await
    }

    async fn get_budget(&self, _thread_id: &str) -> Result<Value, CallbackError> {
        self.call("get_budget", json!({})).await
    }

    async fn publish_artifact(
        &self,
        _thread_id: &str,
        artifact: Value,
    ) -> Result<Value, CallbackError> {
        self.call("publish_artifact", json!({ "artifact": artifact }))
            .await
    }

    async fn set_facets(
        &self,
        _thread_id: &str,
        facets: Value,
    ) -> Result<Value, CallbackError> {
        self.call("set_facets", json!({ "facets": facets })).await
    }

    async fn get_facets(&self, _thread_id: &str) -> Result<Value, CallbackError> {
        Err(CallbackError::ActionFailed {
            code: "not_available".to_string(),
            message: "get_facets not available over HTTP".to_string(),
            retryable: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_fails_without_env_vars() {
        std::env::remove_var("RYEOSD_URL");
        std::env::remove_var("RYEOSD_CALLBACK_TOKEN");
        assert!(HttpRuntimeClient::from_env().is_err());
    }

    #[test]
    fn new_constructs_client() {
        let client = HttpRuntimeClient::new(
            "http://localhost:8080",
            "cbt-test123",
            "T-1",
            "/project",
        );
        assert_eq!(client.base_url, "http://localhost:8080");
        assert_eq!(client.callback_token, "cbt-test123");
    }

    #[test]
    fn new_strips_trailing_slash() {
        let client = HttpRuntimeClient::new(
            "http://localhost:8080/",
            "cbt-test",
            "T-1",
            "/project",
        );
        assert_eq!(client.base_url, "http://localhost:8080");
    }
}
