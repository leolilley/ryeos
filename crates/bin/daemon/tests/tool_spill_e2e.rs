//! An oversize tool result end to end against the scripted chat stub: the tool
//! returns 200 KB, the model's next request carries a bounded preview that says
//! it is partial, and the record holds the spill, names the stored attachment
//! in `attachment_refs` and never carries the full text.
//!
//! These need the bundle bins, so run them with a bundle state that matches
//! the branch.

#[path = "common/stub_rig.rs"]
mod stub_rig;

use std::time::Duration;

use serde_json::Value;
use stub_rig::{RigOptions, chain_payloads, launch_on, start_rig, thread_detail, wait_terminal};

const MAX_WAIT: Duration = Duration::from_secs(150);

#[tokio::test(flavor = "multi_thread")]
async fn an_oversize_tool_result_is_stored_and_the_model_sees_a_partial_preview() {
    let rig = start_rig(
        "steps:\n  - tool_calls:\n      - { id: call_0, name: big_text_big_text, arguments: {} }\n  - text: \"done\"\n",
        RigOptions {
            spill_tools: true,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    let thread_id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &thread_id, 800).await
    );

    // The wire: the tool message is the preview, not 200 KB.
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 2);
    let history = bodies[1]["messages"].as_array().expect("messages");
    let tool = history.last().expect("last message");
    assert_eq!(tool["role"], "tool");
    let text = tool["content"].as_str().expect("tool text");
    assert!(
        text.starts_with("[tool result partial: "),
        "{}",
        &text[..120]
    );
    assert!(
        text.lines()
            .next()
            .unwrap()
            .ends_with("; the full result is not available to you]")
    );
    assert!(text.len() < 40 * 1024, "{}", text.len());

    // The record: the spill, and the attachment named after any images.
    let results = chain_payloads(&rig.h, &thread_id, "tool_call_result").await;
    assert_eq!(results.len(), 1);
    let payload = &results[0];
    let guard = &payload["guard"];
    assert_eq!(guard["truncated"], true);
    assert_eq!(guard["truncated_reason"], "result_spilled");
    let spill = &guard["spill"];
    let attachment = spill["attachment"].as_str().expect("attachment hash");
    assert_eq!(attachment.len(), 64);
    assert_eq!(payload["attachment_refs"], Value::from(vec![attachment]));
    assert_eq!(spill["total_bytes"], guard["result_size_bytes"]);
    assert_eq!(spill["stored_bytes"], spill["total_bytes"]);
    assert!(
        payload.to_string().len() < 60 * 1024,
        "the event must not hold the full text"
    );
    assert!(
        text.contains(attachment),
        "the preview names the attachment"
    );
}
