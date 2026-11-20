use super::*;
use ryeos_engine::trust::{SignerRecord, SignerScope};

struct Fixture {
    project: tempfile::TempDir,
    app_root: tempfile::TempDir,
    node: NodeIdentity,
    authoring: SigningKey,
    operator: SigningKey,
    trust: TrustStore,
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn fixture() -> Fixture {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(project.path().join(".ai/directives")).unwrap();
    let app_root = tempfile::tempdir().unwrap();
    let node = NodeIdentity::create(&app_root.path().join("node-key.pem")).unwrap();
    let authoring = key(11);
    let operator = key(12);
    let trust = trust_with(
        &node,
        &[
            (&authoring, SignerRole::Authoring),
            (&operator, SignerRole::Operator),
        ],
    );
    Fixture {
        project,
        app_root,
        node,
        authoring,
        operator,
        trust,
    }
}

fn trust_with(node: &NodeIdentity, keys: &[(&SigningKey, SignerRole)]) -> TrustStore {
    let records = keys
        .iter()
        .map(|(key, role)| {
            SignerRecord::new(
                &key.verifying_key(),
                "test",
                *role,
                SignerScope::everywhere(),
                SignerStatus::Active,
            )
        })
        .collect();
    TrustStore::from_registry(*node.verifying_key(), records).unwrap()
}

impl Fixture {
    fn root(&self) -> &Path {
        self.project.path()
    }

    fn heads(&self) -> RevisionHeads {
        RevisionHeads::for_writer(self.app_root.path(), &self.node)
    }

    fn item_directory(&self) -> PathBuf {
        self.root().join(".ai/directives")
    }

    /// Write `content` as the item file and record it, signed by `signer`.
    fn change(&self, signer: &SigningKey, role: ActorRole, content: Option<&str>) -> Recorded {
        let item = ChainItem::parse("directive:greet").unwrap();
        let path = self.item_directory().join("greet.yaml");
        let before = std::fs::read(&path).ok();
        match content {
            Some(content) => std::fs::write(&path, content).unwrap(),
            None => std::fs::remove_file(&path).unwrap(),
        }
        append(
            self.root(),
            signer,
            role,
            Change {
                item: &item,
                file_ext: ".yaml",
                before: before.as_deref(),
                after: content,
                provenance: BTreeMap::from([("thread_id".to_owned(), "T-1".to_owned())]),
            },
        )
        .unwrap()
    }

    fn authored(&self, content: &str) -> Recorded {
        let recorded = self.change(&self.authoring, ActorRole::Authoring, Some(content));
        self.attest(&recorded);
        recorded
    }

    fn attest(&self, recorded: &Recorded) {
        attest(
            &self.heads(),
            self.root(),
            &ChainItem::parse("directive:greet").unwrap(),
            recorded,
        )
        .unwrap();
    }

    fn read(&self) -> ChainView {
        read_chain(
            self.root(),
            &self.item_directory(),
            &ChainItem::parse("directive:greet").unwrap(),
            &self.trust,
            &self.heads(),
        )
        .unwrap()
    }

    fn record_path(&self, seq: u64) -> PathBuf {
        self.root()
            .join(".ai/revisions/directive/greet")
            .join(record_name(seq))
    }
}

#[test]
fn author_upsert_and_remove_chain_by_digest() {
    let f = fixture();
    let first = f.authored("value: 1\n");
    let second = f.authored("value: 2\n");
    let third = {
        let recorded = f.change(&f.authoring, ActorRole::Authoring, None);
        f.attest(&recorded);
        recorded
    };
    assert_eq!((first.seq, second.seq, third.seq), (1, 2, 3));
    assert_eq!(first.previous_digest, None);
    assert_eq!(
        second.previous_digest.as_deref(),
        Some(first.digest.as_str())
    );
    assert_eq!(
        third.previous_digest.as_deref(),
        Some(second.digest.as_str())
    );

    let view = f.read();
    assert!(view.is_sound(), "{view:#?}");
    assert_eq!(view.head, HeadStatus::Attested { seq: 3 });
    assert_eq!(view.file, FileStatus::Removed);
    let bodies: Vec<&RevisionBody> = view
        .records
        .iter()
        .map(|record| record.body.as_ref().unwrap())
        .collect();
    assert_eq!(bodies[0].previous, genesis_previous("directive:greet"));
    assert_eq!(bodies[1].previous, first.digest);
    assert_eq!(bodies[2].previous, second.digest);
    assert_eq!(bodies[0].before_digest, None);
    assert_eq!(bodies[0].after_digest, Some(file_digest(b"value: 1\n")));
    assert_eq!(bodies[1].before_digest, bodies[0].after_digest);
    assert_eq!(bodies[1].after_digest, Some(file_digest(b"value: 2\n")));
    assert_eq!(bodies[2].before_digest, bodies[1].after_digest);
    assert!(bodies[2].is_tombstone());
    assert_eq!(bodies[2].content, None);
    assert_eq!(bodies[1].content.as_deref(), Some("value: 2\n"));
    assert_eq!(bodies[1].provenance["thread_id"], "T-1");
    assert!(
        view.records
            .iter()
            .all(|record| !record.unrecorded_change_before)
    );
    assert_eq!(
        view.records[0].verify.signer.as_deref(),
        Some(
            format!(
                "fp:{}",
                lillux::signature::compute_fingerprint(&f.authoring.verifying_key())
            )
            .as_str()
        )
    );
}

#[test]
fn a_tampered_record_is_named_and_breaks_the_link_after_it() {
    let f = fixture();
    f.authored("value: 1\n");
    f.authored("value: 2\n");
    f.authored("value: 3\n");
    let path = f.record_path(2);
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, original.replace("value: 2", "value: 9")).unwrap();

