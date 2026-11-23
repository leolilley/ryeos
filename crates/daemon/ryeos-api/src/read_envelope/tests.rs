use super::*;
use serde_json::json;

const CODEC: CursorCodec = CursorCodec::new("node_journal");

fn bad_request(error: HandlerError) -> String {
    match error {
        HandlerError::BadRequest(message) => message,
        other => panic!("not a bad request: {other:?}"),
    }
}

#[test]
fn a_cursor_returns_its_position_only_to_its_own_read_filter_and_order() {
    let filter = filter_digest(&json!({"types": ["node.started"]})).unwrap();
    let cursor = CODEC.encode(&filter, "at_desc", "100.node_journal.7");
    assert_eq!(
        CODEC.decode(&cursor, &filter, "at_desc").unwrap(),
        "100.node_journal.7"
    );

    let other_filter = filter_digest(&json!({"types": ["node.exited"]})).unwrap();
    for (error, why) in [
        (
            CODEC.decode(&cursor, &other_filter, "at_desc").unwrap_err(),
            "another filter",
        ),
        (
            CODEC.decode(&cursor, &filter, "at_asc").unwrap_err(),
            "another order",
        ),
        (
            CursorCodec::new("threads")
                .decode(&cursor, &filter, "at_desc")
                .unwrap_err(),
            "another read",
        ),
        (
            CODEC
                .decode("not base64 !", &filter, "at_desc")
                .unwrap_err(),
            "not one this read issued",
        ),
        (
            CODEC.decode("", &filter, "at_desc").unwrap_err(),
            "empty or too long",
        ),
    ] {
        let message = bad_request(error);
        assert!(message.contains("_cursor_invalid"), "{message}");
        assert!(message.contains(why), "{message}");
    }
    let threads = bad_request(
        CursorCodec::new("threads")
            .decode(&cursor, &filter, "at_desc")
            .unwrap_err(),
    );
    assert!(threads.starts_with("threads_cursor_invalid"), "{threads}");
}

#[test]
fn a_cursor_carries_only_the_position_and_its_binding() {
    let cursor = CODEC.encode("f", "o", "p");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .unwrap();
    let payload: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        payload,
        json!({"read": "node_journal", "filter_digest": "f", "order": "o", "position": "p"})
    );
}

#[test]
fn the_revision_changes_with_any_source_tip_and_with_the_filter() {
    let tips = [("journal", "a"), ("ledger", "b"), ("trust", "c")];
    let base = revision_of("node_journal", &tips, "f");
    assert!(base.starts_with("rev1:"));
    assert_eq!(base, revision_of("node_journal", &tips, "f"));
    for changed in [
        revision_of(
            "node_journal",
            &[("journal", "a"), ("ledger", "b"), ("trust", "x")],
            "f",
        ),
        revision_of(
            "node_journal",
            &[("journal", "x"), ("ledger", "b"), ("trust", "c")],
            "f",
        ),
        revision_of("node_journal", &tips, "g"),
        revision_of("threads", &tips, "f"),
    ] {
        assert_ne!(base, changed);
    }
}

#[test]
fn not_modified_answers_only_an_exact_revision() {
    assert_eq!(
        not_modified(Some("rev1:x"), "rev1:x"),
        Some(NotModified {
            not_modified: true,
            revision: "rev1:x".to_owned()
        })
    );
    assert_eq!(not_modified(Some("rev1:y"), "rev1:x"), None);
    assert_eq!(not_modified(None, "rev1:x"), None);
}

#[test]
fn limits_default_and_are_bounded() {
    assert_eq!(page_limit(None).unwrap(), DEFAULT_LIMIT);
    assert_eq!(page_limit(Some(MAX_LIMIT)).unwrap(), MAX_LIMIT);
    for bad in [0, MAX_LIMIT + 1] {
        assert!(bad_request(page_limit(Some(bad)).unwrap_err()).contains("limit must be"));
    }
}

#[test]
fn key_ids_are_typed_once() {
    assert_eq!(key_id("ab"), "fp:ab");
    assert_eq!(key_id("fp:ab"), "fp:ab");
}

