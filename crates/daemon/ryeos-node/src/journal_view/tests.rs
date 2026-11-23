use super::*;
use crate::node_journal::{JournalDraft, JournalType};

fn row(
    source: Source,
    sequence: u64,
    at: &str,
    row_type: &str,
    subject: Option<&str>,
) -> JournalRow {
    JournalRow {
        id: digest_ref(source, sequence, "d"),
        source,
        row_type: row_type.to_owned(),
        at: at.to_owned(),
        at_basis: AtBasis::Body,
        subject_ref: subject.map(str::to_owned),
        before_ref: None,
        after_ref: None,
        actor_key: None,
        actor_basis: None,
        attested_by: Some("node".to_owned()),
        detail: BTreeMap::new(),
        unix_secs: unix_secs("at", at).unwrap(),
        sequence,
    }
}

fn ids(page: &JournalPage) -> Vec<&str> {
    page.rows.iter().map(|row| row.id.as_str()).collect()
}

fn sample() -> Vec<JournalRow> {
    vec![
        row(
            Source::NodeJournal,
            1,
            "2026-11-20T00:00:00Z",
            "node.started",
            None,
        ),
        row(
            Source::OfflineLedger,
            1,
            "2026-11-20T00:00:05Z",
            "node.policy-apply",
            Some("r1"),
        ),
        row(
            Source::TrustChain,
            3,
            "2026-11-20T00:00:05Z",
            "trust.pinned",
            Some("k1"),
        ),
        row(
            Source::NodeJournal,
            2,
            "2026-11-20T00:01:00Z",
            "node.exited",
            None,
        ),
    ]
}

#[test]
fn rows_are_newest_first_with_a_stable_order_at_equal_times() {
    let page = page(sample(), Vec::new(), Vec::new(), &JournalQuery::default()).unwrap();
    // Equal times order by source, descending: trust chain before the
    // offline ledger.
    assert_eq!(
        ids(&page),
        [
            "node_journal:2:d",
            "trust_chain:3:d",
            "offline_ledger:1:d",
            "node_journal:1:d"
        ]
    );
    assert_eq!(page.next_position, None);
    assert_eq!(page.total, 4);
}

#[test]
fn a_cursor_pages_older_rows_without_repeating_or_skipping() {
    let first = page(
        sample(),
        Vec::new(),
        Vec::new(),
        &JournalQuery {
            limit: Some(2),
            ..JournalQuery::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&first), ["node_journal:2:d", "trust_chain:3:d"]);
    let cursor = first.next_position.clone().expect("a next page");
    let second = page(
        sample(),
        Vec::new(),
        Vec::new(),
        &JournalQuery {
            limit: Some(2),
            position: Some(cursor),
            ..JournalQuery::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&second), ["offline_ledger:1:d", "node_journal:1:d"]);
    assert_eq!(second.next_position, None);
    // The total counts the filter, never the page.
    assert_eq!(first.total, 4);
    assert_eq!(second.total, 4);
}

#[test]
fn filters_select_by_type_window_and_subject() {
    let by_type = page(
        sample(),
        Vec::new(),
        Vec::new(),
        &JournalQuery {
            types: Some(BTreeSet::from([
                "node.started".to_owned(),
                "node.exited".to_owned(),
            ])),
            ..JournalQuery::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&by_type), ["node_journal:2:d", "node_journal:1:d"]);

    let window = page(
        sample(),
        Vec::new(),
        Vec::new(),
        &JournalQuery {
            since: Some("2026-11-20T00:00:05Z".to_owned()),
            until: Some("2026-11-20T00:00:05Z".to_owned()),
            ..JournalQuery::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&window), ["trust_chain:3:d", "offline_ledger:1:d"]);

    let subject = page(
        sample(),
        Vec::new(),
        Vec::new(),
        &JournalQuery {
            subject: Some("k1".to_owned()),
            ..JournalQuery::default()
        },
    )
    .unwrap();
    assert_eq!(ids(&subject), ["trust_chain:3:d"]);
}

#[test]
fn malformed_queries_are_refused() {
    for (query, refusal) in [
        (
            JournalQuery {
                limit: Some(0),
                ..JournalQuery::default()
            },
            "limit must be",
        ),
        (
            JournalQuery {
                limit: Some(MAX_PAGE_ROWS + 1),
                ..JournalQuery::default()
            },
            "limit must be",
        ),
        (
            JournalQuery {
                types: Some(BTreeSet::new()),
                ..JournalQuery::default()
            },
            "types must name",
        ),
        (
            JournalQuery {
                since: Some("yesterday".to_owned()),
                ..JournalQuery::default()
            },
            "not a canonical UTC instant",
        ),
        (
            JournalQuery {
                position: Some("1.nowhere.2".to_owned()),
                ..JournalQuery::default()
            },
            "names no source",
        ),
        (
            JournalQuery {
                position: Some("garbage".to_owned()),
                ..JournalQuery::default()
            },
            "not one this read returned",
        ),
    ] {
        let error = page(sample(), Vec::new(), Vec::new(), &query)
            .unwrap_err()
            .to_string();
        assert!(error.contains(refusal), "{error}");
    }
}

