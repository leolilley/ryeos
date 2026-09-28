//! Shared bounded public fixture launch: one accepted ingress, exact-root reads,
//! no mutation retries or fabricated execution authority. Test harness only.

use crate::{common::DaemonHarness, production_service};
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use std::time::Duration;

/// A single ingress followed only by read observations of its returned root.
/// This deliberately does not use the existing Wait-only producer convenience
/// function: a lost Wait response would hide the accepted root coordinate.
pub(crate) async fn completed_launch(
    harness: &DaemonHarness,
    request: Value,
    launch_id: &str,
) -> Result<(Value, Value)> {
    let (accepted, observed) =
        terminal_launch(harness, request, launch_id, Duration::from_secs(180)).await?;
    ensure!(
        observed.pointer("/thread/status") == Some(&json!("completed"))
            && observed.pointer("/thread/chain_root_id") == accepted.get("thread_id"),
        "prerequisite did not complete as its exact root: launch_id={launch_id}, detail={observed}"
    );
    Ok((accepted, observed))
}

/// A verifier is expected to fail closed until its independent claims are
/// implemented. Preserve the accepted root and observe its terminal state
/// without treating a failure as a successful qualification.
pub(crate) async fn terminal_launch(
    harness: &DaemonHarness,
    request: Value,
    launch_id: &str,
    observation_timeout: Duration,
) -> Result<(Value, Value)> {
    ensure!(
        observation_timeout >= Duration::from_secs(1)
            && observation_timeout <= Duration::from_secs(600),
        "public fixture observation timeout outside bound"
    );
    eprintln!(
        "public fixture dispatch launch_id={launch_id}; retain original ingress on uncertainty"
    );
    let (status, accepted) = tokio::time::timeout(
        Duration::from_secs(60),
        harness.post_json("/execute/launch", request),
    )
    .await
    .with_context(|| {
        format!("acceptance timed out for launch_id={launch_id}; retain state, do not relaunch")
    })?
    .with_context(|| {
        format!("acceptance uncertain for launch_id={launch_id}; retain state, do not relaunch")
    })?;
    ensure!(
        status == reqwest::StatusCode::ACCEPTED && accepted["launch_id"] == launch_id,
        "launch_id={launch_id} not accepted exactly: {status}: {accepted}; do not relaunch"
    );
    let root = accepted["thread_id"]
        .as_str()
        .context("accepted prerequisite root absent")?
        .to_owned();
    ryeos_runtime::validate_runtime_thread_id(&root).map_err(anyhow::Error::msg)?;
    eprintln!("public fixture accepted launch_id={launch_id} thread_id={root}");
    let observed = tokio::time::timeout(observation_timeout, async {
        loop {
            let detail = production_service(harness, "service:threads/get", json!({"thread_id":root})).await?;
            ensure!(detail.pointer("/thread/thread_id") == Some(&json!(root)), "thread observation changed identity");
            let status = detail.pointer("/thread/status").and_then(Value::as_str).context("thread status absent")?;
            let status = ryeos_state::objects::ThreadStatus::from_str_lossy(status).context("unknown thread status")?;
            if status.is_terminal() { return Ok::<_, anyhow::Error>(detail); }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }).await.with_context(|| format!("observation expired launch_id={launch_id} thread_id={root}; retain state, do not relaunch"))?
        .with_context(|| format!("observation failed launch_id={launch_id} thread_id={root}; retain state, do not relaunch"))?;
    ensure!(
        observed.pointer("/thread/chain_root_id") == Some(&json!(root)),
        "terminal launch changed its exact root: launch_id={launch_id}, thread_id={root}, detail={observed}"
    );
    Ok((accepted, observed))
}
