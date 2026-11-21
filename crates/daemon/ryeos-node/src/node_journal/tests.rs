use super::*;

struct Fixture {
    root: tempfile::TempDir,
    node: NodeIdentity,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let node =
            NodeIdentity::create(&root.path().join(".ai/node/identity/private_key.pem")).unwrap();
        Self { root, node }
    }

    fn journal(&self) -> NodeJournal {
        NodeJournal::for_app_root(self.root.path(), self.node.clone())
    }

    fn dir(&self) -> PathBuf {
        self.journal().dir.clone()
    }

    fn path(&self, sequence: u64) -> PathBuf {
        self.dir().join(format!("{sequence:020}.json"))
    }
}

fn draft(tag: u64) -> JournalDraft {
    JournalDraft::new(JournalType::NodeStarted).with("pid", tag)
}

fn read_record(path: &Path) -> SignedJournalRecord {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn write_record(path: &Path, record: &SignedJournalRecord) {
    std::fs::write(path, serde_json::to_vec(record).unwrap()).unwrap();
}

fn started_facts(pid: u32) -> StartFacts<'static> {
    StartFacts {
        version: "1.2.3",
        revision: Some("abc123"),
        profile: Some("release"),
        pid,
        started_at: "2026-11-20T00:00:00Z",
        launch_route: LaunchRoute::Direct,
    }
}

fn identity_at(pid: u32, ticks: u64) -> lillux::process_control::ExactProcessIdentity {
    lillux::process_control::ExactProcessIdentity {
        boot_id: "00000000-dead-beef-0000-000000000000".into(),
        target_pid: pid,
        target_start_time_ticks: ticks,
        group_leader_pid: pid,
        group_leader_start_time_ticks: ticks,
        pid_namespace: "pid:[4026531836]".into(),
        group_basis: Default::default(),
    }
}

fn unclean(pid: u32) -> UncleanExit {
    UncleanExit {
        identity: identity_at(pid, 5),
        started_at: "2026-11-19T00:00:00Z".into(),
        launch_route: Some(LaunchRoute::Supervised),
    }
}

#[test]
fn an_empty_journal_has_the_genesis_head() {
    let fx = Fixture::new();
    let state = fx.journal().load().unwrap();
    assert_eq!(state.head, genesis_journal_head(fx.node.fingerprint()));
    assert_eq!(state.last_sequence, 0);
    assert!(fx.journal().read().unwrap().records.is_empty());
}

#[test]
fn records_chain_and_the_head_follows_the_last() {
    let fx = Fixture::new();
    let journal = fx.journal();
    let first = journal.append(draft(1)).unwrap();
    let second = journal.append(draft(2)).unwrap();
    assert_eq!(first.body.sequence, 1);
    assert_eq!(
        first.body.previous,
        genesis_journal_head(fx.node.fingerprint())
    );
    assert_eq!(second.body.previous, first.digest().unwrap());
    assert_eq!(first.body.node_fingerprint, fx.node.fingerprint());
    let state = journal.load().unwrap();
    assert_eq!(state.head, second.digest().unwrap());
    assert_eq!(state.retained, 2);
    let read = journal.read().unwrap();
    assert_eq!(read.records, vec![first, second]);
    assert_eq!(read.truncated_before, None);
}

#[test]
fn a_record_verifies_against_the_node_key_only() {
    let fx = Fixture::new();
    let record = fx.journal().append(draft(1)).unwrap();
    record.verify(fx.node.verifying_key()).unwrap();
    let other = NodeIdentity::create(&fx.root.path().join("other.pem")).unwrap();
    assert!(record.verify(other.verifying_key()).is_err());
}

#[test]
fn a_tampered_body_breaks_the_chain() {
    let fx = Fixture::new();
    let journal = fx.journal();
    for tag in 1..=3 {
        journal.append(draft(tag)).unwrap();
    }
    let mut record = read_record(&fx.path(2));
    record.body.detail.insert("pid".into(), Value::from(99));
    write_record(&fx.path(2), &record);
    assert!(journal.load().is_err());
}

#[test]
fn the_recorded_time_is_covered_by_the_signature() {
    let fx = Fixture::new();
    let journal = fx.journal();
    journal.append(draft(1)).unwrap();
    let mut record = read_record(&fx.path(1));
    assert!(record.body.recorded_at.ends_with('Z'));
    record.body.recorded_at = "2001-01-01T00:00:00Z".into();
    assert!(record.verify(fx.node.verifying_key()).is_err());
    write_record(&fx.path(1), &record);
    let error = format!("{:#}", journal.load().unwrap_err());
    assert!(error.contains("signature"), "{error}");
}

