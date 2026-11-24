use std::path::PathBuf;

use lillux::crypto::SigningKey;
use ryeos_engine::trust::{
    SignerRecord, SignerRole, SignerScope, SignerStatus, TrustEvent, TrustEventKind, TrustStore,
    compute_fingerprint,
};
use ryeos_state::objects::{
    ChildProjectAuthorityPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
    LiveAccessAuthority, LiveFilesystemConfinement, LiveProjectAccess, LiveProjectIntent,
    PinnedProjectRealization, ThreadStatus,
};
use ryeos_state::principal_id::PrincipalId;
use ryeos_state::project_identity::{
    LogicalProject, LogicalProjectAbsence, ProjectIdentity, ProjectName,
};

use super::*;

/// A node registry with four keys:
/// - `alpha_root`, retired, succeeded by `alpha_now` (one principal, rotated);
/// - `beta`, active, another principal;
/// - `revoked`, revoked.
struct Keys {
    trust: TrustStore,
    alpha_root: String,
    alpha_now: String,
    beta: String,
    revoked: String,
    alpha: PrincipalId,
    beta_principal: PrincipalId,
}

fn record(key: &SigningKey, status: SignerStatus) -> SignerRecord {
    SignerRecord::new(
        &key.verifying_key(),
        "seat-authority-test",
        SignerRole::ProjectAuthor,
        SignerScope::authoring_default(),
        status,
    )
}

fn fp(key: &SigningKey) -> String {
    compute_fingerprint(&key.verifying_key())
}

fn solo(fingerprint: &str) -> PrincipalId {
    ryeos_engine::trust::SuccessionGraph::default()
        .ownership_of(fingerprint)
        .unwrap()
        .principal
}

fn keys() -> Keys {
    let node = SigningKey::from_bytes(&[1u8; 32]).verifying_key();
    let alpha_root = SigningKey::from_bytes(&[2u8; 32]);
    let alpha_now = SigningKey::from_bytes(&[3u8; 32]);
    let beta = SigningKey::from_bytes(&[4u8; 32]);
    let revoked = SigningKey::from_bytes(&[5u8; 32]);
    let succeeded = TrustEvent {
        schema: "x".into(),
        seq: 1,
        previous: None,
        kind: TrustEventKind::Succeeded,
        signer: fp(&alpha_now),
        record_digest: String::new(),
        request_digest: String::new(),
        operator: String::new(),
        predecessor: Some(fp(&alpha_root)),
    };
    let trust = TrustStore::from_registry(
        node,
        vec![
            record(&alpha_root, SignerStatus::Retired),
            record(&alpha_now, SignerStatus::Active),
            record(&beta, SignerStatus::Active),
            record(&revoked, SignerStatus::Revoked),
        ],
    )
    .unwrap()
    .with_succession_chain(std::slice::from_ref(&succeeded));
    Keys {
        alpha: solo(&fp(&alpha_root)),
        beta_principal: solo(&fp(&beta)),
        alpha_root: format!("fp:{}", fp(&alpha_root)),
        alpha_now: format!("fp:{}", fp(&alpha_now)),
        beta: format!("fp:{}", fp(&beta)),
        revoked: format!("fp:{}", fp(&revoked)),
        trust,
    }
}

fn project(owner: &PrincipalId, name: &str) -> ProjectIdentity {
    ProjectIdentity::new(owner.clone(), name.parse::<ProjectName>().unwrap())
}

fn subject(
    requested_by: &str,
    project_authority: ExecutionProjectAuthority,
) -> AuthoritativeThreadSubject {
    AuthoritativeThreadSubject {
        thread_id: "T-subject".to_string(),
        chain_root_id: "T-subject".to_string(),
        kind: "tool".to_string(),
        item_ref: "tool:test/subject".to_string(),
        executor_ref: "native:test".to_string(),
        launch_mode: "wait".to_string(),
        current_site_id: "site:host".to_string(),
        origin_site_id: "site:host".to_string(),
        upstream_thread_id: None,
        project_root: None,
        created_at: "created".to_string(),
        updated_at: "updated".to_string(),
        started_at: None,
        finished_at: None,
        status: ThreadStatus::Running,
        requested_by: Some(requested_by.to_string()),
        project_authority,
        admitted_launch_capsule_hash: None,
        result_project_snapshot_hash: None,
        result_workspace_output_capture_hash: None,
    }
}

