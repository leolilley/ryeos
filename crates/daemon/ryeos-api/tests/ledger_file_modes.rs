//! The served-bytes journals take their modes from code, never the umask.
//!
//! This binary holds one test because it sets the process-wide umask to 002,
//! the umask of the machine this node is developed on: a journal created
//! under it must still be owner-only (0600, in a 0700 directory it creates),
//! and an existing journal left group-writable is set back on its next write.

use ryeos_api::published::{ByteLedger, LEDGER_DIR_MODE, LEDGER_FILE_MODE};
use ryeos_api::routes::public_limits::{PublicRouteLimiter, ledger_path};

#[test]
fn journals_are_owner_only_under_a_permissive_umask() {
    let previous = lillux::set_process_umask_for_test(0o002);
    let root = tempfile::tempdir().expect("tempdir");
    let mode = |path: &std::path::Path| lillux::permission_mode_for_test(path).expect("mode");

    // The /published journal, in a state directory it creates.
    let state = root.path().join("state");
    let published = state.join("publication-served.ledger");
    let mut ledger = ByteLedger::load(published.clone(), 1_000);
    ledger
        .reserve(1_000, 10, 1_000_000)
        .expect("write the journal")
        .expect("within the cap");
    assert_eq!(mode(&state), LEDGER_DIR_MODE);
    assert_eq!(mode(&published), LEDGER_FILE_MODE);

    // A public route's journal, through the limiter.
    let mut policy =
        ryeos_node_runtime::node_policy::sections::public_routes::PublicRoutesPolicy::as_shipped();
    policy.routes.insert(
        "public/front".into(),
        ryeos_node_runtime::node_policy::sections::public_routes::PublicRouteLimits {
            daily_bytes: 1_000_000,
            concurrent_max: 1,
        },
    );
    let limiter = PublicRouteLimiter::build(&state, &policy, 1_000);
    let permit = limiter.admit("public/front").expect("a permit");
    let response =
        axum::response::IntoResponse::into_response((axum::http::StatusCode::OK, vec![b'x'; 4]));
    let sent = limiter.charge(
        "public/front",
        &axum::http::Method::GET,
        response,
        permit,
        1_000,
    );
    assert_eq!(sent.status(), axum::http::StatusCode::OK);
    assert_eq!(mode(&ledger_path(&state, "public/front")), LEDGER_FILE_MODE);

    // An existing journal left group-writable is owner-only after its next
    // write: the mode is set on every open, not only at create.
    lillux::atomic_write_with_mode(&published, b"", 0o664).expect("loosen the journal");
    assert_eq!(mode(&published), 0o664);
    let mut ledger = ByteLedger::load(published.clone(), 2_000);
    ledger
        .reserve(2_000, 10, 1_000_000)
        .expect("write the journal")
        .expect("within the cap");
    assert_eq!(mode(&published), LEDGER_FILE_MODE);

    lillux::set_process_umask_for_test(previous);
}
