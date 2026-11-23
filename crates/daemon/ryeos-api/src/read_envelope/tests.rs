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
            "not a cursor for this read",
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
    let base = revision_of("node_journal", "auth", &tips, "f");
    assert!(base.starts_with("rev1:"));
    assert_eq!(base, revision_of("node_journal", "auth", &tips, "f"));
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
        revision_of("node_journal", "auth", &tips, "g"),
        revision_of("threads", "auth", &tips, "f"),
        revision_of(
            "node_journal",
            &[
                ("journal", "a"),
                ("ledger", "b"),
                ("trust", "c"),
                ("policy", "p2"),
            ],
            "f",
        ),
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
fn key_ids_are_typed_once_and_validated() {
    let hex = "ab".repeat(32);
    assert_eq!(key_id(&hex).unwrap(), format!("fp:{hex}"));
    assert_eq!(key_id(&format!("fp:{hex}")).unwrap(), format!("fp:{hex}"));
    for junk in ["ab", "fp:ab", &"AB".repeat(32), &"zz".repeat(32), ""] {
        assert!(key_id(junk).is_err(), "{junk}");
    }
}

#[test]
fn the_revision_follows_the_callers_authorization() {
    let policy = "gen1";
    let wide = authorization_digest("fp:k", &["a".to_owned(), "b".to_owned()], policy);
    let narrowed = authorization_digest("fp:k", &["a".to_owned()], policy);
    let reordered = authorization_digest("fp:k", &["b".to_owned(), "a".to_owned()], policy);
    let other_key = authorization_digest("fp:j", &["a".to_owned(), "b".to_owned()], policy);
    let other_policy = authorization_digest("fp:k", &["a".to_owned(), "b".to_owned()], "gen2");
    assert_eq!(wide, reordered);
    let tips = [("journal", "a")];
    let base = revision_of("node_journal", &wide, &tips, "f");
    for changed in [narrowed, other_key, other_policy] {
        assert_ne!(base, revision_of("node_journal", &changed, &tips, "f"));
    }
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
            record(&"a".repeat(64), Standing::Active),
            record(&"b".repeat(64), Standing::Revoked),
            record(&"c".repeat(64), Standing::Retired),
        ],
        chain: Vec::new(),
        signed_at: Vec::new(),
        registry_digest: "d".to_owned(),
    };
    let fp = |c: &str| c.repeat(64);
    let table = signers_table(
        [
            format!("fp:{}", fp("b")).as_str(),
            fp("c").as_str(),
            fp("e").as_str(),
            fp("f").as_str(),
        ],
        &view,
        &fp("e"),
    )
    .unwrap();
    let key = |c: &str| format!("fp:{}", fp(c));
    assert_eq!(
        table.keys().cloned().collect::<Vec<_>>(),
        [key("b"), key("c"), key("e"), key("f")]
    );
    assert_eq!(table[&key("b")].status, SignerStatus::Revoked);
    assert_eq!(table[&key("c")].status, SignerStatus::Retired);
    assert_eq!(
        table[&key("c")].label.as_deref(),
        Some(format!("label-{}", fp("c")).as_str())
    );
    assert_eq!(table[&key("e")].status, SignerStatus::Node);
    assert_eq!(table[&key("f")].status, SignerStatus::Unknown);
    assert!(!table.contains_key(&key("a")));
    assert!(signers_table(["junk"], &view, &fp("e")).is_err());
}

#[test]
fn coverage_extras_never_overwrite_state_or_reason() {
    let coverage = Coverage::observed().with_extra("truncations", json!([]));
    assert!(coverage.extra.contains_key("truncations"));
    let result = std::panic::catch_unwind(|| {
        Coverage::with_reason(CoverageState::Partial, "history_rotated")
            .with_extra("state", json!("observed"))
    });
    if cfg!(debug_assertions) {
        assert!(result.is_err(), "debug builds refuse the key loudly");
    } else {
        let coverage = result.unwrap();
        assert!(coverage.extra.is_empty());
        assert_eq!(coverage.state, CoverageState::Partial);
    }
}

#[test]
fn a_filter_digest_is_stable_for_a_fixed_field_struct() {
    #[derive(serde::Serialize)]
    struct Filter {
        types: std::collections::BTreeSet<&'static str>,
        since: Option<&'static str>,
    }
    let one = Filter {
        types: ["b", "a"].into_iter().collect(),
        since: None,
    };
    let two = Filter {
        types: ["a", "b"].into_iter().collect(),
        since: None,
    };
    assert_eq!(filter_digest(&one).unwrap(), filter_digest(&two).unwrap());
}
