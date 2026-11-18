//! Images a tool returns, end to end against the scripted chat stub: the tool
//! answers with a `result_parts` envelope, the runtime stores the bytes as
//! thread attachments and records only their hashes, and the next request
//! carries each image in a user message after the tool results. The stub
//! records every request body, so the assertions are about the wire.
//!
//! These need the bundle bins, so run them with a bundle state that matches
//! the branch.

#[path = "common/stub_rig.rs"]
mod stub_rig;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use stub_rig::{
    Rig, RigOptions, chain_payloads, launch_on, start_rig, thread_detail, wait_terminal,
};

const MAX_WAIT: Duration = Duration::from_secs(150);

fn execution(extra: &str) -> String {
    format!(
        "category: \"ryeos-runtime\"\nretries: 3\nbackoff_base_ms: 100\nbackoff_max_ms: 1000\n{extra}"
    )
}

/// The provider entry of an image-capable model.
fn image_input(max_images_per_request: u32) -> String {
    format!(
        "image_input:\n  accepts:\n    media_types: [image/png]\n    max_bytes_per_image: 4096\n    max_images_per_request: {max_images_per_request}\n    text_part_template: {{type: text, text: \"{{text}}\"}}\n    image_part_template: {{type: image_url, image_url: {{url: \"{{data_url}}\"}}}}\n    tool_result_note: \"[{{count}} image(s) follow in the next message]\"\n    deferred_label: \"Image {{index}} from tool result {{call_id}}:\"\n    omitted_note: \"[earlier image omitted: {{media_type}} {{width}}x{{height}}]\"\n    cost: {{pixels_per_token: 750, max_tokens: 1600, unknown_dimensions_tokens: 1600}}\n"
    )
}

fn calls(names: &[&str]) -> String {
    let mut script = String::from("steps:\n");
    for (index, name) in names.iter().enumerate() {
        script.push_str(&format!(
            "  - tool_calls:\n      - {{ id: call_{index}, name: {name}, arguments: {{}} }}\n"
        ));
    }
    script.push_str("  - text: \"done\"\n");
    script
}

async fn rig(script: &str, opts: RigOptions) -> Rig {
    start_rig(
        script,
        RigOptions {
            image_tools: true,
            ..opts
        },
    )
    .await
    .expect("start rig")
}

async fn run_to_completion(rig: &Rig) -> String {
    let thread_id = launch_on(rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &thread_id, 800).await
    );
    thread_id
}