#[test]
fn a_removed_record_breaks_the_chain() {
    let fx = Fixture::new();
    let journal = fx.journal();
    for tag in 1..=3 {
        journal.append(draft(tag)).unwrap();
    }
    std::fs::remove_file(fx.path(2)).unwrap();
    assert!(journal.load().is_err());
}

#[test]
fn a_record_signed_by_another_key_is_refused() {
    let fx = Fixture::new();
    let journal = fx.journal();
    journal.append(draft(1)).unwrap();
    let other = NodeIdentity::create(&fx.root.path().join("other.pem")).unwrap();
    let record = read_record(&fx.path(1));
    let forged = SignedJournalRecord::sign(record.body, other.signing_key()).unwrap();
    write_record(&fx.path(1), &forged);
    assert!(journal.load().is_err());
}

#[test]
fn a_record_cannot_be_written_over() {
    let fx = Fixture::new();
    let journal = fx.journal();
    let stale = {
        journal.append(draft(1)).unwrap();
        JournalState {
            head: genesis_journal_head(fx.node.fingerprint()),
            last_sequence: 0,
            retained: 0,
            dropped_records: 0,
            oldest_recorded_at: None,
        }
    };
    assert!(journal.append_after(&stale, draft(2)).is_err());
}

#[test]
fn a_tampered_journal_refuses_a_write_as_an_integrity_error() {
    let fx = Fixture::new();
    let journal = fx.journal();
    journal.append(draft(1)).unwrap();
    let mut record = read_record(&fx.path(1));
    record.body.detail.insert("pid".into(), Value::from(7));
    write_record(&fx.path(1), &record);
    assert!(matches!(
        journal.append(draft(2)),
        Err(JournalError::Integrity(_))
    ));
    assert!(!fx.path(2).exists());
}

#[test]
fn a_write_that_cannot_land_is_a_write_error() {
    let fx = Fixture::new();
    let journal = fx.journal();
    // A regular file where the journal directory belongs.
    std::fs::create_dir_all(fx.dir().parent().unwrap()).unwrap();
    std::fs::write(fx.dir(), b"not a directory").unwrap();
    assert!(matches!(
        journal.append(draft(1)),
        Err(JournalError::Write(_))
    ));
}

#[test]
fn rotation_carries_the_tip_in_a_checkpoint() {
    let fx = Fixture::new();
    let journal = fx.journal();
    journal.create_dir().unwrap();
    let (_, mut chain_state) = journal
        .append_after(&journal.load().unwrap(), draft(0))
        .unwrap();
    let first_bytes = std::fs::read(fx.path(1)).unwrap();
    for tag in 1..=JOURNAL_ROTATE_AT as u64 {
        chain_state = journal.append_after(&chain_state, draft(tag)).unwrap().1;
    }
    let state = journal.load().unwrap();
    assert_eq!(state.head, chain_state.head);
    assert_eq!(state.last_sequence, JOURNAL_ROTATE_AT as u64 + 1);
    assert!(state.retained <= JOURNAL_KEEP + 1);
    assert!(state.dropped_records > 0);
    assert!(fx.dir().join("checkpoint.json").exists());
    let read = journal.read().unwrap();
    assert_eq!(read.dropped_records, state.dropped_records);
    assert_eq!(read.truncated_before, state.oldest_recorded_at);
    assert!(read.truncated_before.is_some());
    assert_eq!(read.records.len(), state.retained);
    // Appending continues the chain across the checkpoint.
    let next = journal.append(draft(7)).unwrap();
    assert_eq!(next.body.previous, state.head);
    journal.load().unwrap();
    // A death between the checkpoint and the removals leaves a covered
    // record behind; the journal still verifies and does not count it.
    std::fs::write(fx.path(1), first_bytes).unwrap();
    let after = journal.load().unwrap();
    assert_eq!(after.retained, state.retained + 1);
    assert_eq!(after.head, next.digest().unwrap());
}

