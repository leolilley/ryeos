//! Test-only tool executable that speaks the daemon callback protocol.
//!
//! A planted fixture tool launches this binary under the default
//! `tool_callback` protocol. It reads `{"callback": <name>, "params": {...}}`
//! from stdin, makes that one callback through the shipped runtime client
//! (`ryeos_runtime::callback_uds::UdsRuntimeClient`, built from the
//! environment the daemon injected), and prints one JSON line:
//!
//! * `{"ryeos_callback_probe":true,"ok":true,"result":<daemon result>}`
//! * `{"ryeos_callback_probe":true,"ok":false,"code":<code>,"message":<text>}`
//!
//! A refused callback still exits zero, so the launch itself succeeds and the
//! refusal code reaches the test intact.

use std::io::Read as _;

use anyhow::{Context as _, Result, bail};
use ryeos_runtime::callback::{CallbackError, RuntimeCallbackAPI};
use ryeos_runtime::callback_uds::UdsRuntimeClient;
use serde_json::{Value, json};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("read probe request from stdin")?;
    let request: Value = serde_json::from_str(&input).context("probe request must be JSON")?;
    let callback = request
        .get("callback")
        .and_then(Value::as_str)
        .context("probe request needs a `callback` name")?
        .to_owned();
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));

    let thread_id = std::env::var("RYEOSD_THREAD_ID")
        .context("RYEOSD_THREAD_ID is set only inside a daemon-dispatched tool")?;
    let client = UdsRuntimeClient::from_env()
        .map_err(|error| anyhow::anyhow!("cannot build the callback client: {error}"))?;

    let outcome = match callback.as_str() {
        "bundle_events_append" => client.bundle_events_append(&thread_id, params).await,
        "bundle_events_read_chain" => client.bundle_events_read_chain(&thread_id, params).await,
        "bundle_events_scan" => client.bundle_events_scan(&thread_id, params).await,
        "author_item" => client.author_item(&thread_id, params).await,
        other => bail!("unknown callback `{other}`"),
    };

    let report = match outcome {
        Ok(result) => json!({"ryeos_callback_probe": true, "ok": true, "result": result}),
        Err(CallbackError::ActionFailed { code, message, .. }) => json!({
            "ryeos_callback_probe": true,
            "ok": false,
            "code": code,
            "message": message,
        }),
        Err(error) => bail!("callback transport failure: {error}"),
    };
    println!("{report}");
    Ok(())
}