/// The base64 of every good image the tools returned, in call order.
fn returned_images(rig: &Rig) -> Vec<String> {
    std::fs::read_to_string(rig.aux.path().join("shots.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn data_url(b64: &str) -> String {
    format!("data:image/png;base64,{b64}")
}

/// Every image URL in a request body, in order.
fn image_urls(body: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    for message in body["messages"].as_array().expect("messages") {
        if let Some(blocks) = message["content"].as_array() {
            for block in blocks {
                if block["type"] == "image_url" {
                    urls.push(block["image_url"]["url"].as_str().expect("url").to_owned());
                }
            }
        }
    }
    urls
}

fn messages(body: &Value) -> &Vec<Value> {
    body["messages"].as_array().expect("messages")
}

fn is_hash(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

// 1 ── the image reaches the next request after the tool results ─────────

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_image_reaches_the_next_request_as_a_user_image_after_the_tool_results() {
    let rig = rig(
        &calls(&["shot_shot"]),
        RigOptions {
            provider_top_extra: image_input(4),
            ..RigOptions::default()
        },
    )
    .await;
    let thread_id = run_to_completion(&rig).await;

    let sent = returned_images(&rig);
    assert_eq!(sent.len(), 1);
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 2);
    let history = messages(&bodies[1]);
    let last = history.last().expect("last message");
    assert_eq!(last["role"], "user");
    assert_eq!(
        last["content"],
        json!([
            {"type": "text", "text": "Image 1 from tool result call_0:"},
            {"type": "image_url", "image_url": {"url": data_url(&sent[0])}}
        ])
    );
    let tool = &history[history.len() - 2];
    assert_eq!(tool["role"], "tool");
    let text = tool["content"].as_str().expect("tool text");
    assert!(text.contains("shot 1"), "{text}");
    assert!(
        text.ends_with("[1 image(s) follow in the next message]"),
        "{text}"
    );

    // The record holds the hash and the size the runtime read from the
    // header, never the bytes.
    let results = chain_payloads(&rig.h, &thread_id, "tool_call_result").await;
    assert_eq!(results.len(), 1);
    let payload = &results[0];
    assert_eq!(payload["parts"][0]["type"], "text");
    let image = &payload["parts"][1];
    assert_eq!(image["type"], "image");
    assert_eq!(image["media_type"], "image/png");
    assert_eq!(
        (image["width"].clone(), image["height"].clone()),
        (json!(1), json!(1))
    );
    assert!(is_hash(&image["attachment"]), "{image}");
    assert_eq!(payload["attachment_refs"], json!([image["attachment"]]));
    assert!(
        !payload.to_string().contains(&sent[0]),
        "the event must not hold the image bytes"
    );
}

// 2 ── a model not declared image-capable ────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_model_not_declared_image_capable_gets_an_error_result_and_nothing_is_stored() {
    let rig = rig(&calls(&["shot_shot"]), RigOptions::default()).await;
    let thread_id = run_to_completion(&rig).await;
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 2);
    assert!(image_urls(&bodies[1]).is_empty());
    let tool = messages(&bodies[1]).last().expect("last message").clone();
    assert_eq!(tool["role"], "tool");
    assert!(
        tool["content"]
            .as_str()
            .expect("tool text")
            .contains("image_input_unsupported"),
        "{tool}"
    );
    let results = chain_payloads(&rig.h, &thread_id, "tool_call_result").await;
    assert_eq!(results[0]["status"], "error");
    assert!(results[0].get("attachment_refs").is_none());
    assert_eq!(results[0]["parts"].as_array().expect("parts").len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_text_note_entry_keeps_the_result_text_and_stores_nothing() {
    let rig = rig(
        &calls(&["shot_shot"]),
        RigOptions {
            provider_top_extra: "image_input:\n  unsupported: text_note\n".to_string(),
            ..RigOptions::default()
        },
    )
    .await;
    let thread_id = run_to_completion(&rig).await;
    let bodies = rig.stub.request_bodies().await;
    let tool = messages(&bodies[1]).last().expect("last message").clone();
    assert_eq!(
        tool["content"],
        "shot 1\n[image 1 not used: image_input_unsupported]"
    );
    assert!(image_urls(&bodies[1]).is_empty());
    let results = chain_payloads(&rig.h, &thread_id, "tool_call_result").await;
    assert_eq!(results[0]["status"], "ok");
    assert_eq!(results[0]["guard"]["truncated"], true);
    assert_eq!(
        results[0]["guard"]["truncated_reason"],
        "image_input_unsupported"
    );
    assert!(results[0].get("attachment_refs").is_none());
}

// 3 ── only the newest images are carried ────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_request_carries_only_the_newest_images_and_notes_the_older_ones_in_place() {
    let rig = rig(
        &calls(&["shot_shot", "shot_shot", "shot_shot"]),
        RigOptions {
            provider_top_extra: image_input(2),
            ..RigOptions::default()
        },
    )
    .await;
    let thread_id = run_to_completion(&rig).await;
    let sent = returned_images(&rig);
    assert_eq!(sent.len(), 3);
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 4);
    let last = &bodies[3];
    assert_eq!(
        image_urls(last),
        vec![data_url(&sent[1]), data_url(&sent[2])]
    );
    // The note stands alone in its message, so it is plain text there.
    let note = "[earlier image omitted: image/png 1x1]";
    let notes = messages(last)
        .iter()
        .filter(|message| message["role"] == "user" && message["content"] == note)
        .count();
    assert_eq!(notes, 1);
    // All three were stored and recorded; only the request is bounded.
    let results = chain_payloads(&rig.h, &thread_id, "tool_call_result").await;
    assert_eq!(
        results
            .iter()
            .filter(|payload| payload["parts"][1]["type"] == "image")
            .count(),
        3
    );
    // An earlier request is built the same way every time: the second request
    // carried one image, the third two.
    assert_eq!(image_urls(&bodies[1]).len(), 1);
    assert_eq!(image_urls(&bodies[2]).len(), 2);
}

// 4 ── the signed per-run cap ────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn the_per_run_cap_refuses_an_image_past_the_limit_and_the_run_goes_on() {
    let rig = rig(
        &calls(&["shot_shot", "shot_shot", "shot_shot"]),
        RigOptions {
            provider_top_extra: image_input(4),
            exec_yaml: Some(execution("tool_images:\n  max_images_per_run: 2\n")),
            ..RigOptions::default()
        },
    )
    .await;
    let thread_id = run_to_completion(&rig).await;
    let results = chain_payloads(&rig.h, &thread_id, "tool_call_result").await;
    assert_eq!(results.len(), 3);
    assert_eq!(results[1]["parts"][1]["type"], "image");
    let refused = &results[2];
    assert_eq!(refused["parts"].as_array().expect("parts").len(), 1);
    assert_eq!(refused["guard"]["truncated_reason"], "image_limit");
    assert!(refused.get("attachment_refs").is_none());
    assert!(
        refused["parts"][0]["text"]
            .as_str()
            .expect("text")
            .contains("[image 1 not used: image_limit]")
    );
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(image_urls(bodies.last().expect("last request")).len(), 2);
}