    let view = f.read();
    assert!(!view.is_sound());
    assert!(view.records[0].verify.is_verified());
    assert_eq!(
        view.records[1].verify.reason,
        Some(FailureCode::SignatureInvalid)
    );
    assert_eq!(
        view.records[2].verify.reason,
        Some(FailureCode::PreviousMismatch)
    );
}

#[test]
fn a_record_re_signed_by_another_key_is_not_pinned() {
    let f = fixture();
    f.authored("value: 1\n");
    let path = f.record_path(1);
    let content = std::fs::read_to_string(&path).unwrap();
    let body = lillux::signature::strip_signature_lines_with_envelope(&content, "#", None);
    let forged = lillux::signature::sign_content(&body, &key(99), "#", None);
    std::fs::write(&path, forged).unwrap();

    let view = f.read();
    assert_eq!(
        view.records[0].verify.reason,
        Some(FailureCode::SignerNotPinned)
    );
}

#[test]
fn a_publisher_key_may_not_sign_a_record() {
    let mut f = fixture();
    let publisher = key(13);
    f.trust = trust_with(
        &f.node,
        &[
            (&f.authoring, SignerRole::Authoring),
            (&f.operator, SignerRole::Operator),
            (&publisher, SignerRole::Publisher),
        ],
    );
    f.change(&publisher, ActorRole::Authoring, Some("value: 1\n"));
    let view = f.read();
    assert_eq!(
        view.records[0].verify.reason,
        Some(FailureCode::SignerRoleNotPermitted)
    );
}

#[test]
fn a_record_claiming_the_wrong_role_does_not_verify() {
    let f = fixture();
    f.change(&f.authoring, ActorRole::Operator, Some("value: 1\n"));
    let view = f.read();
    assert_eq!(
        view.records[0].verify.reason,
        Some(FailureCode::ActorMismatch)
    );
}

#[test]
fn deleting_the_newest_records_is_detected_by_the_node_head() {
    let f = fixture();
    f.authored("value: 1\n");
    f.authored("value: 2\n");
    f.authored("value: 3\n");
    std::fs::remove_file(f.record_path(3)).unwrap();
    std::fs::remove_file(f.record_path(2)).unwrap();

    let view = f.read();
    assert_eq!(
        view.head,
        HeadStatus::Truncated {
            head_seq: 3,
            tip_seq: 1
        }
    );
    let refusal = check_before_append(
        f.root(),
        &ChainItem::parse("directive:greet").unwrap(),
        &f.trust,
        &f.heads(),
    )
    .unwrap_err();
    let refusal = refusal
        .downcast_ref::<ChainRefusal>()
        .expect("typed refusal");
    assert_eq!(refusal.code(), "revision_chain_truncated");
}

#[test]
fn replacing_the_newest_record_is_detected_as_divergence() {
    let f = fixture();
    f.authored("value: 1\n");
    f.authored("value: 2\n");
    std::fs::remove_file(f.record_path(2)).unwrap();
    // A different, validly signed second record in its place.
    f.change(&f.authoring, ActorRole::Authoring, Some("value: forged\n"));

    let view = f.read();
    assert_eq!(view.head, HeadStatus::Diverged { head_seq: 2 });
    let refusal = check_before_append(
        f.root(),
        &ChainItem::parse("directive:greet").unwrap(),
        &f.trust,
        &f.heads(),
    )
    .unwrap_err();
    assert_eq!(
        refusal.downcast_ref::<ChainRefusal>().unwrap().code(),
        "revision_chain_diverged"
    );
}

#[test]
fn a_missing_middle_record_is_reported_and_leaves_the_next_unverifiable() {
    let f = fixture();
    f.authored("value: 1\n");
    f.authored("value: 2\n");
    f.authored("value: 3\n");
    std::fs::remove_file(f.record_path(2)).unwrap();
    let view = f.read();
    assert_eq!(view.records[1].verify.reason, Some(FailureCode::Missing));
    assert_eq!(
        view.records[2].verify.reason,
        Some(FailureCode::PreviousUnverifiable)
    );
}