fn live(root: &str, signer_project: LogicalProject) -> ExecutionProjectAuthority {
    ExecutionProjectAuthority::LiveProject {
        authority_id: "authority".to_string(),
        authored_project_identity: "project".to_string(),
        canonical_root: PathBuf::from(root),
        live_access: LiveAccessAuthority {
            access: LiveProjectAccess::ReadOnly,
            intent: LiveProjectIntent::Default,
            authorized_write_namespaces: Vec::new(),
            confinement: LiveFilesystemConfinement::UnconfinedHost,
        },
        environment: EnvironmentAuthority::None,
        capability_ceiling: Vec::new(),
        child_policy: ChildProjectAuthorityPolicy::Inherit,
        signer_project,
    }
}

fn pinned(signer_project: LogicalProject) -> ExecutionProjectAuthority {
    ExecutionProjectAuthority::PinnedGeneration {
        stable_project_identity: "project".to_string(),
        display_path: None,
        base_snapshot_hash: "a".repeat(64),
        snapshot_hash: "b".repeat(64),
        realization: PinnedProjectRealization::ReadOnly,
        workspace_outputs: None,
        environment: EnvironmentAuthority::None,
        capability_ceiling: Vec::new(),
        child_policy: ChildProjectAuthorityPolicy::Inherit,
        signer_project,
    }
}

fn absent() -> LogicalProject {
    LogicalProject::Absent(LogicalProjectAbsence::NotRegistered)
}

fn reader(key: &str, seat_project: SeatProjectState) -> ThreadReader {
    ThreadReader {
        key: key.to_string(),
        seat_project,
        project_authored_source: false,
        own_session: None,
    }
}

/// The hole this closes: B's seat bound to A's checkout path read every
/// thread A launched there, because the roots were equal. A path is no
/// longer authority, so B reads none of them, whatever its seat states.
#[test]
fn another_principal_on_the_same_checkout_reads_none_of_its_threads() {
    let k = keys();
    let alpha = project(&k.alpha, "alpha");
    let stated = subject(
        &k.alpha_now,
        live("/work/alpha", LogicalProject::Stated(alpha.clone())),
    );
    let unstated = subject(&k.alpha_now, live("/work/alpha", absent()));
    for seat in [
        SeatProjectState::Stated(alpha.clone()),
        SeatProjectState::Absent(SeatProjectAbsence::Unregistered),
        SeatProjectState::Projectless,
    ] {
        let beta = reader(&k.beta, seat.clone());
        assert!(!beta.admits(&k.trust, &stated), "{seat:?}");
        assert!(!beta.admits(&k.trust, &unstated), "{seat:?}");
    }
    // B's own launch in that checkout is still B's.
    let own = subject(&k.beta, live("/work/alpha", absent()));
    assert!(
        reader(
            &k.beta,
            SeatProjectState::Absent(SeatProjectAbsence::Unregistered)
        )
        .admits(&k.trust, &own)
    );
}

/// A stated thread is read by any key that speaks for its owner now, so a
/// rotated principal keeps its pre-rotation threads, and the retired key
/// reads nothing. The same holds for pinned generations.
#[test]
fn a_rotated_principal_keeps_its_threads_and_the_retired_key_loses_them() {
    let k = keys();
    let alpha = project(&k.alpha, "alpha");
    for authority in [
        live("/work/alpha", LogicalProject::Stated(alpha.clone())),
        pinned(LogicalProject::Stated(alpha.clone())),
    ] {
        let launched_before_rotation = subject(&k.alpha_root, authority);
        for seat in [
            SeatProjectState::Stated(alpha.clone()),
            SeatProjectState::Projectless,
        ] {
            assert!(
                reader(&k.alpha_now, seat.clone()).admits(&k.trust, &launched_before_rotation),
                "{seat:?}"
            );
            assert!(
                !reader(&k.alpha_root, seat.clone()).admits(&k.trust, &launched_before_rotation),
                "{seat:?}"
            );
        }
    }
}