// 5 ── bad images are refused with stable codes ──────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_type_and_an_oversize_image_are_dropped_with_their_codes() {
    let rig = rig(
        &calls(&["shot_bad_shot_bad", "shot_big_shot_big"]),
        RigOptions {
            provider_top_extra: image_input(4),
            ..RigOptions::default()
        },
    )
    .await;
    let thread_id = run_to_completion(&rig).await;
    let results = chain_payloads(&rig.h, &thread_id, "tool_call_result").await;
    assert_eq!(results.len(), 2);
    // The first result lost its GIF-as-PNG and kept the good image.
    assert_eq!(results[0]["guard"]["truncated_reason"], "image_media_type");
    assert_eq!(results[0]["parts"].as_array().expect("parts").len(), 2);
    assert!(
        results[0]["parts"][0]["text"]
            .as_str()
            .expect("text")
            .contains("[image 1 not used: image_media_type]")
    );
    // The second held only an oversize image.
    assert_eq!(results[1]["guard"]["truncated_reason"], "image_oversize");
    assert_eq!(results[1]["parts"].as_array().expect("parts").len(), 1);
    assert!(results[1].get("attachment_refs").is_none());
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(image_urls(bodies.last().expect("last request")).len(), 1);
}

// 6 ── crash mid-loop: the resumed request carries the same bytes ────────

const RESUME_SCRIPT: &str = "steps:\n  - tool_calls:\n      - { id: call_0, name: shot_shot, arguments: {} }\n  - tool_calls:\n      - { id: call_1, name: marker_marker, arguments: {} }\n  - text: \"done\"\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_mid_loop_resumes_with_the_same_image_bytes_read_back_from_the_node() {
    let mut rig = rig(
        RESUME_SCRIPT,
        RigOptions {
            provider_top_extra: image_input(4),
            marker_sleep_seconds: 25,
            ..RigOptions::default()
        },
    )
    .await;
    let thread_id = launch_on(&rig).await.expect("launch");
    let deadline = Instant::now() + Duration::from_secs(90);
    while rig.marker_count() == 0 {
        assert!(Instant::now() < deadline, "the second tool never started");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The image tool settled before the crash; the new process holds none of
    // its bytes and must read them back from the node.
    rig.h.restart().await.expect("kill and restart the node");
    let (status, _) = wait_terminal(&rig.h, &thread_id, MAX_WAIT).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &thread_id, 800).await
    );
    let sent = returned_images(&rig);
    assert_eq!(sent.len(), 1, "the settled image tool must not run again");
    let bodies = rig.stub.request_bodies().await;
    let last = bodies.last().expect("final request");
    assert_eq!(image_urls(last), vec![data_url(&sent[0])]);
    // The image sits where the live run puts it: after its own tool result.
    let history = messages(last);
    let at = history
        .iter()
        .position(|message| message["role"] == "user" && message["content"].is_array())
        .expect("image message");
    assert_eq!(history[at - 1]["role"], "tool");
}

// 7 ── a continuation successor reads the images back too ────────────────

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_successor_sends_the_images_of_its_predecessor() {
    let script = "steps:\n  - tool_calls:\n      - { id: call_0, name: shot_shot, arguments: {} }\n  - text: \"done\"\n  - text: \"again\"\n";
    let rig = rig(
        script,
        RigOptions {
            provider_top_extra: image_input(4),
            ..RigOptions::default()
        },
    )
    .await;
    let first = run_to_completion(&rig).await;
    let (status, body) = rig
        .h
        .post_execute(
            "service:threads/input",
            &rig.project_str(),
            json!({
                "input": "continue",
                "target": {"kind": "thread", "thread_id": first},
            }),
        )
        .await
        .expect("post threads.input");
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "follow-up refused: {body:#}"
    );
    let successor = body
        .pointer("/result/thread_id")
        .and_then(Value::as_str)
        .expect("successor thread id")
        .to_string();
    let (status, _) = wait_terminal(&rig.h, &successor, MAX_WAIT).await;
    assert_eq!(status, "completed");
    let sent = returned_images(&rig);
    let bodies = rig.stub.request_bodies().await;
    assert_eq!(bodies.len(), 3);
    assert_eq!(image_urls(&bodies[2]), vec![data_url(&sent[0])]);
}
