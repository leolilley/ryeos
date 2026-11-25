//! Launch lineage of an evaluator: its origin is read from the sealed capsule
//! and walked up the child links in the runtime store. Rows and links are real;
//! no process runs and no thread events are written, so the chain and thread
//! rows are inserted directly instead of through a launch.
use super::*;

fn capsule_with_origin(origin: Option<&str>) -> ryeos_state::objects::AdmittedLaunchCapsule {
    let key = lillux::crypto::SigningKey::from_bytes(&[7; 32]);
    let fingerprint = lillux::crypto::fingerprint(&key.verifying_key());
    let mut capsule = rooted_bundle_proof_fixture(&key, &fingerprint);
    capsule.evaluator_origin_chain = origin.map(str::to_owned);
    capsule
}

/// A hosted session chain `T-session`, a detached child `T-child` linked under
/// it in its own chain, a grandchild `T-grandchild` linked under the child, and
/// a person-launched chain `T-person` linked to nothing.
fn session_tree() -> TestStateStore {
    let store = test_store();
    {
        let g = store.lock().unwrap();
        for thread in ["T-session", "T-child", "T-grandchild", "T-person"] {
            g.runtime_db.insert_thread_runtime(thread, thread).unwrap();
        }
        g.runtime_db
            .record_child_link("T-session", "T-child", "dispatch")
            .unwrap();
        g.runtime_db
            .record_child_link("T-child", "T-grandchild", "dispatch")
            .unwrap();
    }
    store
}

#[test]
fn an_evaluator_launched_from_a_grandchild_of_the_session_reaches_the_session() {
    let store = session_tree();
    // session -> child -> grandchild launches an evaluator: whichever of the
    // chains in that ancestry the launch recorded as its origin.
    for origin in ["T-session", "T-child", "T-grandchild"] {
        assert!(
            store
                .sealed_origin_reaches_chain(&capsule_with_origin(Some(origin)), "T-session")
                .unwrap(),
            "origin {origin} descends from the session"
        );
    }
}

#[test]
fn a_person_launched_or_unrelated_evaluator_does_not_reach_the_session() {
    let store = session_tree();
    // A person's root launch seals no origin.
    assert!(
        !store
            .sealed_origin_reaches_chain(&capsule_with_origin(None), "T-session")
            .unwrap()
    );
    // An origin chain unrelated to the session.
    assert!(
        !store
            .sealed_origin_reaches_chain(&capsule_with_origin(Some("T-person")), "T-session")
            .unwrap()
    );
}

#[test]
fn a_cycle_in_the_links_ends_in_an_error_not_an_acceptance() {
    let store = test_store();
    {
        let g = store.lock().unwrap();
        for thread in ["T-a", "T-b"] {
            g.runtime_db.insert_thread_runtime(thread, thread).unwrap();
        }
        g.runtime_db
            .record_child_link("T-a", "T-b", "dispatch")
            .unwrap();
        g.runtime_db
            .record_child_link("T-b", "T-a", "dispatch")
            .unwrap();
    }
    assert!(
        store
            .sealed_origin_reaches_chain(&capsule_with_origin(Some("T-a")), "T-session")
            .is_err()
    );
}
