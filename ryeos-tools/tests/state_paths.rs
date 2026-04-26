//! PR1a Task 1 test: verify canonical layout resolves consistently.

#[test]
fn canonical_layout_resolves_consistently() {
    let tmp = tempfile::tempdir().unwrap();
    let state_dir = tmp.path();

    // state_inner should return state_dir/.state
    let inner = ryeos_tools::state_inner(state_dir);
    assert_eq!(inner, state_dir.join(".state"));

    // Derived CAS paths should match the daemon's convention
    assert_eq!(inner.join("objects"), state_dir.join(".state/objects"));
    assert_eq!(inner.join("refs"), state_dir.join(".state/refs"));
    assert_eq!(inner.join("projection.sqlite3"), state_dir.join(".state/projection.sqlite3"));

    // status.rs already uses this convention (state_dir.join(".state/...")).
    // rebuild.rs, verify.rs, gc.rs now use state_inner() instead of
    // bare state_root.join("objects").
    let objects_via_helper = ryeos_tools::state_inner(state_dir).join("objects");
    assert_eq!(
        objects_via_helper,
        state_dir.join(".state").join("objects"),
        "state_inner must nest under .state/"
    );
}