#[test]
fn operator_records_made_without_the_node_read_as_behind_and_verify() {
    let f = fixture();
    f.authored("value: 1\n");
    let recorded = f.change(&f.operator, ActorRole::Operator, Some("value: 2\n"));
    assert_eq!(recorded.seq, 2);
    let view = f.read();
    assert!(view.is_sound(), "{view:#?}");
    assert_eq!(
        view.head,
        HeadStatus::Behind {
            head_seq: 1,
            tip_seq: 2
        }
    );
    // The node verifies the unattested tail before it extends the chain.
    check_before_append(
        f.root(),
        &ChainItem::parse("directive:greet").unwrap(),
        &f.trust,
        &f.heads(),
    )
    .unwrap();
    f.attest(&recorded);
    assert_eq!(f.read().head, HeadStatus::Attested { seq: 2 });
}

#[test]
fn a_tampered_unattested_tail_blocks_extension() {
    let f = fixture();
    f.authored("value: 1\n");
    f.change(&f.operator, ActorRole::Operator, Some("value: 2\n"));
    let path = f.record_path(2);
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, original.replace("value: 2", "value: 7")).unwrap();
    let refusal = check_before_append(
        f.root(),
        &ChainItem::parse("directive:greet").unwrap(),
        &f.trust,
        &f.heads(),
    )
    .unwrap_err();
    assert_eq!(
        refusal.downcast_ref::<ChainRefusal>().unwrap().code(),
        "revision_chain_broken"
    );
}

#[test]
fn a_hand_edit_reads_as_an_unrecorded_change_and_the_next_record_flags_the_gap() {
    let f = fixture();
    f.authored("value: 1\n");
    std::fs::write(f.item_directory().join("greet.yaml"), "value: hand\n").unwrap();
    assert_eq!(f.read().file, FileStatus::UnrecordedChange);

    // The operator signs the hand edit: the record's before digest is the
    // hand-edited file, not what the chain last wrote.
    let item = ChainItem::parse("directive:greet").unwrap();
    let before = std::fs::read(f.item_directory().join("greet.yaml")).unwrap();
    append(
        f.root(),
        &f.operator,
        ActorRole::Operator,
        Change {
            item: &item,
            file_ext: ".yaml",
            before: Some(&before),
            after: Some("value: hand-signed\n"),
            provenance: BTreeMap::new(),
        },
    )
    .unwrap();
    let view = f.read();
    assert!(view.records[1].unrecorded_change_before);
    assert!(!view.records[0].unrecorded_change_before);
}

#[test]
fn a_chain_with_a_gap_refuses_to_grow() {
    let f = fixture();
    f.authored("value: 1\n");
    f.authored("value: 2\n");
    f.authored("value: 3\n");
    std::fs::remove_file(f.record_path(2)).unwrap();
    let item = ChainItem::parse("directive:greet").unwrap();
    let error = append(
        f.root(),
        &f.authoring,
        ActorRole::Authoring,
        Change {
            item: &item,
            file_ext: ".yaml",
            before: None,
            after: Some("value: 4\n"),
            provenance: BTreeMap::new(),
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("not contiguous"), "{error:#}");
}

#[test]
fn a_forged_head_document_is_invalid_not_trusted() {
    let f = fixture();
    let other_node = NodeIdentity::create(&f.app_root.path().join("other-key.pem")).unwrap();
    let forged_heads = RevisionHeads::for_writer(f.app_root.path(), &other_node);
    forged_heads
        .record(f.root(), "directive:greet", 5, "deadbeef")
        .unwrap();
    f.change(&f.authoring, ActorRole::Authoring, Some("value: 1\n"));
    let view = f.read();
    assert!(
        matches!(view.head, HeadStatus::HeadInvalid { .. }),
        "{:?}",
        view.head
    );
    assert!(!view.is_sound());
}

#[test]
fn an_oversized_item_is_refused_before_anything_is_written() {
    let f = fixture();
    let item = ChainItem::parse("directive:big").unwrap();
    let big = "x".repeat(MAX_INLINE_CONTENT_BYTES + 1);
    let error = append(
        f.root(),
        &f.authoring,
        ActorRole::Authoring,
        Change {
            item: &item,
            file_ext: ".yaml",
            before: None,
            after: Some(&big),
            provenance: BTreeMap::new(),
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("carries at most"), "{error:#}");
    assert!(!f.root().join(".ai/revisions").exists());
}

#[test]
fn revision_records_have_no_chain_of_their_own() {
    assert!(ChainItem::parse("revision:directive/greet/00000001").is_err());
}
