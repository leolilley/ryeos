//! Recorded and pinned context, end to end against the scripted chat stub.
//!
//! A directive with `before` and `after` context positions records them as
//! pinned inputs. These tests prove what the model is actually sent: the
//! context messages are in the record, a crash resume rebuilds the same
//! request, and a successor keeps the pinned prefix whatever the carried tail.
//! They need populated bundles (`-f bundles=branch`).

use ryeosd::test_support as common;

#[path = "common/stub_rig.rs"]
mod stub_rig;
use stub_rig::*;

use std::path::Path;
use std::time::Duration;

use lillux::crypto::SigningKey;
use serde_json::Value;

const BEFORE_TEXT: &str = "PIN-BEFORE: the output contract is a single sentence.";
const AFTER_TEXT: &str = "PIN-AFTER: end every answer with the word finished.";

/// The directive header that declares both context positions.
const CONTEXT_HEADER: &str = "context:\n  before:\n    - \"knowledge:pin/before\"\n  after:\n    - \"knowledge:pin/after\"\n";

/// Plant a knowledge item at `<project>/.ai/knowledge/<rel>.md`.
fn plant_knowledge(
    project: &Path,
    rel: &str,
    body: &str,
    signer: &SigningKey,
) -> anyhow::Result<()> {
    let path = project.join(format!(".ai/knowledge/{rel}.md"));
    std::fs::create_dir_all(path.parent().expect("knowledge parent dir"))?;
    let stem = Path::new(rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel);
    let category = Path::new(rel)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    let content = format!("```yaml\nname: {stem}\ncategory: \"{category}\"\n```\n\n{body}\n");
    let signed = lillux::signature::sign_content(&content, signer, "<!--", Some("-->"));
    std::fs::write(&path, signed)?;
    Ok(())
}

fn plant_context(rig: &Rig) -> anyhow::Result<()> {
    plant_knowledge(
        rig.project.path(),
        "pin/before",
        BEFORE_TEXT,
        &rig.publisher,
    )?;
    plant_knowledge(rig.project.path(), "pin/after", AFTER_TEXT, &rig.publisher)
}

/// The roles and texts of a request's non-system messages.
fn conversation(body: &Value) -> Vec<(String, String)> {
    body.get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter(|m| m.get("role").and_then(Value::as_str) != Some("system"))
                .map(|m| {
                    (
                        m.get("role")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_string(),
                        m.get("content")
                            .map(|c| match c {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                            .unwrap_or_default(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The launch batch's inputs: `cognition_in` payloads that carry an origin
/// (turn markers carry none).
async fn input_payloads(rig: &Rig, root: &str) -> Vec<Value> {
    chain_payloads(&rig.h, root, "cognition_in")
        .await
        .into_iter()
        .filter(|payload| payload.get("origin").is_some())
        .collect()
}

/// E1. The context positions are recorded as inputs in the order the model is
/// given them, each pinned, and the first request carries them with their
/// acknowledgements.
#[tokio::test(flavor = "multi_thread")]
async fn context_is_recorded_as_pinned_inputs() {
    let rig = start_rig(
        "steps:\n  - text: done\n",
        RigOptions {
            header_extra: CONTEXT_HEADER,
            ..RigOptions::default()
        },
    )
    .await
    .expect("start rig");
    plant_context(&rig).expect("plant context");
    let id = launch_on(&rig).await.expect("launch");
    let (status, _) = wait_terminal(&rig.h, &id, Duration::from_secs(120)).await;
    assert_eq!(
        status,
        "completed",
        "{}",
        thread_detail(&rig.h, &id, 2000).await
    );

    let inputs = input_payloads(&rig, &id).await;
    let origins: Vec<&str> = inputs
        .iter()
        .map(|p| p["origin"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(origins, ["context", "launch", "context"], "{inputs:?}");
    for input in &inputs {
        assert_eq!(input["pinned"], true, "{input}");
    }
    assert!(inputs[0]["ack"].is_string(), "{}", inputs[0]);

    let requests = rig.stub.requests().await;
    let first = conversation(&requests[0].body);
    let roles: Vec<&str> = first.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user", "user", "assistant"],
        "{first:?}"
    );
    assert!(first[0].1.contains(BEFORE_TEXT), "{first:?}");
    assert!(first[3].1.contains(AFTER_TEXT), "{first:?}");
}