/// A seat bound to a project is a view of that project: it reads no thread
/// of another project, even one its own principal launched, and not one of
/// the same owner under another name.
#[test]
fn a_project_seat_reads_only_its_own_project() {
    let k = keys();
    let alpha = project(&k.alpha, "alpha");
    let other = project(&k.alpha, "other");
    let thread = subject(
        &k.alpha_now,
        live("/work/other", LogicalProject::Stated(other.clone())),
    );
    assert!(!reader(&k.alpha_now, SeatProjectState::Stated(alpha)).admits(&k.trust, &thread));
    assert!(reader(&k.alpha_now, SeatProjectState::Stated(other)).admits(&k.trust, &thread));
    // A seat bound to no project may read across its principal's projects.
    assert!(reader(&k.alpha_now, SeatProjectState::Projectless).admits(&k.trust, &thread));
}

/// A seat bound to a checkout that states no identity now (renamed, or its
/// registration lapsed) still reads a stated thread its own key launched,
/// and no other key's, even one that speaks for the same owner.
#[test]
fn an_absent_seat_reads_stated_threads_its_key_launched() {
    let k = keys();
    let alpha = project(&k.alpha, "alpha");
    let thread = subject(
        &k.alpha_now,
        live("/work/alpha", LogicalProject::Stated(alpha.clone())),
    );
    for absence in [
        SeatProjectAbsence::CheckoutNameChanged,
        SeatProjectAbsence::Unregistered,
    ] {
        let seat = SeatProjectState::Absent(absence);
        assert!(
            reader(&k.alpha_now, seat.clone()).admits(&k.trust, &thread),
            "{absence:?}"
        );
        assert!(
            !reader(&k.beta, seat.clone()).admits(&k.trust, &thread),
            "{absence:?}"
        );
    }
    // Launched by the retired key: the launcher path needs an active key, and
    // the identity path is closed to a seat that is not this project's view.
    let before_rotation = subject(
        &k.alpha_root,
        live("/work/alpha", LogicalProject::Stated(alpha)),
    );
    let seat = SeatProjectState::Absent(SeatProjectAbsence::CheckoutNameChanged);
    assert!(!reader(&k.alpha_root, seat.clone()).admits(&k.trust, &before_rotation));
    assert!(!reader(&k.alpha_now, seat).admits(&k.trust, &before_rotation));
}

/// A thread with no identity, a projectless thread, and a pinned generation
/// that states none are read by the key that launched them, and only while
/// that key is active.
#[test]
fn unstated_threads_are_read_by_their_active_launcher_only() {
    let k = keys();
    for authority in [
        live("/work/folder", absent()),
        pinned(LogicalProject::Absent(
            LogicalProjectAbsence::ForeignHeadNotHeld,
        )),
        ExecutionProjectAuthority::PROJECTLESS,
    ] {
        let by_beta = subject(&k.beta, authority.clone());
        assert!(reader(&k.beta, SeatProjectState::Projectless).admits(&k.trust, &by_beta));
        assert!(!reader(&k.alpha_now, SeatProjectState::Projectless).admits(&k.trust, &by_beta));
        // Rotation does not carry an unstated thread: it names no owner.
        let by_root = subject(&k.alpha_root, authority.clone());
        assert!(!reader(&k.alpha_now, SeatProjectState::Projectless).admits(&k.trust, &by_root));
        assert!(!reader(&k.alpha_root, SeatProjectState::Projectless).admits(&k.trust, &by_root));
        let by_revoked = subject(&k.revoked, authority);
        assert!(!reader(&k.revoked, SeatProjectState::Projectless).admits(&k.trust, &by_revoked));
    }
}

