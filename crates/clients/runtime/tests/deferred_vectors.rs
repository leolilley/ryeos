//! Vector files for behaviour the next slice implements. Each test loads its
//! file now so the data is wired and validated as well-formed; the assertions
//! against the implementation arrive with the behaviour.

mod common;

use common::{load, name_of};

macro_rules! deferred_file {
    ($test:ident, $file:literal) => {
        #[test]
        #[ignore = "implemented in the next slice"]
        fn $test() {
            let cases = load($file);
            assert!(!cases.is_empty(), "no {} vectors", $file);
            for case in &cases {
                assert!(case.get("expected").is_some(), "{}", name_of(case));
            }
        }
    };
}

deferred_file!(seat_history_stream_vectors, "seat-history");
deferred_file!(seat_touch_classification_vectors, "seat-touch");
deferred_file!(thread_tail_vectors, "thread-tail");
deferred_file!(layout_preference_vectors, "layout-preferences");
deferred_file!(boot_attachment_vectors, "boot-attachment");