#[test]
fn the_envelope_serializes_in_the_shared_shape() {
    let coverage = Coverage::with_reason(CoverageState::Partial, "history_truncated")
        .with_extra("truncations", json!([{"source": "node_journal"}]));
    let envelope = ReadEnvelope::rows(
        "ryeos.read.node.journal/v1",
        "rev1:x".to_owned(),
        coverage,
        vec![json!({"id": "r1"})],
        Page {
            next_cursor: None,
            total: Some(1),
            total_basis: TotalBasis::Exact,
        },
        BTreeMap::from([(
            "fp:ab".to_owned(),
            SignerEntry {
                label: Some("operator".to_owned()),
                role: Some("operator".to_owned()),
                status: SignerStatus::Retired,
            },
        )]),
    );
    let value = serde_json::to_value(&envelope).unwrap();
    assert_eq!(value["schema"], "ryeos.read.node.journal/v1");
    assert_eq!(value["coverage"]["state"], "partial");
    assert_eq!(value["coverage"]["reason"], "history_truncated");
    assert_eq!(
        value["coverage"]["truncations"][0]["source"],
        "node_journal"
    );
    assert_eq!(value["page"]["total_basis"], "exact");
    assert_eq!(value["signers"]["fp:ab"]["status"], "retired");
    assert!(value.get("record").is_none());
    assert!(lillux::time::unix_secs_from_iso8601(value["as_of"].as_str().unwrap()).is_ok());

    let single = ReadEnvelope::record(
        "ryeos.read.records.get/v1",
        "rev1:y".to_owned(),
        Coverage::observed(),
        json!({"id": "r1"}),
        BTreeMap::new(),
    );
    let value = serde_json::to_value(&single).unwrap();
    assert_eq!(value["record"]["id"], "r1");
    assert!(value.get("rows").is_none() && value.get("page").is_none());
    assert_eq!(value["coverage"]["reason"], Value::Null);
}

#[test]
fn every_envelope_leaf_is_listed() {
    for leaf in [
        "/schema",
        "/as_of",
        "/revision",
        "/coverage/state",
        "/coverage/reason",
        "/page/next_cursor",
        "/page/total",
        "/page/total_basis",
        "/signers/*/label",
        "/signers/*/role",
        "/signers/*/status",
        "/not_modified",
    ] {
        assert!(ENVELOPE_LEAVES.contains(&leaf), "{leaf}");
    }
}

#[test]
fn the_signers_table_names_exactly_the_given_keys_with_fresh_standing() {
    use ryeos_engine::trust::{
        SignerRecord, SignerRole, SignerScope, SignerSpace, SignerStatus as Standing,
    };
    let record = |fingerprint: &str, status: Standing| SignerRecord {
        schema: "x".to_owned(),
        fingerprint: fingerprint.to_owned(),
        public_key: "ed25519:k".to_owned(),
        label: format!("label-{fingerprint}"),
        role: SignerRole::Operator,
        scope: SignerScope {
            spaces: vec![SignerSpace::Node],
            kinds: vec!["*".to_owned()],
            projects: Vec::new(),
        },
        status,
        event: "e".to_owned(),
    };
    let view = ryeos_node::trust_registry::RegistryView {
        records: vec![
            record("aa", Standing::Active),
            record("bb", Standing::Revoked),
            record("cc", Standing::Retired),
        ],
        chain: Vec::new(),
        signed_at: Vec::new(),
        registry_digest: "d".to_owned(),
    };
    let table = signers_table(["fp:bb", "cc", "nn", "zz"], &view, "nn");
    assert_eq!(
        table.keys().collect::<Vec<_>>(),
        ["fp:bb", "fp:cc", "fp:nn", "fp:zz"]
    );
    assert_eq!(table["fp:bb"].status, SignerStatus::Revoked);
    assert_eq!(table["fp:cc"].status, SignerStatus::Retired);
    assert_eq!(table["fp:cc"].label.as_deref(), Some("label-cc"));
    assert_eq!(table["fp:nn"].status, SignerStatus::Node);
    assert_eq!(table["fp:zz"].status, SignerStatus::Unknown);
    assert!(!table.contains_key("fp:aa"));
}