/// A source a project-authored item declared reads live threads of its
/// seat's own stated project, and nothing else: no unstated, pinned or
/// projectless thread, even its own launcher's.
#[test]
fn a_project_authored_source_is_confined_to_its_seat_project() {
    let k = keys();
    let alpha = project(&k.alpha, "alpha");
    let source = ThreadReader {
        key: k.alpha_now.clone(),
        seat_project: SeatProjectState::Stated(alpha.clone()),
        project_authored_source: true,
        own_session: None,
    };
    assert!(source.admits(
        &k.trust,
        &subject(
            &k.alpha_now,
            live("/work/alpha", LogicalProject::Stated(alpha.clone()))
        )
    ));
    for authority in [
        live("/work/alpha", absent()),
        live(
            "/work/other",
            LogicalProject::Stated(project(&k.alpha, "other")),
        ),
        pinned(LogicalProject::Stated(alpha.clone())),
        ExecutionProjectAuthority::PROJECTLESS,
    ] {
        assert!(
            !source.admits(&k.trust, &subject(&k.alpha_now, authority.clone())),
            "{authority:?}"
        );
    }
    let projectless_source = ThreadReader {
        seat_project: SeatProjectState::Projectless,
        ..source
    };
    assert!(!projectless_source.admits(
        &k.trust,
        &subject(
            &k.alpha_now,
            live("/work/alpha", LogicalProject::Stated(alpha))
        )
    ));
}

/// A seat's own thread records the browser session it was opened through as
/// its launcher. A reader made through that session reads it; another session,
/// a read with no session, a near-miss spelling, a retired or revoked key and
/// a project-authored source do not.
#[test]
fn a_seat_reads_the_thread_its_own_session_launched() {
    let k = keys();
    let through = |key: &str, session: Option<&str>, seat: SeatProjectState| ThreadReader {
        own_session: session.map(str::to_string),
        ..reader(key, seat)
    };
    let alpha = project(&k.alpha, "alpha");
    for authority in [
        live("/work/alpha", absent()),
        pinned(absent()),
        ExecutionProjectAuthority::PROJECTLESS,
        live("/work/alpha", LogicalProject::Stated(alpha.clone())),
    ] {
        let seat_thread = subject("session:S-1", authority.clone());
        for seat in [
            SeatProjectState::Absent(SeatProjectAbsence::Unregistered),
            SeatProjectState::Projectless,
        ] {
            let stated = matches!(
                &authority,
                ExecutionProjectAuthority::LiveProject {
                    signer_project: LogicalProject::Stated(_),
                    ..
                }
            );
            // A stated thread under a projectless seat is read by speaks_for,
            // which a session launcher is not part of.
            if !(stated && seat == SeatProjectState::Projectless) {
                assert!(
                    through(&k.alpha_now, Some("S-1"), seat.clone()).admits(&k.trust, &seat_thread),
                    "{authority:?} {seat:?}"
                );
            }
            for (key, session) in [
                (&k.alpha_now, None),
                (&k.alpha_now, Some("S-2")),
                (&k.alpha_now, Some("")),
                (&k.alpha_now, Some("S-")),
                (&k.alpha_root, Some("S-1")),
                (&k.revoked, Some("S-1")),
            ] {
                if stated && seat == SeatProjectState::Projectless && key == &k.alpha_now {
                    continue;
                }
                assert!(
                    !through(key, session, seat.clone()).admits(&k.trust, &seat_thread),
                    "{authority:?} {seat:?} {key} {session:?}"
                );
            }
        }
        let source = ThreadReader {
            project_authored_source: true,
            ..through(
                &k.alpha_now,
                Some("S-1"),
                SeatProjectState::Absent(SeatProjectAbsence::Unregistered),
            )
        };
        assert!(!source.admits(&k.trust, &seat_thread), "{authority:?}");
    }
    // An empty recorded session never matches an empty or absent one.
    let empty = subject("session:", ExecutionProjectAuthority::PROJECTLESS);
    assert!(
        !through(&k.alpha_now, Some(""), SeatProjectState::Projectless).admits(&k.trust, &empty)
    );
}

/// Another principal's stated thread is never read through speaks_for.
#[test]
fn another_principal_never_speaks_for_the_owner() {
    let k = keys();
    let beta_project = project(&k.beta_principal, "beta");
    let thread = subject(
        &k.beta,
        live("/work/beta", LogicalProject::Stated(beta_project.clone())),
    );
    assert!(
        reader(&k.beta, SeatProjectState::Stated(beta_project.clone())).admits(&k.trust, &thread)
    );
    assert!(
        !reader(&k.alpha_now, SeatProjectState::Stated(beta_project)).admits(&k.trust, &thread)
    );
    assert!(!reader(&k.alpha_now, SeatProjectState::Projectless).admits(&k.trust, &thread));
}

