//! Replay of recorded external-agent transcripts through the real bridge.
//!
//! The transcripts in `fixtures/` are frames a real agent emitted. Each
//! inbound notification goes through `StructuredWorkload::route_message` with
//! the signed production profile and its schemas, so a frame shape the profile
//! does not know, or a mapping that drifts, fails here before it fails in a
//! session. Only the child process is replaced: a sleeping shell stands in for
//! the agent, because the frames come from the transcript.

use super::*;

/// What the bridge did with one inbound notification.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Accepted and deliberately not recorded (`ignored_notifications`, or a
    /// rule that is not durable).
    Dropped,
    /// Accepted and recorded as one event of this type.
    Event(String),
    /// Refused; the text is the refusal's own message.
    Refused(String),
}

struct Transcript {
    thread_id: String,
    notifications: Vec<Value>,
}

fn load_transcript(text: &str) -> Transcript {
    let mut thread_id = None;
    let mut notifications = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let entry: Value = serde_json::from_str(line).expect("transcript line is JSON");
        if entry["type"] != "emit_inbound" {
            continue;
        }
        let frame = entry["frame"].clone();
        if frame.get("id").is_some() || frame.get("method").is_none() {
            // Responses answer the transcript's own requests; the bridge
            // correlates only its own.
            continue;
        }
        if thread_id.is_none() {
            thread_id = frame
                .pointer("/params/thread/id")
                .or_else(|| frame.pointer("/params/threadId"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        notifications.push(frame);
    }
    Transcript {
        thread_id: thread_id.expect("transcript names its thread"),
        notifications,
    }
}

fn codex_profile() -> StructuredSessionProfile {
    let mut profile: StructuredSessionProfile = serde_json::from_str(include_str!(
        "../../../../bundles/codex/.ai/workers/codex/lib/hosted/structured-session.profile.json"
    ))
    .expect("production Codex profile decodes");
    profile.workload_args = vec!["-c".to_owned(), "sleep 30".to_owned()];
    profile
}

fn codex_schemas() -> HashMap<String, Value> {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../bundles/codex/.ai/workers/codex/lib/hosted/schema");
    std::fs::read_dir(directory)
        .expect("Codex schema directory")
        .map(|entry| {
            let path = entry.expect("schema entry").path();
            let name = format!(
                "schema/{}",
                path.file_name().and_then(|name| name.to_str()).unwrap()
            );
            let value = serde_json::from_slice(&std::fs::read(&path).expect("schema bytes"))
                .expect("schema is JSON");
            (name, value)
        })
        .collect()
}

/// Route every notification of the transcript through the production Codex
/// profile, one frame at a time.
fn replay_codex(text: &str) -> Vec<(String, Outcome)> {
    let transcript = load_transcript(text);
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let events = Arc::new(Mutex::new(EventQueue::default()));
    let (_control_sender, controls) = sync_channel(1);
    let (result_sender, _results) = sync_channel(1);
    let mut lifecycle = BridgeLifecycle::default();
    let mut workload = StructuredWorkload::start(
        &mut lifecycle,
        "/bin/sh",
        root.path().to_str().unwrap(),
        home.to_str().unwrap(),
        codex_profile(),
        "session".to_owned(),
        HashSet::from([RouteEffectClass::SessionMutation]),
        codex_schemas(),
        Arc::clone(&events),
        controls,
        result_sender,
        "/bin/sh",
        None,
        &BTreeMap::new(),
        Vec::new(),
    )
    .expect("bridge starts against the stand-in child");
    workload.bound_session_id = Some(transcript.thread_id);
    let mut outcomes = Vec::new();
    for frame in transcript.notifications {
        let method = frame["method"].as_str().unwrap().to_owned();
        let before = events.lock().unwrap().events.len();
        let outcome = match workload.route_message(frame) {
            Err(error) => Outcome::Refused(format!("{error:#}")),
            Ok(()) => {
                let queue = events.lock().unwrap();
                match queue.events.get(before) {
                    Some(event) => Outcome::Event(
                        event["event_type"].as_str().unwrap_or_default().to_owned(),
                    ),
                    None => Outcome::Dropped,
                }
            }
        };
        outcomes.push((method, outcome));
    }
    outcomes
}

const CODEX_SIMPLE: &str = include_str!("../fixtures/codex/simple-0.156.1.ndjson");

#[test]
fn recorded_codex_turn_replays_through_the_production_profile() {
    let outcomes = replay_codex(CODEX_SIMPLE);
    for (method, outcome) in &outcomes {
        eprintln!("replay {method} -> {outcome:?}");
    }
    let usage: Vec<_> = outcomes
        .iter()
        .filter(|(method, _)| method == "thread/tokenUsage/updated")
        .collect();
    assert_eq!(usage.len(), 1, "the recorded turn carries one usage frame");
    // Today the profile ignores usage frames. When a declared usage
    // observation replaces this, this assertion changes with it.
    assert_eq!(usage[0].1, Outcome::Dropped);
    let completed: Vec<_> = outcomes
        .iter()
        .filter(|(method, _)| method == "turn/completed")
        .collect();
    assert_eq!(completed.len(), 1);
    assert!(
        matches!(completed[0].1, Outcome::Event(_)),
        "the recorded turn completion is recorded, got {:?}",
        completed[0].1
    );
}