#[test]
fn journal_records_become_rows_attested_by_the_daemon() {
    let root = tempfile::tempdir().unwrap();
    let node =
        NodeIdentity::create(&root.path().join(".ai/node/identity/private_key.pem")).unwrap();
    let journal = NodeJournal::for_app_root(root.path(), node.clone());
    journal
        .append(JournalDraft::new(JournalType::NodeStarted).with("pid", 7))
        .unwrap();
    let mut endorsed = JournalDraft::new(JournalType::EndorsementAccepted);
    endorsed.actor = Some("a".repeat(64));
    endorsed.subject_ref = Some("item".to_owned());
    let written = journal.append(endorsed).unwrap();

    let (rows, truncation) = journal_rows(&journal).unwrap();
    assert_eq!(truncation, None);
    assert_eq!(rows.len(), 2);
    let started = &rows[0];
    assert_eq!(started.row_type, "node.started");
    assert_eq!(started.at_basis, AtBasis::Body);
    assert_eq!(started.actor_key, None);
    assert_eq!(started.actor_basis, None);
    assert_eq!(started.detail["pid"], json!(7));
    let accepted = &rows[1];
    assert_eq!(
        accepted.id,
        format!("node_journal:2:{}", written.digest().unwrap())
    );
    assert_eq!(accepted.actor_basis, Some(ActorBasis::DaemonAttestation));
    assert_eq!(accepted.actor_key, Some(format!("fp:{}", "a".repeat(64))));
    assert_eq!(accepted.attested_by.as_deref(), Some("node"));
    assert_eq!(accepted.subject_ref.as_deref(), Some("item"));
}

#[test]
fn a_journal_that_does_not_verify_refuses_the_read() {
    let root = tempfile::tempdir().unwrap();
    let node =
        NodeIdentity::create(&root.path().join(".ai/node/identity/private_key.pem")).unwrap();
    let journal = NodeJournal::for_app_root(root.path(), node.clone());
    journal
        .append(JournalDraft::new(JournalType::NodeStarted))
        .unwrap();
    let dir = ryeos_engine::roots::RuntimeRoot::new(root.path().to_path_buf())
        .state()
        .join("node-journal");
    let record = dir.join(format!("{:020}.json", 1));
    let mut bytes = std::fs::read(&record).unwrap();
    let flip = bytes.iter().position(|byte| *byte == b'7').unwrap_or(10);
    bytes[flip] ^= 1;
    std::fs::write(&record, bytes).unwrap();
    assert!(journal_rows(&journal).is_err());
}

#[test]
fn trust_events_become_rows_timed_by_the_node() {
    use ryeos_engine::trust::{TrustEvent, TrustEventKind};
    let event = |seq: u64, kind: TrustEventKind, predecessor: Option<&str>| TrustEvent {
        schema: "s".to_owned(),
        seq,
        previous: None,
        kind,
        signer: format!("k{seq}"),
        record_digest: format!("rd{seq}"),
        request_digest: format!("q{seq}"),
        operator: "op".to_owned(),
        predecessor: predecessor.map(str::to_owned),
    };
    let view = crate::trust_registry::RegistryView {
        records: Vec::new(),
        chain: vec![
            event(1, TrustEventKind::Pinned, None),
            event(2, TrustEventKind::Succeeded, Some("k1")),
        ],
        signed_at: vec![
            "2026-11-20T00:00:00Z".to_owned(),
            "2026-11-20T00:00:09Z".to_owned(),
        ],
        registry_digest: "rg".to_owned(),
    };
    let rows = trust_rows(&view).unwrap();
    assert_eq!(rows[0].row_type, "trust.pinned");
    assert_eq!(rows[0].at_basis, AtBasis::RecordedByNode);
    assert_eq!(rows[0].actor_basis, Some(ActorBasis::RecordedByNode));
    assert_eq!(rows[1].row_type, "trust.succeeded");
    assert_eq!(rows[1].subject_ref.as_deref(), Some("fp:k2"));
    assert_eq!(rows[1].detail["predecessor"], json!("fp:k1"));
    assert_eq!(rows[1].actor_key.as_deref(), Some("fp:op"));
    let page = page(
        rows.clone(),
        Vec::new(),
        Vec::new(),
        &JournalQuery::default(),
    )
    .unwrap();
    assert_eq!(
        page.keys().into_iter().collect::<Vec<_>>(),
        ["fp:k1", "fp:k2", "fp:op"]
    );
    assert!(!rows[0].detail.contains_key("predecessor"));

    let mut uneven = view;
    uneven.signed_at.pop();
    assert!(trust_rows(&uneven).is_err());
}