#[test]
fn a_forged_checkpoint_is_refused() {
    let fx = Fixture::new();
    let journal = fx.journal();
    journal.create_dir().unwrap();
    let mut chain_state = journal.load().unwrap();
    for tag in 0..=JOURNAL_ROTATE_AT as u64 {
        chain_state = journal.append_after(&chain_state, draft(tag)).unwrap().1;
    }
    let path = fx.dir().join("checkpoint.json");
    let mut checkpoint: crate::offline_operation::contract::SignedCheckpoint =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    checkpoint.body.dropped_receipts = 1;
    std::fs::write(&path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
    assert!(journal.load().is_err());
}

#[test]
fn a_stray_file_does_not_stop_the_journal_but_a_non_file_record_does() {
    let fx = Fixture::new();
    let journal = fx.journal();
    journal.append(draft(1)).unwrap();
    std::fs::write(fx.dir().join("notes.txt"), b"hello").unwrap();
    journal.append(draft(2)).unwrap();
    std::fs::create_dir(fx.path(3)).unwrap();
    let error = format!("{:#}", journal.load().unwrap_err());
    assert!(error.contains("is not a regular file"), "{error}");
}

#[test]
fn a_record_is_bounded() {
    let fx = Fixture::new();
    let journal = fx.journal();
    let nested = JournalDraft::new(JournalType::NodeStarted).with("x", serde_json::json!({"a": 1}));
    assert!(matches!(
        journal.append(nested),
        Err(JournalError::Write(_))
    ));
    let long = JournalDraft::new(JournalType::NodeStarted)
        .with("x", "a".repeat(MAX_DETAIL_STRING_BYTES + 1));
    assert!(matches!(journal.append(long), Err(JournalError::Write(_))));
    let mut many = JournalDraft::new(JournalType::NodeStarted);
    for index in 0..=MAX_DETAIL_FIELDS {
        many = many.with(&format!("k{index}"), index as u64);
    }
    assert!(matches!(journal.append(many), Err(JournalError::Write(_))));
    let mut reference = draft(1);
    reference.subject_ref = Some("r".repeat(MAX_REF_BYTES + 1));
    assert!(matches!(
        journal.append(reference),
        Err(JournalError::Write(_))
    ));
    assert_eq!(journal.load().unwrap().last_sequence, 0);
}

#[test]
fn the_type_vocabulary_is_closed() {
    let fx = Fixture::new();
    let record = fx.journal().append(draft(1)).unwrap();
    let mut body = serde_json::to_value(&record.body).unwrap();
    serde_json::from_value::<JournalBody>(body.clone()).unwrap();
    body["type"] = "bundle.installed".into();
    assert!(serde_json::from_value::<JournalBody>(body).is_err());
    assert_eq!(JournalType::NodeStarted.as_str(), "node.started");
    assert_eq!(
        serde_json::to_value(JournalType::NodeExitInferredUnclean).unwrap(),
        "node.exit_inferred_unclean"
    );
}

#[test]
fn a_start_records_its_facts() {
    let fx = Fixture::new();
    let journal = fx.journal();
    journal.record_start(None, &started_facts(41)).unwrap();
    let records = journal.read().unwrap().records;
    assert_eq!(records.len(), 1);
    let body = &records[0].body;
    assert_eq!(body.record_type, JournalType::NodeStarted);
    assert_eq!(body.detail["version"], "1.2.3");
    assert_eq!(body.detail["revision"], "abc123");
    assert_eq!(body.detail["profile"], "release");
    assert_eq!(body.detail["pid"], 41);
}

#[test]
fn a_handled_exit_records_reason_and_cleanliness() {
    let fx = Fixture::new();
    let journal = fx.journal();
    let exit = |reason, error| ExitFacts {
        reason,
        error,
        pid: 41,
        started_at: "2026-11-20T00:00:00Z",
        launch_route: LaunchRoute::Direct,
    };
    journal.record_exit(&exit("signal", None)).unwrap();
    journal
        .record_exit(&exit("runtime_error", Some("boom")))
        .unwrap();
    journal.record_exit(&exit("startup_failed", None)).unwrap();
    let records = journal.read().unwrap().records;
    assert!(
        records
            .iter()
            .all(|record| record.body.record_type == JournalType::NodeExited)
    );
    assert_eq!(records[0].body.detail["reason"], "signal");
    assert_eq!(records[0].body.detail["clean"], true);
    assert_eq!(records[1].body.detail["clean"], false);
    assert_eq!(records[1].body.detail["error"], "boom");
    assert_eq!(records[2].body.detail["clean"], false);
}

#[test]
fn an_inferred_unclean_exit_is_written_once_before_the_start() {
    let fx = Fixture::new();
    let journal = fx.journal();
    let lost = InferredExit::Unclean(unclean(77));
    journal
        .record_start(Some(&lost), &started_facts(41))
        .unwrap();
    // The same lost run shown again (a second start reading the same marker)
    // adds a start but no second inference.
    journal
        .record_start(Some(&lost), &started_facts(42))
        .unwrap();
    let types: Vec<JournalType> = journal
        .read()
        .unwrap()
        .records
        .iter()
        .map(|record| record.body.record_type)
        .collect();
    assert_eq!(
        types,
        vec![
            JournalType::NodeExitInferredUnclean,
            JournalType::NodeStarted,
            JournalType::NodeStarted
        ]
    );
    let inferred = &journal.read().unwrap().records[0].body;
    assert_eq!(inferred.detail["pid"], 77);
    assert_eq!(inferred.detail["start_time_ticks"], 5);
    assert_eq!(inferred.detail["pid_namespace"], "pid:[4026531836]");
    assert_eq!(inferred.detail["started_at"], "2026-11-19T00:00:00Z");
    // A different lost run is a different record.
    journal
        .record_start(
            Some(&InferredExit::Unclean(unclean(78))),
            &started_facts(43),
        )
        .unwrap();
    assert_eq!(
        journal
            .read()
            .unwrap()
            .records
            .iter()
            .filter(|record| record.body.record_type == JournalType::NodeExitInferredUnclean)
            .count(),
        2
    );
}

#[test]
fn a_start_with_nothing_lost_writes_no_inference() {
    let fx = Fixture::new();
    let journal = fx.journal();
    journal.record_start(None, &started_facts(41)).unwrap();
    assert_eq!(journal.read().unwrap().records.len(), 1);
}

#[test]
fn a_start_over_a_tampered_journal_is_an_integrity_error() {
    let fx = Fixture::new();
    let journal = fx.journal();
    journal.record_start(None, &started_facts(41)).unwrap();
    let mut record = read_record(&fx.path(1));
    record.body.detail.insert("pid".into(), Value::from(1));
    write_record(&fx.path(1), &record);
    assert!(matches!(
        journal.record_start(None, &started_facts(42)),
        Err(JournalError::Integrity(_))
    ));
}

#[test]
fn the_same_pid_at_another_birth_is_a_different_lost_run() {
    let fx = Fixture::new();
    let journal = fx.journal();
    let first = InferredExit::Unclean(unclean(77));
    let mut reborn = unclean(77);
    reborn.identity.target_start_time_ticks = 6;
    let reborn = InferredExit::Unclean(reborn);
    for exit in [&first, &reborn, &first, &reborn] {
        journal
            .record_start(Some(exit), &started_facts(41))
            .unwrap();
    }
    let unclean_records = journal
        .read()
        .unwrap()
        .records
        .iter()
        .filter(|record| record.body.record_type == JournalType::NodeExitInferredUnclean)
        .count();
    assert_eq!(unclean_records, 2);
}

fn unknown(reason: &str, identity: Option<u32>) -> InferredExit {
    InferredExit::Unknown(UnknownExit {
        reason: reason.into(),
        identity: identity.map(|pid| identity_at(pid, 5)),
        started_at: identity.map(|_| "2026-11-19T00:00:00Z".into()),
        launch_route: identity.map(|_| LaunchRoute::Direct),
    })
}

#[test]
fn an_unknown_exit_is_recorded_once_and_is_never_unclean() {
    let fx = Fixture::new();
    let journal = fx.journal();
    let gap = unknown("indeterminate:probe refused", Some(77));
    journal
        .record_start(Some(&gap), &started_facts(41))
        .unwrap();
    journal
        .record_start(Some(&gap), &started_facts(42))
        .unwrap();
    let retired = unknown("retired_stale_running:absent", None);
    journal
        .record_start(Some(&retired), &started_facts(43))
        .unwrap();
    let records = journal.read().unwrap().records;
    let types: Vec<JournalType> = records.iter().map(|r| r.body.record_type).collect();
    assert_eq!(
        types,
        vec![
            JournalType::NodeExitUnknown,
            JournalType::NodeStarted,
            JournalType::NodeStarted,
            JournalType::NodeExitUnknown,
            JournalType::NodeStarted,
        ]
    );
    assert_eq!(
        records[0].body.detail["reason"],
        "indeterminate:probe refused"
    );
    assert_eq!(records[0].body.detail["pid"], 77);
    assert_eq!(
        records[3].body.detail["reason"],
        "retired_stale_running:absent"
    );
    assert!(!records[3].body.detail.contains_key("pid"));
    assert_eq!(JournalType::NodeExitUnknown.as_str(), "node.exit_unknown");
}