/// The wire form the client reducer matches on.
#[test]
fn seat_project_state_serializes_as_state_project_absence() {
    let k = keys();
    let alpha = project(&k.alpha, "alpha");
    let cases = [
        (
            SeatProjectState::Stated(alpha.clone()),
            serde_json::json!({"state": "stated", "project": alpha.to_string()}),
        ),
        (
            SeatProjectState::Absent(SeatProjectAbsence::OwnerKeyNotActive),
            serde_json::json!({"state": "absent", "absence": "owner_key_not_active"}),
        ),
        (
            SeatProjectState::Absent(SeatProjectAbsence::CheckoutNameChanged),
            serde_json::json!({"state": "absent", "absence": "checkout_name_changed"}),
        ),
        (
            SeatProjectState::Projectless,
            serde_json::json!({"state": "projectless"}),
        ),
    ];
    for (state, wire) in cases {
        assert_eq!(serde_json::to_value(&state).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<SeatProjectState>(wire).unwrap(),
            state
        );
    }
    for absence in [
        SeatProjectAbsence::Unregistered,
        SeatProjectAbsence::OwnerKeyNotActive,
        SeatProjectAbsence::NoProjectName,
        SeatProjectAbsence::CheckoutNameChanged,
    ] {
        assert_eq!(
            serde_json::to_value(absence).unwrap(),
            serde_json::json!(absence.code())
        );
    }
    for malformed in [
        serde_json::json!({"state": "stated"}),
        serde_json::json!({"state": "absent", "project": alpha.to_string(), "absence": "unregistered"}),
        serde_json::json!({"state": "projectless", "absence": "unregistered"}),
        serde_json::json!({"state": "projectless", "extra": true}),
    ] {
        assert!(
            serde_json::from_value::<SeatProjectState>(malformed.clone()).is_err(),
            "{malformed}"
        );
    }
}

/// Each outcome of the live check reaches its own seat state, and a checkout
/// that now resolves to another identity than the binding recorded is not
/// that project any more.
#[test]
fn live_outcomes_map_to_seat_states() {
    let k = keys();
    let alpha = project(&k.alpha, "alpha");
    let renamed = project(&k.alpha, "renamed");
    assert_eq!(
        seat_state_from_outcome(&alpha, LiveSignerOutcome::Stated(alpha.clone())),
        SeatProjectState::Stated(alpha.clone())
    );
    assert_eq!(
        seat_state_from_outcome(&alpha, LiveSignerOutcome::Stated(renamed.clone())),
        SeatProjectState::Absent(SeatProjectAbsence::Unregistered)
    );
    for (outcome, absence) in [
        (
            LiveSignerOutcome::NoLiveWrite,
            SeatProjectAbsence::Unregistered,
        ),
        (
            LiveSignerOutcome::NotRegistered,
            SeatProjectAbsence::Unregistered,
        ),
        (
            LiveSignerOutcome::OwnerKeyNotActive,
            SeatProjectAbsence::OwnerKeyNotActive,
        ),
        (
            LiveSignerOutcome::NoProjectName,
            SeatProjectAbsence::NoProjectName,
        ),
        (
            LiveSignerOutcome::CheckoutNameChanged {
                checkout: PathBuf::from("/work/alpha"),
                registered: alpha.clone(),
                declared: "renamed".parse().unwrap(),
            },
            SeatProjectAbsence::CheckoutNameChanged,
        ),
    ] {
        assert_eq!(
            seat_state_from_outcome(&alpha, outcome),
            SeatProjectState::Absent(absence)
        );
    }
    assert_eq!(
        SeatProjectState::from_sealed(true, &LogicalProject::Stated(alpha.clone())),
        SeatProjectState::Stated(alpha)
    );
    assert_eq!(
        SeatProjectState::from_sealed(true, &absent()),
        SeatProjectState::Absent(SeatProjectAbsence::Unregistered)
    );
    assert_eq!(
        SeatProjectState::from_sealed(
            true,
            &LogicalProject::Absent(LogicalProjectAbsence::NoProjectName)
        ),
        SeatProjectState::Absent(SeatProjectAbsence::NoProjectName)
    );
    assert_eq!(
        SeatProjectState::from_sealed(
            false,
            &LogicalProject::Absent(LogicalProjectAbsence::Projectless)
        ),
        SeatProjectState::Projectless
    );
}
