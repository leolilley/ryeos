//! Vector files for behaviour a later slice implements. Each test loads its
//! file so the data stays wired and validated as well-formed; the assertions
//! against the implementation arrive with the behaviour.
//!
//! The history, touch, tail and boot-attachment vectors are replayed by
//! `history_vectors`, `seat_vectors`, `tail_vectors` and `admission_vectors`.

mod common;

use common::{load, name_of};

#[test]
#[ignore = "layout-preference persistence is not part of the protocol slice"]
fn layout_preference_vectors() {
    let cases = load("layout-preferences");
    assert!(!cases.is_empty(), "no layout-preferences vectors");
    for case in &cases {
        assert!(case.get("expected").is_some(), "{}", name_of(case));
    }
}
